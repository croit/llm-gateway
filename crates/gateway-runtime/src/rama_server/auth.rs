// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Bearer-token auth for the rama gateway.
//!
//! Ported from `gateway_core::server::auth::middleware`. Same wire shape
//! (`Authorization: Bearer gwk_…`, OpenAI-shaped 401 envelope on miss)
//! but expressed as a helper handlers call at their entry instead of a
//! tower-style Layer. Rama supports layers too, but for the small set
//! of bearer-gated routes we have, in-handler is more readable and
//! avoids the extension/context plumbing.
//!
//! `x-api-key` is accepted as a second spelling of the same credential.
//! Anthropic-format clients put the key in whichever header their
//! configuration implies — Claude Code sends `Authorization: Bearer` for
//! `ANTHROPIC_AUTH_TOKEN`, `x-api-key` for `ANTHROPIC_API_KEY`, and *both*
//! on its model-discovery request — so reading only one header would make
//! authentication depend on which environment variable a developer happened
//! to be told to set. Same token, same lookup, same 401.

use rama::http::header::{AUTHORIZATION, HeaderValue};
use rama::http::service::web::response::IntoResponse;
use rama::http::{HeaderMap, Response, StatusCode};
use serde_json::json;

use crate::rama_server::state::RamaState;
use gateway_core::server::auth::UserCtx;
use gateway_core::server::auth::token;
use gateway_core::server::db;

/// The second header the same gateway token may arrive in. Lowercase because
/// `HeaderMap` lookups by `&str` are case-insensitive only for the canonical
/// (lowercase) spelling.
const API_KEY_HEADER: &str = "x-api-key";

/// Reads + validates the bearer token from `headers`, returning the
/// user context or a fully-built 401 response. Background-bumps the
/// token's `last_used_at` on success; failures of that bump don't
/// affect the request (logged + dropped).
pub async fn require_bearer(
    state: &RamaState,
    headers: &HeaderMap,
) -> Result<UserCtx, AuthRefusal> {
    let bearer = credential(headers).ok_or_else(unauthorized)?;
    let hash = token::hash_bearer(bearer).ok_or_else(unauthorized)?;

    let token_row = db::tokens::find_active_by_hash(&state.db, &hash)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "token lookup failed");
            internal_error("token lookup failed")
        })?
        .ok_or_else(unauthorized)?;

    // Both reads depend only on `token_row` and nothing orders them, so they
    // go out together: this is every `/v1` request's critical path, and
    // awaiting them in sequence would spend two round-trips where one does.
    //
    // The allowlist gets the same failure posture as the lookups around it —
    // a database error is a 500, never a silent promotion to unrestricted.
    // `None` means the token genuinely has no allowlist.
    let (user, allowed_models) = tokio::try_join!(
        async {
            db::users::find_by_id(&state.db, &token_row.user_id)
                .await
                .map_err(|err| {
                    tracing::warn!(error = %err, "user lookup failed");
                    internal_error("user lookup failed")
                })
        },
        async {
            db::token_models::for_token(&state.db, &token_row.id)
                .await
                .map_err(|err| {
                    tracing::warn!(error = %err, "token model allowlist lookup failed");
                    internal_error("token model allowlist lookup failed")
                })
        }
    )?;
    let user = user.ok_or_else(|| internal_error("token references missing user"))?;
    let allowed_models = allowed_models.map(std::sync::Arc::new);

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
        allowed_models,
    })
}

/// The caller's token from either header it may arrive in: `Authorization:
/// Bearer …` first, then `x-api-key`. A client that sends both (Claude Code's
/// model discovery does) carries the same value in each, so first-wins is
/// safe.
fn credential(headers: &HeaderMap) -> Option<&str> {
    parse_bearer(headers.get(AUTHORIZATION)).or_else(|| parse_api_key(headers.get(API_KEY_HEADER)))
}

fn parse_bearer(value: Option<&HeaderValue>) -> Option<&str> {
    let s = value?.to_str().ok()?;
    let rest = s.strip_prefix("Bearer ")?;
    let trimmed = rest.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// `x-api-key` carries the bare token — no scheme prefix.
fn parse_api_key(value: Option<&HeaderValue>) -> Option<&str> {
    let trimmed = value?.to_str().ok()?.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Why a request was refused, before anything decides how to say so.
///
/// `require_bearer` used to hand back a rendered OpenAI-shaped `Response`,
/// which works only while every caller speaks OpenAI. The Anthropic endpoints
/// have to say the same thing in their own envelope, and reconstructing it
/// from a rendered response means sniffing the status and inventing a message
/// — which loses the distinction between "your token is not valid" and "the
/// user lookup failed". Carrying the two facts and rendering late keeps that.
#[derive(Debug, Clone)]
pub struct AuthRefusal {
    pub status: StatusCode,
    pub message: String,
    /// Whether to advertise the expected scheme (OAuth 2.0 §3.1). Only a 401
    /// carries it.
    challenge: bool,
}

impl IntoResponse for AuthRefusal {
    /// The OpenAI-shaped envelope every existing caller already returns.
    fn into_response(self) -> Response {
        let code = if self.status == StatusCode::UNAUTHORIZED {
            "unauthorized"
        } else {
            "internal_error"
        };
        let body = json!({
            "error": {
                "message": self.message,
                "type": code,
                "code": code,
            }
        });
        let mut resp = (
            self.status,
            [("content-type", "application/json")],
            body.to_string(),
        )
            .into_response();
        if self.challenge {
            resp.headers_mut().insert(
                rama::http::header::WWW_AUTHENTICATE,
                HeaderValue::from_static(r#"Bearer realm="gateway""#),
            );
        }
        resp
    }
}

fn unauthorized() -> AuthRefusal {
    AuthRefusal {
        status: StatusCode::UNAUTHORIZED,
        message: "missing or invalid bearer token".into(),
        challenge: true,
    }
}

fn internal_error(message: &str) -> AuthRefusal {
    AuthRefusal {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: message.to_string(),
        challenge: false,
    }
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

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                rama::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                header_value(value),
            );
        }
        map
    }

    #[test]
    fn credential_reads_the_authorization_header() {
        let h = headers(&[("authorization", "Bearer gwk_abc")]);
        assert_eq!(credential(&h), Some("gwk_abc"));
    }

    /// `ANTHROPIC_API_KEY` puts the credential here, with no scheme prefix.
    #[test]
    fn credential_falls_back_to_x_api_key() {
        let h = headers(&[("x-api-key", "gwk_abc")]);
        assert_eq!(credential(&h), Some("gwk_abc"));
    }

    /// Claude Code's model-discovery request sends both headers.
    #[test]
    fn credential_prefers_authorization_when_both_are_present() {
        let h = headers(&[
            ("authorization", "Bearer gwk_auth"),
            ("x-api-key", "gwk_key"),
        ]);
        assert_eq!(credential(&h), Some("gwk_auth"));
    }

    /// A malformed `Authorization` doesn't shadow a usable `x-api-key` — the
    /// two headers are alternatives, not a chain that fails closed on the
    /// first one.
    #[test]
    fn a_malformed_authorization_falls_through_to_x_api_key() {
        let h = headers(&[("authorization", "Basic nope"), ("x-api-key", "gwk_key")]);
        assert_eq!(credential(&h), Some("gwk_key"));
    }

    #[test]
    fn credential_rejects_an_empty_x_api_key() {
        let h = headers(&[("x-api-key", "   ")]);
        assert!(credential(&h).is_none());
    }

    #[test]
    fn credential_rejects_no_headers_at_all() {
        assert!(credential(&HeaderMap::new()).is_none());
    }
}
