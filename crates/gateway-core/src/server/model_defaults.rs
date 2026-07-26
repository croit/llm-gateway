// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Apply per-model default sampling parameters to a chat-completion
//! request body before forwarding it upstream.
//!
//! The admin UI at `/admin/models` stores a TOML key=value blob per
//! model name. At request time both the chat-page path
//! (`openai_driver.rs`) and the /v1 proxy path (`proxy.rs`) call
//! [`apply_defaults`], which:
//!
//! 1. Looks up the row keyed by the request body's `model` field.
//! 2. Parses the row's TOML into a flat table of scalars.
//! 3. For each top-level key the *request* didn't already set,
//!    inserts the stored default.
//!
//! Client-supplied values always win — that's the whole contract,
//! and what makes the admin UI a "defaults" surface rather than an
//! override surface.

use serde_json::Value;
use thiserror::Error;

use crate::server::db::{Pool, model_defaults as db};

#[derive(Debug, Error)]
pub enum DefaultsError {
    #[error("TOML parse: {0}")]
    TomlParse(#[from] toml::de::Error),
    /// Top level must be a table of scalars (or arrays of scalars).
    /// Reject anything that nests further — sampling parameters are
    /// flat by definition, and a nested table would silently fail to
    /// merge into the request body.
    #[error("`{0}` is not a scalar / array-of-scalars; sampling params must be flat")]
    NotScalar(String),
    #[error("db: {0}")]
    Db(#[from] crate::server::db::DbError),
}

/// Look up the stored defaults for the request's `model` and merge
/// them in. No-op (and no DB hit on the cached-empty path) when the
/// model isn't in the body, or when the stored row is missing /
/// empty.
pub async fn apply_defaults(pool: &Pool, body: &mut Value) -> Result<(), DefaultsError> {
    let Some(model) = body.get("model").and_then(|m| m.as_str()) else {
        return Ok(());
    };
    let model = model.to_string();
    let Some(row) = db::get(pool, &model).await? else {
        return Ok(());
    };
    if row.defaults_toml.trim().is_empty() {
        return Ok(());
    }
    let defaults = parse_defaults(&row.defaults_toml)?;
    fill_missing_keys(body, &defaults);
    Ok(())
}

/// Bytes-oriented variant for the /v1 proxy's pass-through path,
/// which holds the request body as raw `Bytes` until just before
/// forwarding. Short-circuits on the common "no stored row" case so
/// the fast path stays fast (one indexed DB lookup, no parse).
/// On any error (TOML broken, JSON malformed) we log + return the
/// original bytes — the upstream's own response handles the
/// downstream-visible failure.
pub async fn apply_defaults_to_bytes(
    pool: &Pool,
    model: &str,
    body: rama::bytes::Bytes,
) -> rama::bytes::Bytes {
    let row = match db::get(pool, model).await {
        Ok(Some(r)) if !r.defaults_toml.trim().is_empty() => r,
        Ok(_) => return body,
        Err(err) => {
            tracing::warn!(error = %err, %model, "model_defaults: db lookup failed");
            return body;
        }
    };
    let defaults = match parse_defaults(&row.defaults_toml) {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!(error = %err, %model, "model_defaults: TOML parse failed");
            return body;
        }
    };
    let mut value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, %model, "model_defaults: body not JSON; passing through");
            return body;
        }
    };
    fill_missing_keys(&mut value, &defaults);
    match serde_json::to_vec(&value) {
        Ok(out) => rama::bytes::Bytes::from(out),
        Err(err) => {
            tracing::warn!(error = %err, %model, "model_defaults: re-serialise failed");
            body
        }
    }
}

/// Parse a TOML blob into a JSON object whose values are all scalars
/// (or arrays of scalars). Save-time validator + request-time parser
/// share this implementation so what's saved is exactly what
/// `apply_defaults` will accept later.
pub fn parse_defaults(toml_text: &str) -> Result<serde_json::Map<String, Value>, DefaultsError> {
    let parsed: toml::Value = toml::from_str(toml_text)?;
    let table = match parsed {
        toml::Value::Table(t) => t,
        _ => return Err(DefaultsError::NotScalar("(root)".to_string())),
    };
    let mut out = serde_json::Map::with_capacity(table.len());
    for (k, v) in table {
        let json_value = toml_to_json_scalar(&k, v)?;
        out.insert(k, json_value);
    }
    Ok(out)
}

/// Top-level merge: any key from `defaults` not already present in
/// `body` gets inserted. Doesn't recurse into nested structures —
/// sampling params are flat. Non-object request bodies are
/// left untouched (the upstream will reject them anyway).
fn fill_missing_keys(body: &mut Value, defaults: &serde_json::Map<String, Value>) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for (k, v) in defaults {
        if !obj.contains_key(k) {
            obj.insert(k.clone(), v.clone());
        }
    }
}

fn toml_to_json_scalar(key: &str, v: toml::Value) -> Result<Value, DefaultsError> {
    Ok(match v {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Array(arr) => {
            // Allow `stop = ["END", "<|im_end|>"]` — useful for stop
            // tokens — but require each element be a scalar.
            let mut out = Vec::with_capacity(arr.len());
            for (idx, elem) in arr.into_iter().enumerate() {
                match elem {
                    toml::Value::String(s) => out.push(Value::String(s)),
                    toml::Value::Integer(i) => out.push(Value::Number(i.into())),
                    toml::Value::Float(f) => out.push(
                        serde_json::Number::from_f64(f)
                            .map(Value::Number)
                            .unwrap_or(Value::Null),
                    ),
                    toml::Value::Boolean(b) => out.push(Value::Bool(b)),
                    _ => {
                        return Err(DefaultsError::NotScalar(format!("{key}[{idx}]")));
                    }
                }
            }
            Value::Array(out)
        }
        _ => return Err(DefaultsError::NotScalar(key.to_string())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_accepts_typical_sampling_params() {
        let toml = r#"
temperature      = 0.7
top_p            = 0.95
top_k            = 40
min_p            = 0.05
repeat_penalty   = 1.1
frequency_penalty= 0.0
presence_penalty = 0.0
max_tokens       = 2048
stop             = ["END", "<|im_end|>"]
        "#;
        let parsed = parse_defaults(toml).unwrap();
        assert_eq!(parsed["temperature"], json!(0.7));
        assert_eq!(parsed["top_k"], json!(40));
        assert_eq!(parsed["stop"], json!(["END", "<|im_end|>"]));
    }

    #[test]
    fn parse_rejects_nested_table() {
        let toml = r#"
[sampling]
temperature = 0.7
        "#;
        let err = parse_defaults(toml).unwrap_err();
        assert!(matches!(err, DefaultsError::NotScalar(ref k) if k == "sampling"));
    }

    #[test]
    fn parse_rejects_invalid_toml() {
        let err = parse_defaults("temperature = ").unwrap_err();
        assert!(matches!(err, DefaultsError::TomlParse(_)), "{err:?}");
    }

    #[test]
    fn merge_fills_missing_keys() {
        let mut body = json!({"model": "m", "messages": [], "temperature": 0.2});
        let defaults = parse_defaults("temperature = 0.7\ntop_p = 0.95").unwrap();
        fill_missing_keys(&mut body, &defaults);
        // Client value wins.
        assert_eq!(body["temperature"], json!(0.2));
        // Stored default fills in.
        assert_eq!(body["top_p"], json!(0.95));
    }

    #[test]
    fn merge_noop_for_non_object_body() {
        let mut body = json!([1, 2, 3]);
        let defaults = parse_defaults("temperature = 0.7").unwrap();
        fill_missing_keys(&mut body, &defaults);
        assert_eq!(body, json!([1, 2, 3]));
    }

    #[tokio::test]
    async fn apply_defaults_round_trip_against_db() {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        crate::server::db::model_defaults::upsert(
            &pool,
            "m",
            "temperature = 0.7\nmax_tokens = 1024",
        )
        .await
        .unwrap();
        let mut body = json!({"model": "m", "messages": []});
        apply_defaults(&pool, &mut body).await.unwrap();
        assert_eq!(body["temperature"], json!(0.7));
        assert_eq!(body["max_tokens"], json!(1024));
    }

    #[tokio::test]
    async fn apply_defaults_skips_when_no_row() {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let mut body = json!({"model": "untouched", "messages": []});
        apply_defaults(&pool, &mut body).await.unwrap();
        assert_eq!(body, json!({"model": "untouched", "messages": []}));
    }
}
