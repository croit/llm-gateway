// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Streamed OpenAI chunks → the Anthropic SSE event sequence.
//!
//! The two wire formats disagree about what a stream *is*. OpenAI sends a
//! flat run of `chat.completion.chunk` objects whose `delta` may carry
//! anything, terminated by `[DONE]`. Anthropic sends a *structured* sequence
//! with named events and explicit content-block framing:
//!
//! ```text
//! message_start
//!   content_block_start (index 0, thinking)  content_block_delta …  content_block_stop
//!   content_block_start (index 1, text)      content_block_delta …  content_block_stop
//!   content_block_start (index 2, tool_use)  content_block_delta …  content_block_stop
//! message_delta   (stop_reason + usage)
//! message_stop
//! ```
//!
//! [`StreamEncoder`] is the state machine that bridges them: it opens and
//! closes blocks as the delta channel changes (reasoning → text → tool calls),
//! keeps the running block index, and remembers the stop reason and token
//! counts for the closing `message_delta`.
//!
//! ## Tool calls arrive whole, not incrementally
//!
//! The gateway's streaming tool loop withholds tool-call deltas: it has to
//! see the complete call before it can decide whether the *gateway* runs it
//! or the client does. Client-owned calls are handed back at the end of the
//! round, so [`StreamEncoder::tool_use`] takes the finished arguments and
//! emits a block with a single `input_json_delta`. The client sees one
//! well-formed tool_use block; it just doesn't watch the arguments type
//! themselves.

use serde_json::{Value, json};

use crate::server::sse::ChatDelta;

use super::response::{THINKING_SIGNATURE, message_skeleton, usage_object};

/// Which kind of content block is currently open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    Thinking,
    Text,
}

/// Encodes one Anthropic message as SSE. Feed it upstream chunks; it hands
/// back the frames to write to the client, in order.
#[derive(Debug)]
pub struct StreamEncoder {
    message_id: String,
    model: String,
    started: bool,
    finished: bool,
    open: Option<OpenBlock>,
    next_index: usize,
    input_tokens: i64,
    output_tokens: i64,
    stop_reason: Option<String>,
}

impl StreamEncoder {
    pub fn new(message_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            message_id: message_id.into(),
            model: model.into(),
            started: false,
            finished: false,
            open: None,
            next_index: 0,
            input_tokens: 0,
            output_tokens: 0,
            stop_reason: None,
        }
    }

    /// Emit `message_start`, if it hasn't been emitted yet.
    ///
    /// Called eagerly by the handler rather than lazily on the first chunk:
    /// the client's stream watchdog counts bytes from the moment the response
    /// headers land, and a keep-alive `ping` before `message_start` would be
    /// the first thing it ever saw.
    ///
    /// Its `usage.input_tokens` is therefore `0`: nothing has asked the
    /// backend anything yet, and the prompt size only comes back with the
    /// round's usage frame. The real figure rides the closing `message_delta`
    /// instead. A client that wants it up front can ask
    /// `/v1/messages/count_tokens`.
    pub fn start(&mut self) -> Vec<String> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![frame(
            "message_start",
            &json!({
                "type": "message_start",
                "message": message_skeleton(&self.message_id, &self.model, self.input_tokens),
            }),
        )]
    }

    /// Record one upstream round's final token counts.
    ///
    /// The two halves accumulate differently, on purpose. `output_tokens` sums
    /// — every one of them was generated, and a client's cost display should
    /// show all of them. `input_tokens` is **replaced**: each round of a
    /// gateway-tool loop resends the whole conversation, so round two's prompt
    /// already contains round one's. Summing would report a context several
    /// times its real size to a client that reads this figure to decide when
    /// to compact. A round that reported nothing (`0`) leaves the last known
    /// prompt size standing rather than erasing it.
    ///
    /// Called once per round, with the same numbers the usage row is billed
    /// on — not once per chunk. A backend that repeats cumulative usage on
    /// several chunks of one round would otherwise multiply the totals the
    /// client sees while billing recorded them once.
    pub fn absorb_round_usage(&mut self, input_tokens: i64, output_tokens: i64) {
        if input_tokens > 0 {
            self.input_tokens = input_tokens;
        }
        self.output_tokens += output_tokens;
    }

    /// Consume one upstream `chat.completion.chunk`.
    ///
    /// Tool-call deltas are ignored here by design — see the module docs;
    /// [`Self::tool_use`] is how a tool call reaches the client. Token counts
    /// arrive through [`Self::absorb_round_usage`], not from the chunk.
    pub fn chunk(&mut self, chunk: &Value) -> Vec<String> {
        let mut out = self.start();
        if self.finished {
            return out;
        }

        let delta = ChatDelta::new(chunk);
        if let Some(reasoning) = delta.reasoning().filter(|s| !s.is_empty()) {
            out.extend(self.ensure_block(OpenBlock::Thinking));
            out.push(frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": self.current_index(),
                    "delta": {"type": "thinking_delta", "thinking": reasoning},
                }),
            ));
        }
        if let Some(text) = delta.content().filter(|s| !s.is_empty()) {
            out.extend(self.ensure_block(OpenBlock::Text));
            out.push(frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": self.current_index(),
                    "delta": {"type": "text_delta", "text": text},
                }),
            ));
        }
        if let Some(reason) = chunk
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            // Recorded, not acted on: the loop may run another upstream round
            // behind this same client-visible message.
            self.stop_reason = Some(super::stop_reason_for(Some(reason)).to_string());
        }
        out
    }

    /// Emit a complete `tool_use` block. `arguments` is the raw
    /// `function.arguments` JSON string; it is re-emitted as one
    /// `input_json_delta` so a client that accumulates `partial_json`
    /// fragments (every Anthropic SDK does) rebuilds the same object.
    pub fn tool_use(&mut self, id: &str, name: &str, arguments: &str) -> Vec<String> {
        let mut out = self.start();
        out.extend(self.close_open());
        let index = self.next_index;
        self.next_index += 1;
        out.push(frame(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
            }),
        ));
        // Normalise so the client never has to parse `""` or a fragment: an
        // empty/garbage argument string becomes `{}`, matching what the
        // gateway itself replays upstream.
        let partial = crate::server::tool_args::normalize_tool_arguments(arguments);
        out.push(frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": partial},
            }),
        ));
        out.push(frame(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ));
        self.stop_reason = Some("tool_use".to_string());
        out
    }

    /// Close the message: any open block, then `message_delta` + `message_stop`.
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = self.start();
        if self.finished {
            return out;
        }
        out.extend(self.close_open());
        self.finished = true;
        let stop = self
            .stop_reason
            .clone()
            .unwrap_or_else(|| "end_turn".into());
        out.push(frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop, "stop_sequence": Value::Null},
                "usage": usage_object(self.input_tokens, self.output_tokens),
            }),
        ));
        out.push(frame("message_stop", &json!({"type": "message_stop"})));
        out
    }

    /// A mid-stream failure. The client sees an `error` event and the stream
    /// ends there — no `message_stop`, because the message never completed.
    pub fn error(&mut self, error_body: &Value) -> Vec<String> {
        let mut out = self.start();
        if self.finished {
            return out;
        }
        self.finished = true;
        out.push(frame("error", error_body));
        out
    }

    /// Whether the message has been closed off (by [`Self::finish`] or
    /// [`Self::error`]).
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// A keep-alive frame. Emitted on a timer while the gateway is busy —
    /// running a tool, waiting on a slow first token — because the client
    /// aborts a stream that relays no bytes for long enough, and a
    /// self-hosted backend sends nothing during those gaps.
    pub fn ping() -> String {
        frame("ping", &json!({"type": "ping"}))
    }

    /// Open `kind` if a different (or no) block is open. No-op when the same
    /// kind is already streaming.
    fn ensure_block(&mut self, kind: OpenBlock) -> Vec<String> {
        if self.open == Some(kind) {
            return Vec::new();
        }
        let mut out = self.close_open();
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some(kind);
        let block = match kind {
            OpenBlock::Thinking => json!({"type": "thinking", "thinking": ""}),
            OpenBlock::Text => json!({"type": "text", "text": ""}),
        };
        out.push(frame(
            "content_block_start",
            &json!({"type": "content_block_start", "index": index, "content_block": block}),
        ));
        out
    }

    /// Close the currently open block, if any.
    fn close_open(&mut self) -> Vec<String> {
        let Some(kind) = self.open.take() else {
            return Vec::new();
        };
        let index = self.next_index - 1;
        let mut out = Vec::new();
        // Anthropic signs thinking blocks before closing them. Ours is a
        // marker rather than a signature (see `response::THINKING_SIGNATURE`),
        // but the block shape stays the one clients expect.
        if kind == OpenBlock::Thinking {
            out.push(frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "signature_delta", "signature": THINKING_SIGNATURE},
                }),
            ));
        }
        out.push(frame(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ));
        out
    }

    /// Index of the block currently open (only called while one is).
    fn current_index(&self) -> usize {
        self.next_index.saturating_sub(1)
    }
}

/// One SSE frame: Anthropic names every event, so both the `event:` and
/// `data:` lines are required.
fn frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse the frames back into `(event name, payload)` pairs so the tests
    /// assert on the sequence rather than on string formatting.
    fn events(frames: &[String]) -> Vec<(String, Value)> {
        frames
            .iter()
            .map(|f| {
                let mut name = String::new();
                let mut data = Value::Null;
                for line in f.lines() {
                    if let Some(rest) = line.strip_prefix("event: ") {
                        name = rest.to_string();
                    } else if let Some(rest) = line.strip_prefix("data: ") {
                        data = serde_json::from_str(rest).expect("frame data is JSON");
                    }
                }
                (name, data)
            })
            .collect()
    }

    fn text_chunk(text: &str) -> Value {
        json!({"choices": [{"index": 0, "delta": {"content": text}}]})
    }

    #[test]
    fn a_text_only_stream_produces_the_canonical_sequence() {
        let mut enc = StreamEncoder::new("msg_1", "claude-sonnet-4-6");
        let mut frames = enc.start();
        frames.extend(enc.chunk(&text_chunk("Hel")));
        frames.extend(enc.chunk(&text_chunk("lo")));
        frames.extend(enc.chunk(&json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})));
        frames.extend(enc.finish());

        let ev = events(&frames);
        let names: Vec<&str> = ev.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(ev[0].1["message"]["id"], "msg_1");
        assert_eq!(ev[0].1["message"]["model"], "claude-sonnet-4-6");
        assert_eq!(ev[1].1["index"], 0);
        assert_eq!(ev[1].1["content_block"]["type"], "text");
        assert_eq!(ev[2].1["delta"]["type"], "text_delta");
        assert_eq!(ev[2].1["delta"]["text"], "Hel");
        assert_eq!(ev[5].1["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn reasoning_opens_a_thinking_block_that_closes_when_text_starts() {
        let mut enc = StreamEncoder::new("m", "x");
        let mut frames = enc.chunk(&json!({"choices": [{"delta": {"reasoning_content": "hm"}}]}));
        frames.extend(enc.chunk(&text_chunk("answer")));
        frames.extend(enc.finish());

        let ev = events(&frames);
        let names: Vec<&str> = ev.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start", // thinking, index 0
                "content_block_delta", // thinking_delta
                "content_block_delta", // signature_delta
                "content_block_stop",
                "content_block_start", // text, index 1
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(ev[1].1["content_block"]["type"], "thinking");
        assert_eq!(ev[2].1["delta"]["type"], "thinking_delta");
        assert_eq!(ev[3].1["delta"]["type"], "signature_delta");
        assert_eq!(ev[4].1["index"], 0);
        assert_eq!(ev[5].1["index"], 1);
        assert_eq!(ev[5].1["content_block"]["type"], "text");
    }

    #[test]
    fn a_tool_call_closes_the_text_block_and_stops_with_tool_use() {
        let mut enc = StreamEncoder::new("m", "x");
        let mut frames = enc.chunk(&text_chunk("let me look"));
        frames.extend(enc.tool_use("toolu_1", "Read", r#"{"path":"a.rs"}"#));
        frames.extend(enc.finish());

        let ev = events(&frames);
        let names: Vec<&str> = ev.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start", // text
                "content_block_delta",
                "content_block_stop",
                "content_block_start", // tool_use
                "content_block_delta", // input_json_delta
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(ev[4].1["index"], 1);
        assert_eq!(ev[4].1["content_block"]["type"], "tool_use");
        assert_eq!(ev[4].1["content_block"]["id"], "toolu_1");
        assert_eq!(ev[4].1["content_block"]["name"], "Read");
        assert_eq!(ev[5].1["delta"]["type"], "input_json_delta");
        assert_eq!(ev[5].1["delta"]["partial_json"], r#"{"path":"a.rs"}"#);
        assert_eq!(ev[7].1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn two_tool_calls_get_consecutive_indices() {
        let mut enc = StreamEncoder::new("m", "x");
        let mut frames = enc.tool_use("a", "Read", "{}");
        frames.extend(enc.tool_use("b", "Write", "{}"));
        let ev = events(&frames);
        let starts: Vec<i64> = ev
            .iter()
            .filter(|(n, _)| n == "content_block_start")
            .map(|(_, d)| d["index"].as_i64().unwrap())
            .collect();
        assert_eq!(starts, vec![0, 1]);
    }

    #[test]
    fn empty_tool_arguments_stream_as_an_empty_object() {
        let mut enc = StreamEncoder::new("m", "x");
        let frames = enc.tool_use("a", "Now", "");
        let ev = events(&frames);
        assert_eq!(ev[2].1["delta"]["partial_json"], "{}");
    }

    /// Output sums across the tool loop's rounds; the prompt does not, because
    /// each round resends the previous one's messages. A client reading
    /// `input_tokens` as "how full is my context" must not be told 30 when the
    /// conversation it sent was 20.
    #[test]
    fn the_closing_delta_sums_output_but_reports_the_last_prompt() {
        let mut enc = StreamEncoder::new("m", "x");
        enc.absorb_round_usage(10, 4);
        enc.absorb_round_usage(20, 6);
        let frames = enc.finish();
        let ev = events(&frames);
        let (_, event) = ev.iter().find(|(n, _)| n == "message_delta").unwrap();
        assert_eq!(event["usage"]["input_tokens"], 20);
        assert_eq!(event["usage"]["output_tokens"], 10);
    }

    /// A round whose backend reported no usage must not erase what the last
    /// one told us.
    #[test]
    fn a_round_without_usage_leaves_the_known_prompt_size_standing() {
        let mut enc = StreamEncoder::new("m", "x");
        enc.absorb_round_usage(120, 5);
        enc.absorb_round_usage(0, 7);
        let ev = events(&enc.finish());
        let (_, event) = ev.iter().find(|(n, _)| n == "message_delta").unwrap();
        assert_eq!(event["usage"]["input_tokens"], 120);
        assert_eq!(event["usage"]["output_tokens"], 12);
    }

    #[test]
    fn finish_is_idempotent_and_start_is_emitted_once() {
        let mut enc = StreamEncoder::new("m", "x");
        let first = enc.start();
        assert_eq!(first.len(), 1);
        assert!(enc.start().is_empty());
        let closing = enc.finish();
        assert_eq!(closing.len(), 2);
        assert!(enc.finish().is_empty());
        assert!(enc.is_finished());
    }

    #[test]
    fn an_error_ends_the_stream_without_a_message_stop() {
        let mut enc = StreamEncoder::new("m", "x");
        let _ = enc.chunk(&text_chunk("partial"));
        let frames =
            enc.error(&json!({"type": "error", "error": {"type": "api_error", "message": "boom"}}));
        let ev = events(&frames);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].0, "error");
        assert_eq!(ev[0].1["error"]["message"], "boom");
        assert!(enc.is_finished());
        // Nothing follows an error, not even a late finish().
        assert!(enc.finish().is_empty());
    }

    #[test]
    fn a_truncated_turn_reports_max_tokens() {
        let mut enc = StreamEncoder::new("m", "x");
        let mut frames = enc.chunk(&text_chunk("half"));
        frames.extend(enc.chunk(&json!({"choices": [{"delta": {}, "finish_reason": "length"}]})));
        frames.extend(enc.finish());
        let ev = events(&frames);
        let (_, event) = ev.iter().find(|(n, _)| n == "message_delta").unwrap();
        assert_eq!(event["delta"]["stop_reason"], "max_tokens");
    }

    #[test]
    fn a_ping_is_a_complete_named_frame() {
        let ping = StreamEncoder::ping();
        assert_eq!(ping, "event: ping\ndata: {\"type\":\"ping\"}\n\n");
    }
}
