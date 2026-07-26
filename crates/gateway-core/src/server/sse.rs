// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Shared decoding for OpenAI-compatible Server-Sent Events (SSE) streams.
//!
//! Three call sites consume an upstream chat-completion SSE stream: the
//! chat-UI driver (`gateway_runtime::openai_driver`'s `run_one_turn`), the `/v1`
//! byte-faithful relay (`rama_server::proxy::forward_streaming`), and the
//! `/v1` streaming tool loop (`…::drive_streaming_tool_loop_inner`). They
//! differ in *policy* — persist to the turn, relay verbatim, or
//! accumulate-and-hide tool calls — but share the *wire decoding*: framing on
//! the blank-line (`\n\n`) event boundary, the `data:` prefix plus `[DONE]`
//! sentinel, the trailing `usage` frame, and the delta field layout. That
//! decoding lives here so a wire-format fix (a new delta field, a framing
//! edge case, a provider quirk) lands in one place instead of drifting across
//! three hand-rolled copies.

use serde_json::Value;

/// Drain the next complete SSE event — up to and including its terminating
/// blank line (`\n\n`) — from `buf`, or `None` if no complete event is
/// buffered yet. Callers append raw network chunks to `buf` and loop on this
/// until it returns `None`, then await more bytes.
pub fn next_event(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let idx = buf.windows(2).position(|w| w == b"\n\n")?;
    Some(buf.drain(..idx + 2).collect())
}

/// The JSON payload carried by an SSE `data:` line, or `None` for any other
/// line (`event:`/`id:`/blank) and for the `[DONE]` sentinel.
pub fn data_payload(line: &str) -> Option<&str> {
    let payload = line.strip_prefix("data:").map(str::trim_start)?;
    (payload != "[DONE]").then_some(payload)
}

/// Token counts `(prompt, completion, total)` from a chat-completion chunk
/// that carries a non-null `usage` object, else `None`. OpenAI sends this on a
/// trailing choice-less frame; some backends ride it on the final content
/// chunk — the caller decides what to do with any accompanying choice.
pub fn usage_tokens(v: &Value) -> Option<(Option<i64>, Option<i64>, Option<i64>)> {
    v.get("usage")
        .is_some_and(|u| !u.is_null())
        .then(|| crate::server::db::usage::usage_from_value(v))
}

/// Read-only view over a streamed chat-completion chunk's first-choice delta
/// (`choices[0].delta`). Cheap to construct — just wraps the borrowed chunk.
pub struct ChatDelta<'a>(&'a Value);

impl<'a> ChatDelta<'a> {
    pub fn new(chunk: &'a Value) -> Self {
        Self(chunk)
    }

    /// `choices[0].delta.content`, if present and a string.
    pub fn content(&self) -> Option<&'a str> {
        self.0
            .pointer("/choices/0/delta/content")
            .and_then(|c| c.as_str())
    }

    /// `choices[0].delta.reasoning_content`, falling back to `.reasoning`
    /// (vLLM's `--reasoning-parser` adapters emit one name or the other).
    pub fn reasoning(&self) -> Option<&'a str> {
        self.0
            .pointer("/choices/0/delta/reasoning_content")
            .and_then(|c| c.as_str())
            .or_else(|| {
                self.0
                    .pointer("/choices/0/delta/reasoning")
                    .and_then(|c| c.as_str())
            })
    }

    /// `choices[0].delta.tool_calls`, if present and an array.
    pub fn tool_calls(&self) -> Option<&'a Vec<Value>> {
        self.0
            .pointer("/choices/0/delta/tool_calls")
            .and_then(|t| t.as_array())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn next_event_splits_on_blank_line_and_keeps_remainder() {
        let mut buf = b"data: a\n\ndata: b\n\ndata: par".to_vec();
        assert_eq!(next_event(&mut buf).unwrap(), b"data: a\n\n");
        assert_eq!(next_event(&mut buf).unwrap(), b"data: b\n\n");
        // Partial trailing event stays buffered.
        assert!(next_event(&mut buf).is_none());
        assert_eq!(buf, b"data: par");
    }

    #[test]
    fn data_payload_strips_prefix_and_skips_done() {
        assert_eq!(data_payload("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(data_payload("data:{\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(data_payload("data: [DONE]"), None);
        assert_eq!(data_payload("event: message"), None);
        assert_eq!(data_payload(""), None);
    }

    #[test]
    fn chat_delta_reads_fields_with_reasoning_fallback() {
        let v = json!({"choices":[{"delta":{"content":"hi","reasoning":"why","tool_calls":[{"index":0}]}}]});
        let d = ChatDelta::new(&v);
        assert_eq!(d.content(), Some("hi"));
        assert_eq!(d.reasoning(), Some("why"));
        assert_eq!(d.tool_calls().map(|t| t.len()), Some(1));

        // reasoning_content takes precedence over reasoning.
        let v2 = json!({"choices":[{"delta":{"reasoning_content":"rc","reasoning":"r"}}]});
        assert_eq!(ChatDelta::new(&v2).reasoning(), Some("rc"));
    }

    #[test]
    fn usage_tokens_only_fires_on_non_null_usage() {
        assert!(usage_tokens(&json!({"choices":[]})).is_none());
        assert!(usage_tokens(&json!({"usage": null})).is_none());
        assert!(
            usage_tokens(
                &json!({"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}})
            )
            .is_some()
        );
    }
}
