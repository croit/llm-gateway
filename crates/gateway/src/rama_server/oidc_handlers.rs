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

use crate::rama_server::session::{COOKIE_NAME, read_cookie};
use crate::rama_server::state::RamaState;
use crate::server::db::users;

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
    let return_to = params.return_to.filter(|rt| is_safe_return_to(rt));
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
        .filter(|rt| is_safe_return_to(rt))
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

async fn finish_cli_login(state: &RamaState, cli_state: &str, subject: &str) -> Response {
    use crate::server::db::cli_logins;

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

    // Do NOT mint a token here. Render a consent page instead — the token
    // is only minted when the authenticated user explicitly authorizes
    // (POST /auth/cli/approve). This is what defeats the phishing case:
    // completing OIDC alone (which a tricked user might) no longer hands a
    // token to whoever initiated the CLI flow. The confirmation code lets
    // the user check the prompt matches the code shown in *their* terminal.
    let email = users::find_by_id(&state.db, subject)
        .await
        .ok()
        .flatten()
        .map(|u| u.email)
        .unwrap_or_else(|| subject.to_string());
    let code = shared::cli_login_code(cli_state);
    let approval =
        crate::rama_server::cli_handlers::approval_token(&state.sessions, cli_state, subject);

    let html = CLI_CONSENT_HTML
        .replace("%%EMAIL%%", &escape_html(&email))
        .replace("%%CODE%%", &escape_html(&code))
        .replace("%%APPROVAL%%", &escape_html(&approval));
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

/// Consent page shown after a successful OIDC login that was started by
/// the CLI. `%%EMAIL%%` / `%%CODE%%` / `%%APPROVAL%%` are substituted
/// (HTML-escaped) per request. Both buttons POST the signed approval token
/// so the server can act without a browser session.
const CLI_CONSENT_HTML: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Authorize CLI sign-in</title>
<style>
 body{font:15px/1.5 system-ui;background:#0f1115;color:#e6e8eb;display:flex;min-height:100vh;margin:0;align-items:center;justify-content:center}
 .card{max-width:30rem;padding:2rem;border:1px solid #2a2f3a;border-radius:12px;background:#151923}
 h1{font-size:1.2rem;margin:0 0 .5rem}
 .code{font:600 1.6rem ui-monospace,monospace;letter-spacing:.15em;background:#0f1115;border:1px solid #2a2f3a;border-radius:8px;padding:.6rem 1rem;text-align:center;margin:1rem 0}
 .warn{color:#f0b429;font-size:.85rem}
 .muted{color:#8a93a6;font-size:.85rem}
 .row{display:flex;gap:.75rem;margin-top:1.5rem}
 .row form{flex:1;margin:0}
 button{width:100%;padding:.6rem 1rem;border-radius:8px;border:0;font:600 1rem system-ui;cursor:pointer}
 .approve{background:#3b82f6;color:#fff}
 .deny{background:#2a2f3a;color:#e6e8eb}
</style></head><body>
<div class="card">
 <h1>Authorize command-line sign-in</h1>
 <p>A command-line application is requesting an API token for <strong>%%EMAIL%%</strong>.</p>
 <p class="muted">Confirm this matches the code shown in your terminal:</p>
 <div class="code">%%CODE%%</div>
 <p class="warn">⚠ Only authorize if you just ran <code>gw auth login</code> yourself and the code matches. If you didn't start this, click Deny.</p>
 <div class="row">
  <form method="post" action="/auth/cli/approve">
    <input type="hidden" name="approval" value="%%APPROVAL%%">
    <button class="approve" type="submit">Authorize</button>
  </form>
  <form method="post" action="/auth/cli/deny">
    <input type="hidden" name="approval" value="%%APPROVAL%%">
    <button class="deny" type="submit">Deny</button>
  </form>
 </div>
</div>
</body></html>"#;

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
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

/// True if `p` is a safe *same-origin* redirect target. `starts_with('/')`
/// is not enough: `//evil.com` and `/\evil.com` are protocol-relative URLs
/// (browsers normalise `\`→`/`), so a naive check would let a post-login
/// redirect bounce the user to an attacker's host. Leading ASCII
/// whitespace is trimmed first, since browsers strip it before resolving.
pub(crate) fn is_safe_return_to(p: &str) -> bool {
    let p = p.trim_start_matches(|c: char| c.is_ascii_whitespace());
    p.starts_with('/') && !p.starts_with("//") && !p.starts_with("/\\")
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

#[cfg(test)]
mod tests {
    use super::is_safe_return_to;

    #[test]
    fn accepts_local_paths() {
        assert!(is_safe_return_to("/"));
        assert!(is_safe_return_to("/chat/abc"));
        assert!(is_safe_return_to("/chat/shared-1?x=1"));
    }

    #[test]
    fn rejects_protocol_relative_and_absolute_urls() {
        // The whole point of the guard: these all pass `starts_with('/')`
        // (or look local) yet resolve off-origin in a browser.
        assert!(!is_safe_return_to("//evil.com"));
        assert!(!is_safe_return_to("/\\evil.com"));
        assert!(!is_safe_return_to("https://evil.com"));
        assert!(!is_safe_return_to("http://evil.com"));
        assert!(!is_safe_return_to("evil.com"));
        assert!(!is_safe_return_to(""));
    }

    #[test]
    fn rejects_whitespace_smuggled_protocol_relative() {
        // Browsers strip leading whitespace before resolving, so we must
        // trim before checking, else `\t//evil.com` would slip through.
        assert!(!is_safe_return_to("  //evil.com"));
        assert!(!is_safe_return_to("\t//evil.com"));
        assert!(!is_safe_return_to("\n//evil.com"));
    }
}
