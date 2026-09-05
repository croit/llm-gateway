// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The Anthropic error envelope.
//!
//! ```json
//! {"type": "error", "error": {"type": "invalid_request_error", "message": "…"}}
//! ```
//!
//! [`from_upstream`] is the one that matters. Claude Code recovers from a
//! handful of upstream rejections — an unsupported `thinking` field, a
//! rejected `cache_control` marker — by **matching on the error message text**
//! and retrying with that capability disabled. A gateway that reworded the
//! message would keep the status code correct and still break the recovery
//! path, turning a self-healing retry into a hard failure for the developer.
//! So the upstream's own wording is lifted out and carried through unchanged;
//! only the envelope around it changes.

use serde_json::{Value, json};

/// How many bytes of an unparseable upstream body to echo back. Enough to
/// carry a real error message, short enough that an HTML error page or a
/// stack trace doesn't become the client's error string.
const MAX_RELAYED_BODY: usize = 1000;

/// The Anthropic error type for an HTTP status.
pub fn kind_for_status(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        422 => "invalid_request_error",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        // 500s and anything unexpected.
        _ => "api_error",
    }
}

/// Build an error envelope.
pub fn envelope(kind: &str, message: &str) -> Value {
    json!({"type": "error", "error": {"type": kind, "message": message}})
}

/// Build an error envelope for `status`, choosing the type from the status.
pub fn for_status(status: u16, message: &str) -> Value {
    envelope(kind_for_status(status), message)
}

/// Re-wrap an upstream (OpenAI-shaped) error body, preserving its wording.
///
/// Reads, in order of preference, `error.message`, a top-level `message`, or
/// FastAPI/vLLM's `detail`; falls back to the raw body text. The result is
/// always a valid Anthropic error envelope even when the upstream returned
/// HTML or nothing at all.
pub fn from_upstream(status: u16, body: &[u8]) -> Value {
    let message = upstream_message(body)
        .unwrap_or_else(|| format!("upstream returned HTTP {status} with no error message"));
    for_status(status, &message)
}

/// The human-readable message inside an upstream error body, if there is one.
fn upstream_message(body: &[u8]) -> Option<String> {
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        for pointer in ["/error/message", "/message", "/detail"] {
            if let Some(m) = v.pointer(pointer).and_then(Value::as_str)
                && !m.trim().is_empty()
            {
                return Some(m.to_string());
            }
        }
        // `detail` is sometimes a list of validation errors rather than a
        // string; relaying its JSON keeps the wording the upstream chose.
        if let Some(detail) = v.pointer("/detail").filter(|d| !d.is_null()) {
            return Some(session_core::render::truncate_chars(
                &detail.to_string(),
                MAX_RELAYED_BODY,
            ));
        }
    }
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| session_core::render::truncate_chars(trimmed, MAX_RELAYED_BODY))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_anthropic_error_types() {
        assert_eq!(kind_for_status(400), "invalid_request_error");
        assert_eq!(kind_for_status(401), "authentication_error");
        assert_eq!(kind_for_status(403), "permission_error");
        assert_eq!(kind_for_status(404), "not_found_error");
        assert_eq!(kind_for_status(429), "rate_limit_error");
        assert_eq!(kind_for_status(500), "api_error");
        assert_eq!(kind_for_status(503), "api_error");
        assert_eq!(kind_for_status(529), "overloaded_error");
    }

    #[test]
    fn an_openai_error_keeps_its_wording() {
        let body = br#"{"error":{"message":"thinking is not supported for this model","type":"invalid_request_error"}}"#;
        let out = from_upstream(400, body);
        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["type"], "invalid_request_error");
        assert_eq!(
            out["error"]["message"],
            "thinking is not supported for this model"
        );
    }

    #[test]
    fn a_vllm_detail_body_is_relayed() {
        let out = from_upstream(
            400,
            br#"{"detail":"Stream options can only be defined when stream=True"}"#,
        );
        assert_eq!(
            out["error"]["message"],
            "Stream options can only be defined when stream=True"
        );
    }

    #[test]
    fn a_structured_detail_is_relayed_as_json() {
        let out = from_upstream(
            422,
            br#"{"detail":[{"loc":["body","max_tokens"],"msg":"too large"}]}"#,
        );
        let msg = out["error"]["message"].as_str().unwrap();
        assert!(msg.contains("too large"), "{msg}");
        assert_eq!(out["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn a_non_json_body_is_relayed_as_text() {
        let out = from_upstream(502, b"<html>bad gateway</html>");
        assert_eq!(out["error"]["message"], "<html>bad gateway</html>");
        assert_eq!(out["error"]["type"], "api_error");
    }

    #[test]
    fn an_empty_body_still_says_something_useful() {
        let out = from_upstream(500, b"");
        let msg = out["error"]["message"].as_str().unwrap();
        assert!(msg.contains("500"), "{msg}");
    }

    #[test]
    fn an_oversized_body_is_truncated() {
        let body = "x".repeat(5000);
        let out = from_upstream(500, body.as_bytes());
        let msg = out["error"]["message"].as_str().unwrap();
        assert!(msg.chars().count() <= MAX_RELAYED_BODY + 1, "{}", msg.len());
        assert!(msg.ends_with('…'));
    }
}
