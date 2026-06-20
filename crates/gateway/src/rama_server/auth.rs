// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Bearer-token auth for the rama gateway.
//!
//! Ported from `crate::server::auth::middleware`. Same wire shape
//! (`Authorization: Bearer gwk_…`, OpenAI-shaped 401 envelope on miss)
//! but expressed as a helper handlers call at their entry instead of a
//! tower-style Layer. Rama supports layers too, but for the small set
//! of bearer-gated routes we have, in-handler is more readable and
//! avoids the extension/context plumbing.

use rama::http::header::{AUTHORIZATION, HeaderValue};
use rama::http::service::web::response::IntoResponse;
use rama::http::{HeaderMap, Response, StatusCode};
use serde_json::json;

use crate::rama_server::state::RamaState;
use crate::server::auth::UserCtx;
use crate::server::auth::token;
use crate::server::db;

/// Reads + validates the bearer token from `headers`, returning the
/// user context or a fully-built 401 response. Background-bumps the
/// token's `last_used_at` on success; failures of that bump don't
/// affect the request (logged + dropped).
pub async fn require_bearer(state: &RamaState, headers: &HeaderMap) -> Result<UserCtx, Response> {
    let bearer = parse_bearer(headers.get(AUTHORIZATION)).ok_or_else(unauthorized)?;
    let hash = token::hash_bearer(bearer).ok_or_else(unauthorized)?;

    let token_row = db::tokens::find_active_by_hash(&state.db, &hash)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "token lookup failed");
            internal_error("token lookup failed")
        })?
        .ok_or_else(unauthorized)?;

    let user = db::users::find_by_id(&state.db, &token_row.user_id)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "user lookup failed");
            internal_error("user lookup failed")
        })?
        .ok_or_else(|| internal_error("token references missing user"))?;

    // Fire-and-forget last_used_at bump. Same pattern as the axum
    // middleware so behaviour on the wire is identical.
    let pool = state.db.clone();
    let token_id = token_row.id.clone();
    tokio::spawn(async move {
        if let Err(err) = db::tokens::touch(&pool, &token_id).await {
            tracing::warn!(error = %err, token_id, "failed to bump last_used_at");
        }
    });

    Ok(UserCtx {
        user_id: user.id,
        user_email: user.email,
        token_id: token_row.id,
        token_name: token_row.name,
        roles: user.roles,
        tools_enabled: token_row.tools_enabled,
    })
}

fn parse_bearer(value: Option<&HeaderValue>) -> Option<&str> {
    let s = value?.to_str().ok()?;
    let rest = s.strip_prefix("Bearer ")?;
    let trimmed = rest.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn unauthorized() -> Response {
    let body = json!({
        "error": {
            "message": "missing or invalid bearer token",
            "type": "unauthorized",
            "code": "unauthorized",
        }
    });
    (
        StatusCode::UNAUTHORIZED,
        [
            ("content-type", "application/json"),
            // OAuth 2.0 §3.1 conformance: tell the client which scheme
            // we expected. Same value the axum side emits.
            ("www-authenticate", r#"Bearer realm="gateway""#),
        ],
        body.to_string(),
    )
        .into_response()
}

fn internal_error(message: &str) -> Response {
    let body = json!({
        "error": {
            "message": message,
            "type": "internal_error",
            "code": "internal_error",
        }
    });
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [("content-type", "application/json")],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_value(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn parse_bearer_accepts_well_formed() {
        let v = header_value("Bearer gwk_abc");
        assert_eq!(parse_bearer(Some(&v)), Some("gwk_abc"));
    }

    #[test]
    fn parse_bearer_rejects_missing_scheme() {
        let v = header_value("gwk_abc");
        assert!(parse_bearer(Some(&v)).is_none());
    }

    #[test]
    fn parse_bearer_rejects_wrong_scheme() {
        let v = header_value("Basic dXNlcjpwYXNz");
        assert!(parse_bearer(Some(&v)).is_none());
    }

    #[test]
    fn parse_bearer_rejects_empty_value() {
        let v = header_value("Bearer   ");
        assert!(parse_bearer(Some(&v)).is_none());
    }

    #[test]
    fn parse_bearer_rejects_missing_header() {
        assert!(parse_bearer(None).is_none());
    }
}
