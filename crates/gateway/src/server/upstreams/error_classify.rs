// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Upstream error classification for auto-learning model capabilities.
//!
//! When the gateway forwards a request and the upstream returns a 400/422 that
//! indicates the model doesn't support a specific content type (images, tools,
//! structured output), the classifier identifies which capability was rejected.
//! The proxy then records it via [`crate::server::db::model_defaults::mark_unsupported`]
//! so the next request avoids the same path — no second error after the first.
//!
//! Patterns match on substrings of the error body because every provider phrases
//! rejections differently, but they deliberately require an explicit
//! *capability-gap* phrase ("not supported", "does not support", …) rather than
//! mere invalidity. A malformed request (a bad tool schema, an invalid
//! `response_format`) is the *caller's* fault, not a property of the model, and
//! must not be learned as a permanent `Some(false)` — otherwise one bad request
//! would durably disable a capability the model actually has. Admin-set
//! `Some(true)` is still never overwritten by [`mark_unsupported`], so the only
//! residual false positive is an unconfigured capability landing on `Some(false)`.

use crate::server::db::model_defaults::CapabilityField;

/// Classify an upstream HTTP error body to determine which capability was
/// rejected, if any. Returns `None` for errors that don't match any known
/// capability-rejection pattern (network errors, 500s, rate limits, etc.).
pub fn classify_error(status: u16, body: &str) -> Option<CapabilityField> {
    if status != 400 && status != 422 {
        return None;
    }
    let lower = body.to_ascii_lowercase();

    if is_vision_rejection(&lower) {
        return Some(CapabilityField::Vision);
    }
    if is_tools_rejection(&lower) {
        return Some(CapabilityField::Tools);
    }
    if is_structured_output_rejection(&lower) {
        return Some(CapabilityField::StructuredOutput);
    }
    None
}

/// Phrases that specifically say the *model can't do this* — a capability gap,
/// as opposed to a malformed request. Only these promote a substring match into
/// a learned `Some(false)`.
fn says_unsupported(body: &str) -> bool {
    body.contains("not supported")
        || body.contains("not support")
        || body.contains("does not support")
        || body.contains("doesn't support")
        || body.contains("unsupported")
        || body.contains("not available")
        || body.contains("not capable")
        || body.contains("cannot process")
        || body.contains("can't process")
}

fn is_vision_rejection(body: &str) -> bool {
    // GLM/z.AI restricts content types on text-only models with the specific
    // phrase "…content.type is invalid, allowed values: ['text']" — a genuine
    // capability rejection, not a malformed-payload error, so keep it verbatim.
    (body.contains("content.type is invalid") && body.contains("text"))
        || (body.contains("image") && says_unsupported(body))
        || (body.contains("image_url") && says_unsupported(body))
        || (body.contains("multimodal") && says_unsupported(body))
        || (body.contains("vision") && says_unsupported(body))
}

fn is_tools_rejection(body: &str) -> bool {
    (body.contains("tools") && says_unsupported(body))
        || (body.contains("tool_choice") && says_unsupported(body))
        || (body.contains("function calling") && says_unsupported(body))
        || (body.contains("function_calling") && says_unsupported(body))
}

fn is_structured_output_rejection(body: &str) -> bool {
    (body.contains("response_format") && says_unsupported(body))
        || (body.contains("json_schema") && says_unsupported(body))
        || (body.contains("structured output") && says_unsupported(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glm_vision_rejection() {
        let body = r#"{"error":{"code":"1210","message":"messages.content.type is invalid, allowed values: ['text']"}}"#;
        assert_eq!(classify_error(400, body), Some(CapabilityField::Vision));
    }

    #[test]
    fn generic_image_not_supported() {
        let body = r#"{"error":{"message":"image input is not supported by this model"}}"#;
        assert_eq!(classify_error(400, body), Some(CapabilityField::Vision));
    }

    #[test]
    fn tools_not_supported() {
        let body = r#"{"error":{"message":"tools parameter is not supported"}}"#;
        assert_eq!(classify_error(400, body), Some(CapabilityField::Tools));
    }

    #[test]
    fn structured_output_not_supported() {
        let body = r#"{"error":{"message":"response_format json_schema is not supported"}}"#;
        assert_eq!(
            classify_error(400, body),
            Some(CapabilityField::StructuredOutput)
        );
    }

    #[test]
    fn rate_limit_not_classified() {
        let body = r#"{"error":{"message":"rate limit exceeded"}}"#;
        assert_eq!(classify_error(429, body), None);
    }

    #[test]
    fn server_error_not_classified() {
        let body = r#"{"error":{"message":"internal server error"}}"#;
        assert_eq!(classify_error(500, body), None);
    }

    #[test]
    fn network_error_not_classified() {
        assert_eq!(classify_error(502, "connection refused"), None);
    }

    #[test]
    fn unrelated_400_not_classified() {
        let body = r#"{"error":{"message":"max_tokens must be positive"}}"#;
        assert_eq!(classify_error(400, body), None);
    }

    /// A malformed tool schema is the caller's fault, not a capability gap — it
    /// must NOT be learned as `cap_tools = false` on a tools-capable model.
    #[test]
    fn malformed_tools_schema_not_classified() {
        let body = r#"{"error":{"type":"invalid_request_error","param":"tools[0].function.parameters","message":"tools[0].function.parameters is invalid"}}"#;
        assert_eq!(classify_error(400, body), None);
    }

    /// "function calling requires a tool definition" contains both "function
    /// calling" and "not"/"none" but is a usage error, not "not supported".
    #[test]
    fn function_calling_usage_error_not_classified() {
        let body = r#"{"error":{"message":"function calling requires a tool definition, none was provided"}}"#;
        assert_eq!(classify_error(400, body), None);
    }

    /// An invalid (but supported) response_format is a caller error, not a
    /// structured-output capability gap.
    #[test]
    fn invalid_response_format_not_classified() {
        let body =
            r#"{"error":{"message":"response_format json_schema is invalid: missing 'schema'"}}"#;
        assert_eq!(classify_error(400, body), None);
    }
}
