// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/auth/login`, `/auth/callback`, `/auth/logout` for the rama server.
//!
//! Re-implements the axum version (`gateway_core::server::api::auth`) on top of
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
use rama::http::{Request, Response, StatusCode, header};
use serde::Deserialize;
use serde_json::json;

use gateway_core::rama_server::session::{COOKIE_NAME, read_cookie};
use gateway_core::server::db::users;
use gateway_runtime::rama_server::state::RamaState;

/// TTL for the in-flight `pending_logins` row. Generous because some
/// IdPs (Authentik, Keycloak's account-linking flows) bounce the user
/// through several screens before redirecting back.
const PENDING_LOGIN_TTL: SignedDuration = SignedDuration::from_mins(15);

/// Cookie that binds an in-flight OIDC login to the browser that started
/// it. Set at `/auth/login`, and required to match the `state` parameter
/// at `/auth/callback`. Without it, the `state` row alone doesn't prove
/// the callback arrived in the same browser, opening login CSRF / session
/// fixation. Scoped to `Path=/auth` so it only rides the login routes.
const OIDC_BINDING_COOKIE: &str = "gw_oidc";

#[derive(Deserialize)]
pub struct LoginParams {
    /// Where to send the user after login. Optional; defaults to `/`.
    pub return_to: Option<String>,
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
    let return_to = params
        .return_to
        .filter(|rt| gateway_core::rama_server::session::is_safe_return_to(rt));

    let res = sqlx::query(
        "INSERT INTO pending_logins
           (state, pkce_verifier, nonce, return_to, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&start.csrf)
    .bind(&start.pkce_verifier)
    .bind(&start.nonce)
    .bind(return_to.as_deref())
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

    // Bind the flow to this browser (see OIDC_BINDING_COOKIE): the
    // callback must echo `state` back via this cookie or we reject it.
    redirect_to_with_binding(&start.url, &start.csrf)
}

#[derive(Deserialize, Default)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// GET /auth/callback — receives the IdP's redirect. Takes the whole
/// `Request` (not a `Query` extractor) so it can also read the
/// browser-binding cookie set by `/auth/login`.
pub async fn callback(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let params: CallbackParams = req
        .uri()
        .query()
        .and_then(|q| serde_urlencoded::from_str::<CallbackParams>(q).ok())
        .unwrap_or_default();
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

    // Bind the callback to the browser that began login. `state` is
    // unguessable + single-use, but on its own it does not prove the
    // callback landed in the same browser — without this a phisher could
    // feed a victim their own `code`+`state` and silently log the victim
    // into the *attacker's* account (login CSRF / session fixation).
    if read_cookie(req.headers(), OIDC_BINDING_COOKIE).as_deref() != Some(state_param.as_str()) {
        return error_html(
            StatusCode::BAD_REQUEST,
            "OIDC callback was not initiated by this browser — restart at /auth/login",
        );
    }

    // Pull the in-flight row. Missing → either expired, already consumed,
    // or never started — all "go back to /auth/login" from the user POV.
    type PendingRow = (String, String, Option<String>, String);
    let pending: Option<PendingRow> = match sqlx::query_as(
        "SELECT pkce_verifier, nonce, return_to, expires_at
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
    let Some((verifier, nonce, return_to, expires_at_raw)) = pending else {
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
            // Same for the picked TTS voice: chosen in the chat header, not by
            // the identity provider, and outside `upsert`'s column list.
            speech_voice: None,
        },
    )
    .await
    {
        tracing::warn!(error = %err, "user upsert");
        return error_html(StatusCode::INTERNAL_SERVER_ERROR, "could not persist user");
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
        .filter(|rt| gateway_core::rama_server::session::is_safe_return_to(rt))
        .unwrap_or_else(|| "/chat".into());
    // Flow complete — clear the login-binding cookie.
    let clear_binding =
        format!("{OIDC_BINDING_COOKIE}=; Path=/auth; HttpOnly; SameSite=Lax; Max-Age=0");
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, target)
        .header(header::SET_COOKIE, cookie)
        .header(header::SET_COOKIE, clear_binding)
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

/// 303 to the IdP while dropping the browser-binding cookie (see
/// [`OIDC_BINDING_COOKIE`]). `state` is the CSRF/state value the callback
/// will later require this cookie to match.
fn redirect_to_with_binding(url: &str, state: &str) -> Response {
    // Path=/auth confines the cookie to the login + callback routes;
    // Max-Age=900 matches PENDING_LOGIN_TTL (15 min). HttpOnly keeps it
    // away from JS; SameSite=Lax is sufficient because the value must
    // still equal `state` at the callback.
    let binding =
        format!("{OIDC_BINDING_COOKIE}={state}; Path=/auth; HttpOnly; SameSite=Lax; Max-Age=900");
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, url)
        .header(header::SET_COOKIE, binding)
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
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string().into())
        .unwrap()
}
