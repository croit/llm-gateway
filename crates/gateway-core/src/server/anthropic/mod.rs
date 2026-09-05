// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Anthropic **Messages API** ⇄ OpenAI **chat-completions** translation.
//!
//! This is the compatibility layer that lets Claude Code — which speaks only
//! the Anthropic wire format — drive the same upstreams, routing, limits,
//! tool loop and usage accounting as every other `/v1` client. The gateway
//! serves `POST /v1/messages`; everything behind that endpoint is the
//! OpenAI-shaped pipeline we already had.
//!
//! The module is deliberately **pure**: `serde_json::Value` in, `Value` (or
//! SSE frame strings) out, no HTTP, no database, no rama types. That keeps
//! the hard part — the format mapping — unit-testable against fixtures, and
//! leaves the I/O edges (`rama_server::messages`) thin.
//!
//! Four pieces:
//!   - [`request`]: an Anthropic `/v1/messages` body → an OpenAI
//!     `/v1/chat/completions` body (plus the request-shape facts the handler
//!     needs: model, stream flag, reasoning effort).
//!   - [`response`]: a buffered OpenAI completion → an Anthropic `Message`.
//!   - [`stream`]: a state machine turning streamed OpenAI chunks into the
//!     Anthropic SSE event sequence (`message_start` … `message_stop`).
//!   - [`error`]: the Anthropic error envelope, preserving upstream wording.
//!
//! ## Why the wording of upstream errors is preserved verbatim
//!
//! Claude Code recovers from a handful of upstream rejections (an unsupported
//! `thinking` field, a rejected `cache_control` marker) by matching on the
//! error *message text* and retrying without the capability. A gateway that
//! rewrites the message breaks that recovery even when the status code is
//! right — so [`error::from_upstream`] re-wraps the upstream body in the
//! Anthropic envelope but never rewords it.

pub mod error;
pub mod request;
pub mod response;
pub mod stream;

/// The `anthropic-version` we implement. Sent back on responses so a client
/// can tell which dialect answered.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic `stop_reason` values we can produce, mapped from the OpenAI
/// `finish_reason` of the same turn.
///
/// `pause_turn` (server-tool continuation) has no OpenAI counterpart, and we
/// never serve Anthropic-hosted server tools, so it is not in the map.
pub fn stop_reason_for(finish_reason: Option<&str>) -> &'static str {
    match finish_reason {
        Some("tool_calls") | Some("function_call") => "tool_use",
        Some("length") => "max_tokens",
        Some("content_filter") => "refusal",
        // `stop` — and anything we don't recognise — is a normal end of turn.
        _ => "end_turn",
    }
}

/// A message id in Anthropic's shape (`msg_…`), derived from the upstream's
/// own completion id so the two are correlatable in logs. Upstream ids look
/// like `chatcmpl-abc123`; anything unusable falls back to a fixed prefix.
pub fn message_id(upstream_id: Option<&str>) -> String {
    let raw = upstream_id.unwrap_or("").trim();
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if cleaned.is_empty() {
        "msg_gateway".to_string()
    } else {
        format!("msg_{cleaned}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reasons_map_to_anthropic_stop_reasons() {
        assert_eq!(stop_reason_for(Some("stop")), "end_turn");
        assert_eq!(stop_reason_for(Some("length")), "max_tokens");
        assert_eq!(stop_reason_for(Some("tool_calls")), "tool_use");
        assert_eq!(stop_reason_for(Some("content_filter")), "refusal");
        assert_eq!(stop_reason_for(None), "end_turn");
        assert_eq!(stop_reason_for(Some("something_new")), "end_turn");
    }

    #[test]
    fn message_ids_are_derived_from_the_upstream_id() {
        assert_eq!(message_id(Some("chatcmpl-abc123")), "msg_chatcmpl-abc123");
        assert_eq!(message_id(Some("")), "msg_gateway");
        assert_eq!(message_id(None), "msg_gateway");
        // Punctuation an upstream might emit is dropped, not passed through.
        assert_eq!(message_id(Some("a/b c")), "msg_abc");
    }
}
