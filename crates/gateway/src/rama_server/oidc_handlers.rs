// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/auth/login`, `/auth/callback`, `/auth/logout` for the rama server.
//!
//! Re-implements the axum version (`crate::server::api::auth`) on top of
//! our hand-rolled session store + a new `pending_logins` DB table. The
//! tower-sessions key/value bag is replaced by a row keyed on the OIDC
//! `state` parameter — that value already round-trips through the IdP
//! and back to `/auth/callback`, so no cookie is needed to carry the
//! in-flight CSRF/PKCE/nonce trio.
//!
//! The OidcClient itself is unchanged; it has no axum coupling.

use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};
use rama::http::service::web::extract::{Query, State};
use rama::http::service::web::response::IntoResponse;
use rama::http::{HeaderMap, Request, Response, StatusCode, header};
use serde::Deserialize;
use serde_json::json;

use crate::rama_server::session::COOKIE_NAME;
use crate::rama_server::state::RamaState;
use crate::server::db::users;

/// TTL for the in-flight `pending_logins` row. Generous because some
/// IdPs (Authentik, Keycloak's account-linking flows) bounce the user
/// through several screens before redirecting back.
const PENDING_LOGIN_TTL: SignedDuration = SignedDuration::from_mins(15);

#[derive(Deserialize)]
pub struct LoginParams {
    /// Where to send the user after login. Optional; defaults to `/`.
    pub return_to: Option<String>,
    /// CLI handoff: if /auth/cli/begin already dropped us a state, the
    /// callback finishes that flow instead of redirecting normally.
    pub cli_state: Option<String>,
}

/// GET /auth/login — starts the OIDC dance.
pub async fn login(
    State(state): State<Arc<RamaState>>,
    Query(params): Query<LoginParams>,
) -> Response {
    let Some(oidc) = state.oidc.as_ref() else {
        return error_html(
            StatusCode::INTERNAL_SERVER_ERROR,
            "OIDC is not configured on this gateway",
        );
    };

    let start = oidc.begin();
    let now = Timestamp::now();
    let return_to = params.return_to.filter(|rt| rt.starts_with('/'));
    let cli_state = params.cli_state.filter(|s| !s.is_empty());

    let res = sqlx::query(
        "INSERT INTO pending_logins
           (state, pkce_verifier, nonce, return_to, cli_state, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&start.csrf)
    .bind(&start.pkce_verifier)
    .bind(&start.nonce)
    .bind(return_to.as_deref())
    .bind(cli_state.as_deref())
    .bind(now.to_string())
    .bind((now + PENDING_LOGIN_TTL).to_string())
    .execute(&state.db)
    .await;
    if let Err(err) = res {
        tracing::warn!(error = %err, "persisting pending login");
        return error_html(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not persist pending login state",
        );
    }

    redirect_to(&start.url)
}

#[derive(Deserialize)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// GET /auth/callback — receives the IdP's redirect.
pub async fn callback(
    State(state): State<Arc<RamaState>>,
    Query(params): Query<CallbackParams>,
) -> Response {
    if let Some(err) = params.error {
        let desc = params.error_description.unwrap_or_default();
        return error_html(
            StatusCode::BAD_REQUEST,
            &format!("OIDC provider returned an error: {err} ({desc})"),
        );
    }
    let Some(code) = params.code else {
        return error_html(StatusCode::BAD_REQUEST, "OIDC callback missing `code`");
    };
    let Some(state_param) = params.state else {
        return error_html(StatusCode::BAD_REQUEST, "OIDC callback missing `state`");
    };

    // Pull the in-flight row. Missing → either expired, already consumed,
    // or never started — all "go back to /auth/login" from the user POV.
    type PendingRow = (String, String, Option<String>, Option<String>, String);
    let pending: Option<PendingRow> = match sqlx::query_as(
        "SELECT pkce_verifier, nonce, return_to, cli_state, expires_at
             FROM pending_logins WHERE state = ?",
    )
    .bind(&state_param)
    .fetch_optional(&state.db)
    .await
    {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "pending login lookup");
            return error_html(StatusCode::INTERNAL_SERVER_ERROR, "session lookup failed");
        }
    };
    let Some((verifier, nonce, return_to, cli_state, expires_at_raw)) = pending else {
        return error_html(
            StatusCode::BAD_REQUEST,
            "OIDC callback without an in-flight session — restart at /auth/login",
        );
    };
    if let Ok(expires_at) = expires_at_raw.parse::<Timestamp>()
        && expires_at < Timestamp::now()
    {
        let _ = sqlx::query("DELETE FROM pending_logins WHERE state = ?")
            .bind(&state_param)
            .execute(&state.db)
            .await;
        return error_html(
            StatusCode::BAD_REQUEST,
            "OIDC pending login has expired — restart at /auth/login",
        );
    }

    // The row is single-use whether the exchange succeeds or not.
    let _ = sqlx::query("DELETE FROM pending_logins WHERE state = ?")
        .bind(&state_param)
        .execute(&state.db)
        .await;

    let Some(oidc) = state.oidc.as_ref() else {
        return error_html(
            StatusCode::INTERNAL_SERVER_ERROR,
            "OIDC is not configured on this gateway",
        );
    };
    let claims = match oidc.complete(&code, &verifier, &nonce).await {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(error = %err, "OIDC token exchange");
            return error_html(
                StatusCode::BAD_GATEWAY,
                &format!("OIDC token exchange failed: {err}"),
            );
        }
    };

    let now = Timestamp::now();
    if let Err(err) = users::upsert(
        &state.db,
        &users::User {
            id: claims.subject.clone(),
            email: claims.email,
            name: claims.name,
            roles: claims.roles,
            created_at: now,
            updated_at: now,
            // Timezone is set later by the browser via
            // `POST /api/v0/me/timezone`. `upsert` doesn't touch it on
            // conflict, so an existing user's previously-saved value
            // survives a re-login.
            timezone: None,
        },
    )
    .await
    {
        tracing::warn!(error = %err, "user upsert");
        return error_html(StatusCode::INTERNAL_SERVER_ERROR, "could not persist user");
    }

    // CLI handoff branch — finishes a `gw auth login` flow. The browser
    // gets a "you can close this tab" page; the polling CLI picks up the
    // freshly-minted token from cli_logins.
    if let Some(cli_state) = cli_state {
        return finish_cli_login(&state, &cli_state, &claims.subject).await;
    }

    // Browser flow: mint a session, set the signed cookie, redirect.
    let session = match state.sessions.create(&claims.subject).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "minting session");
            return error_html(StatusCode::INTERNAL_SERVER_ERROR, "could not mint session");
        }
    };
    let cookie = format!(
        "{name}={signed}; Path=/; HttpOnly; SameSite=Lax",
        name = COOKIE_NAME,
        signed = state.sessions.sign(&session.id),
    );
    // Default landing is the chat surface — a freshly signed-in user
    // should drop straight into a conversation, not a dashboard. An
    // explicit, same-origin `return_to` still wins.
    let target = return_to
        .filter(|rt| rt.starts_with('/'))
        .unwrap_or_else(|| "/chat".into());
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, target)
        .header(header::SET_COOKIE, cookie)
        .body("".into())
        .unwrap()
}

/// POST /auth/logout — destroy the current session.
pub async fn logout(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (parts, _body) = req.into_parts();
    if let Ok(Some(session)) = state.sessions.lookup_from_headers(&parts.headers).await {
        let _ = state.sessions.delete(&session.id).await;
    }
    // Tell the browser to clear the cookie regardless — handles the case
    // where the cookie is stale-but-valid-HMAC against a deleted row.
    let expire = format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, "/")
        .header(header::SET_COOKIE, expire)
        .body("".into())
        .unwrap()
}

async fn finish_cli_login(state: &RamaState, cli_state: &str, user_id: &str) -> Response {
    use crate::server::auth::token;
    use crate::server::db::{cli_logins, tokens};
    use uuid::Uuid;

    let row = match cli_logins::find(&state.db, cli_state).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return error_html(
                StatusCode::BAD_REQUEST,
                "CLI login state has expired — re-run `gw auth login`",
            );
        }
        Err(err) => {
            tracing::warn!(error = %err, "cli_logins lookup");
            return error_html(StatusCode::INTERNAL_SERVER_ERROR, "cli_login lookup failed");
        }
    };
    if row.expires_at < Timestamp::now() {
        return error_html(StatusCode::BAD_REQUEST, "CLI login state has expired");
    }

    let (plaintext, hash) = token::mint();
    let now = Timestamp::now();
    let ttl_days = state.config.gateway.token_ttl_days.max(1);
    let expires_at = now + SignedDuration::from_hours(24 * ttl_days);

    let insert = tokens::insert(
        &state.db,
        &tokens::Token {
            id: Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            name: format!("cli-{}", &cli_state[..8.min(cli_state.len())]),
            hash,
            created_at: now,
            last_used_at: None,
            expires_at,
            revoked_at: None,
            // Tool use is opt-in per token; a fresh CLI token starts with
            // gateway tools off until the owner enables it on /tokens.
            tools_enabled: false,
        },
    )
    .await;
    if let Err(err) = insert {
        tracing::warn!(error = %err, "storing CLI token");
        return error_html(StatusCode::INTERNAL_SERVER_ERROR, "could not store token");
    }

    if let Err(err) = cli_logins::set_token(&state.db, cli_state, &plaintext).await {
        tracing::warn!(error = %err, "storing CLI plaintext");
        return error_html(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not stage plaintext for CLI",
        );
    }

    let html = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Signed in</title>
<style>body{font:14px system-ui;background:#0f1115;color:#e6e8eb;text-align:center;padding-top:6rem}h1{font-weight:600}p{color:#8a93a6}</style>
</head><body>
<h1>You're signed in</h1>
<p>Return to your terminal — the CLI has picked up the token.</p>
<p>You can close this tab.</p>
</body></html>"#;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html.to_string(),
    )
        .into_response()
}

fn redirect_to(url: &str) -> Response {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, url)
        .body("".into())
        .unwrap()
}

fn error_html(status: StatusCode, message: &str) -> Response {
    // Same OpenAI-ish JSON envelope as the proxy routes so monitoring
    // tooling parses both paths uniformly. /auth/* errors aren't really
    // OpenAI-shaped but consistency matters more than realism here.
    let body = json!({
        "error": {
            "message": message,
            "type": "auth_error",
            "code": "auth_error",
        }
    });
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string().into())
        .unwrap()
}
