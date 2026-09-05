// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Coercion for a tool call's `arguments` field.
//!
//! One rule, in one place, because getting it wrong is a hard `400` rather
//! than a soft degradation. A model that calls a no-argument tool frequently
//! emits `""` or a bare `{`, and some backends' chat templates run
//! `json.loads(arguments)` over whatever we replay at them
//! (Mistral/Voxtral via `mistral_common`) — a non-JSON value there fails the
//! *next* request, not the one that produced it.
//!
//! Lives in `gateway-core` rather than next to the tool loop that first
//! needed it because the Anthropic translation layer needs the identical
//! rule: it turns the same `arguments` string into the structured `input` of
//! a `tool_use` block. Two implementations of "coerce to a JSON object" would
//! be two chances to diverge on the constraint above.

use serde_json::Value;

/// The `arguments` string as the JSON object every consumer wants. Anything
/// that isn't a JSON *object* — empty, malformed, or a bare scalar/array —
/// becomes `{}` rather than failing the turn.
pub fn tool_arguments_object(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => v,
        _ => Value::Object(serde_json::Map::new()),
    }
}

/// String form of [`tool_arguments_object`] — the canonical JSON-object text
/// to embed in a `tool_calls[].function.arguments` field replayed upstream.
/// Always valid JSON, so it can't `400` a strict re-parse; identical to the
/// value the tool is actually run with, so history never diverges from what
/// happened.
pub fn normalize_tool_arguments(raw: &str) -> String {
    tool_arguments_object(raw).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_object_survives_unchanged() {
        assert_eq!(tool_arguments_object(r#"{"a":1}"#), json!({"a": 1}));
    }

    #[test]
    fn anything_that_is_not_an_object_becomes_one() {
        for raw in ["", "   ", "not json", "{", "[1,2]", "42", "null"] {
            assert_eq!(tool_arguments_object(raw), json!({}), "input: {raw:?}");
        }
    }

    #[test]
    fn the_string_form_is_always_valid_json() {
        for raw in ["", "{", r#"{"a":1}"#] {
            let out = normalize_tool_arguments(raw);
            serde_json::from_str::<Value>(&out).unwrap_or_else(|e| panic!("{raw:?} → {out}: {e}"));
        }
    }
}
