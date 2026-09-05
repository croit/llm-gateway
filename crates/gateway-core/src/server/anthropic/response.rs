// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Buffered OpenAI chat completion → Anthropic `Message`.
//!
//! The mapping is the mirror of [`super::request`]: the assistant's text
//! becomes a `text` block, `reasoning_content` becomes a `thinking` block,
//! and each `tool_calls[]` entry becomes a `tool_use` block whose `input` is
//! the *parsed* arguments object (Anthropic carries structured input where
//! OpenAI carries a JSON string).

use serde_json::{Value, json};

use crate::server::tool_args::tool_arguments_object;

/// The `signature` we stamp on synthesised `thinking` blocks.
///
/// Anthropic signs thinking blocks so a replayed block can be verified.
/// We have nothing to sign with — the reasoning came from a local backend —
/// so we mark ours plainly instead of forging something that looks real.
/// Nothing verifies it: [`super::request`] drops every thinking block on the
/// way back in, so the round trip is closed regardless of the value.
pub const THINKING_SIGNATURE: &str = "gateway";

/// Translate a buffered completion. `requested_model` is echoed back as the
/// response's `model` — the client asked for an alias and should see the
/// name it used, exactly as `/v1/chat/completions` reports it via
/// `x-gateway-resolved-model` rather than by rewriting the body.
pub fn from_openai(completion: &Value, requested_model: &str) -> Value {
    let message = completion.pointer("/choices/0/message");
    let finish = completion
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str);

    let mut content: Vec<Value> = Vec::new();
    if let Some(message) = message {
        if let Some(reasoning) = reasoning_text(message)
            && !reasoning.is_empty()
        {
            content.push(thinking_block(reasoning));
        }
        if let Some(text) = message.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            content.push(json!({"type": "text", "text": text}));
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                content.push(tool_use_block(call));
            }
        }
    }

    // Anthropic's own API never returns an empty content array; a client that
    // indexes `content[0]` shouldn't have to special-case a backend that
    // produced nothing.
    if content.is_empty() {
        content.push(json!({"type": "text", "text": ""}));
    }

    // The same reader billing and the SSE decoder use, so the three can't
    // disagree about a backend's spelling of the usage fields.
    let (input_tokens, output_tokens, _total) =
        crate::server::db::usage::usage_from_value(completion);

    json!({
        "id": super::message_id(completion.get("id").and_then(Value::as_str)),
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": content,
        "stop_reason": super::stop_reason_for(finish),
        "stop_sequence": Value::Null,
        "usage": usage_object(input_tokens.unwrap_or(0), output_tokens.unwrap_or(0)),
    })
}

/// A `thinking` content block carrying `text`.
pub fn thinking_block(text: &str) -> Value {
    json!({"type": "thinking", "thinking": text, "signature": THINKING_SIGNATURE})
}

/// One OpenAI `tool_calls[]` entry → a `tool_use` block.
fn tool_use_block(call: &Value) -> Value {
    let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
    let name = call
        .pointer("/function/name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let raw = call
        .pointer("/function/arguments")
        .and_then(Value::as_str)
        .unwrap_or("");
    json!({"type": "tool_use", "id": id, "name": name, "input": tool_arguments_object(raw)})
}

/// `reasoning_content`, falling back to `reasoning` — the two spellings vLLM's
/// reasoning-parser adapters use, matching [`crate::server::sse::ChatDelta`].
fn reasoning_text(message: &Value) -> Option<&str> {
    message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .or_else(|| message.get("reasoning").and_then(Value::as_str))
}

/// The Anthropic `usage` object.
///
/// The cache counters are reported as zero rather than omitted: clients read
/// them to show cache activity, and a missing field reads as "unknown"
/// where zero reads as "no cache hit", which is the truth. Our upstreams do
/// prefix caching internally but report nothing about it on the wire.
pub fn usage_object(input_tokens: i64, output_tokens: i64) -> Value {
    json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    })
}

/// Merge helper for the streaming path: an Anthropic `message` skeleton with
/// no content yet, for the `message_start` event.
pub fn message_skeleton(id: &str, model: &str, input_tokens: i64) -> Value {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [],
        "stop_reason": Value::Null,
        "stop_sequence": Value::Null,
        "usage": usage_object(input_tokens, 0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_completion_becomes_a_text_message() {
        let out = from_openai(
            &json!({
                "id": "chatcmpl-1",
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "hello"}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 3},
            }),
            "claude-sonnet-4-6",
        );
        assert_eq!(out["id"], "msg_chatcmpl-1");
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["model"], "claude-sonnet-4-6");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "hello");
        assert_eq!(out["stop_reason"], "end_turn");
        assert!(out["stop_sequence"].is_null());
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert_eq!(out["usage"]["output_tokens"], 3);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 0);
    }

    #[test]
    fn tool_calls_become_tool_use_blocks_with_parsed_input() {
        let out = from_openai(
            &json!({
                "id": "c",
                "choices": [{"finish_reason": "tool_calls", "message": {
                    "role": "assistant",
                    "content": "on it",
                    "tool_calls": [{
                        "id": "call_0", "type": "function",
                        "function": {"name": "Read", "arguments": "{\"path\":\"a.rs\"}"},
                    }],
                }}],
            }),
            "m",
        );
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["content"][0]["text"], "on it");
        assert_eq!(out["content"][1]["type"], "tool_use");
        assert_eq!(out["content"][1]["id"], "call_0");
        assert_eq!(out["content"][1]["name"], "Read");
        assert_eq!(out["content"][1]["input"]["path"], "a.rs");
    }

    #[test]
    fn reasoning_becomes_a_leading_thinking_block() {
        let out = from_openai(
            &json!({"choices": [{"finish_reason": "stop", "message": {
                "content": "42", "reasoning_content": "counting",
            }}]}),
            "m",
        );
        assert_eq!(out["content"][0]["type"], "thinking");
        assert_eq!(out["content"][0]["thinking"], "counting");
        assert_eq!(out["content"][0]["signature"], THINKING_SIGNATURE);
        assert_eq!(out["content"][1]["text"], "42");
    }

    #[test]
    fn the_alternate_reasoning_spelling_works_too() {
        let out = from_openai(
            &json!({"choices": [{"message": {"content": "", "reasoning": "why"}}]}),
            "m",
        );
        assert_eq!(out["content"][0]["thinking"], "why");
    }

    #[test]
    fn an_empty_completion_still_yields_one_content_block() {
        let out = from_openai(&json!({"choices": []}), "m");
        assert_eq!(out["content"].as_array().unwrap().len(), 1);
        assert_eq!(out["content"][0]["text"], "");
        assert_eq!(out["stop_reason"], "end_turn");
    }

    #[test]
    fn a_truncated_completion_reports_max_tokens() {
        let out = from_openai(
            &json!({"choices": [{"finish_reason": "length", "message": {"content": "half"}}]}),
            "m",
        );
        assert_eq!(out["stop_reason"], "max_tokens");
    }

    /// Both spellings of the usage fields reach the client, because the
    /// reader is the one billing already uses.
    #[test]
    fn usage_is_read_in_either_spelling() {
        let anthropic_spelling = from_openai(
            &json!({"choices": [{"message": {"content": "x"}}],
                    "usage": {"input_tokens": 7, "output_tokens": 8}}),
            "m",
        );
        assert_eq!(anthropic_spelling["usage"]["input_tokens"], 7);
        assert_eq!(anthropic_spelling["usage"]["output_tokens"], 8);

        let no_usage = from_openai(&json!({"choices": [{"message": {"content": "x"}}]}), "m");
        assert_eq!(no_usage["usage"]["input_tokens"], 0);
        assert_eq!(no_usage["usage"]["output_tokens"], 0);
    }

    /// Arguments a model mangled still produce a usable `input` object — the
    /// shared coercion, so this can't drift from what the tool loop replays.
    #[test]
    fn garbage_tool_arguments_degrade_to_an_empty_input() {
        let out = from_openai(
            &json!({"choices": [{"finish_reason": "tool_calls", "message": {"content": "",
                "tool_calls": [{"id": "a", "function": {"name": "Now", "arguments": ""}}]}}]}),
            "m",
        );
        assert_eq!(out["content"][0]["input"], json!({}));
    }
}
