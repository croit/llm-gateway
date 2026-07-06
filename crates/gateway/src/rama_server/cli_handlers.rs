// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/auth/cli/*` — CLI loopback-via-polling endpoints for `gw auth login`.
//!
//! Ported from `crate::server::api::auth_cli`. The session-bag write
//! that the axum side did (`session.insert(CLI_STATE, …)`) becomes a
//! redirect query parameter — the rama `/auth/login` handler already
//! threads `cli_state` through `pending_logins` so the callback can
//! finish the flow.

use std::sync::Arc;

use jiff::Timestamp;
use rama::http::service::web::extract::{Query, State};
use rama::http::service::web::response::IntoResponse;
use rama::http::{Request, Response, StatusCode, header};
use rand::TryRngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Digest;

use crate::rama_server::session::SessionStore;
use crate::rama_server::state::RamaState;
use crate::server::db::cli_logins;

const CLI_STATE_TTL_SECS: i64 = 5 * 60;
const CLI_STATE_HEX_LEN: usize = 32;

#[derive(Deserialize)]
pub struct StartRequest {
    pub pkce_challenge: String,
}

#[derive(Serialize)]
pub struct StartResponse {
    pub state: String,
    pub login_url: String,
}

/// POST /auth/cli/start — initial handshake. Body is JSON.
pub async fn start(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return json_error(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };
    let body: StartRequest = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(err) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                &format!("body is not a StartRequest: {err}"),
            );
        }
    };
    if body.pkce_challenge.is_empty() || body.pkce_challenge.len() > 256 {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "pkce_challenge must be a non-empty base64url-encoded SHA-256",
        );
    }

    let cli_state = random_hex(CLI_STATE_HEX_LEN);
    let now = Timestamp::now();
    let row = cli_logins::CliLogin {
        state: cli_state.clone(),
        pkce_challenge: body.pkce_challenge,
        token_plain: None,
        expires_at: now + jiff::SignedDuration::from_secs(CLI_STATE_TTL_SECS),
        created_at: now,
    };
    if let Err(err) = cli_logins::insert(&state.db, &row).await {
        tracing::warn!(error = %err, "storing cli_login");
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "storing cli_login failed",
        );
    }

    let public = state.config.gateway.public_url.trim_end_matches('/');
    let login_url = format!("{public}/auth/cli/begin?state={cli_state}");
    json_ok(&StartResponse {
        state: cli_state,
        login_url,
    })
}

#[derive(Deserialize)]
pub struct BeginParams {
    pub state: String,
}

/// GET /auth/cli/begin — browser entry point. Validates the state row
/// is still in-flight, then 303s to /auth/login with `cli_state` set
/// so the rama /auth/login handler can persist it into pending_logins.
pub async fn begin(
    State(state): State<Arc<RamaState>>,
    Query(params): Query<BeginParams>,
) -> Response {
    let row = match cli_logins::find(&state.db, &params.state).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return text_response(
                StatusCode::BAD_REQUEST,
                "unknown or expired CLI login state — re-run `gw auth login`",
            );
        }
        Err(err) => {
            tracing::warn!(error = %err, "cli_login lookup");
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "cli_login lookup failed");
        }
    };
    if row.expires_at < Timestamp::now() {
        return text_response(
            StatusCode::BAD_REQUEST,
            "CLI login state has expired — re-run `gw auth login`",
        );
    }
    let target = format!("/auth/login?cli_state={}", params.state);
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, target)
        .body("".into())
        .unwrap()
}

#[derive(Deserialize)]
pub struct PollRequest {
    pub state: String,
    pub pkce_verifier: String,
}

#[derive(Serialize)]
pub struct PollResponse {
    pub token: String,
}

/// POST /auth/cli/poll — CLI polls here. Validates PKCE; returns the
/// freshly-minted token on success, 204 if the browser hasn't finished
/// yet, 401 otherwise.
pub async fn poll(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return json_error(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };
    let body: PollRequest = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(err) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                &format!("body is not a PollRequest: {err}"),
            );
        }
    };

    let row = match cli_logins::find(&state.db, &body.state).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return json_error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "unknown cli login state",
            );
        }
        Err(err) => {
            tracing::warn!(error = %err, "cli_login lookup");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "cli_login lookup failed",
            );
        }
    };
    if row.expires_at < Timestamp::now() {
        let _ = cli_logins::delete(&state.db, &body.state).await;
        return json_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "cli login expired",
        );
    }

    let computed = pkce_s256_challenge(&body.pkce_verifier);
    if !constant_time_eq(computed.as_bytes(), row.pkce_challenge.as_bytes()) {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "pkce verifier mismatch",
        );
    }

    match row.token_plain {
        Some(plaintext) => {
            let pool = state.db.clone();
            let st = body.state.clone();
            tokio::spawn(async move {
                let _ = cli_logins::delete(&pool, &st).await;
            });
            json_ok(&PollResponse { token: plaintext })
        }
        None => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body("".into())
            .unwrap(),
    }
}

#[derive(Deserialize)]
pub struct ApprovalForm {
    pub approval: String,
}

/// Mint a tamper-proof approval token binding a `cli_state` to the OIDC
/// `subject` that just authenticated. HMAC-signed with the gateway secret
/// (via the session signer), so `/auth/cli/approve` can trust the subject
/// without a DB column or a browser session. Only `finish_cli_login` —
/// which runs *after* a verified OIDC login — ever issues one.
pub(crate) fn approval_token(sessions: &SessionStore, cli_state: &str, subject: &str) -> String {
    // Payload `<cli_state>:<hex(subject)>`: cli_state is hex and the
    // subject is hex-encoded, so the payload is `.`-free (the signer
    // appends `.<sig>`) and splits cleanly on `:`.
    let payload = format!("{cli_state}:{}", hex_encode(subject.as_bytes()));
    sessions.sign(&payload)
}

/// Inverse of [`approval_token`]: verify the HMAC and recover
/// `(cli_state, subject)`. `None` on any tampering or malformed input.
pub(crate) fn parse_approval(sessions: &SessionStore, token: &str) -> Option<(String, String)> {
    let payload = sessions.verify(token).ok()?;
    let (cli_state, subject_hex) = payload.split_once(':')?;
    let subject = String::from_utf8(hex_decode(subject_hex)?).ok()?;
    Some((cli_state.to_string(), subject))
}

/// POST /auth/cli/approve — the user clicked "Authorize" on the consent
/// page. This is the ONLY place a CLI token is minted: the OIDC callback
/// no longer stages one, so a phished victim who merely completes the
/// browser login (without consciously approving) never yields a token.
pub async fn approve(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    use crate::server::auth::token;
    use crate::server::db::tokens;
    use uuid::Uuid;

    let (_, body) = req.into_parts();
    let bytes = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return text_response(StatusCode::BAD_REQUEST, &msg),
    };
    let form: ApprovalForm = match serde_urlencoded::from_bytes(&bytes) {
        Ok(f) => f,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "malformed approval form"),
    };
    let Some((cli_state, subject)) = parse_approval(&state.sessions, &form.approval) else {
        return text_response(StatusCode::BAD_REQUEST, "invalid or tampered approval");
    };

    // The row must still be in-flight.
    let row = match cli_logins::find(&state.db, &cli_state).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return text_response(
                StatusCode::BAD_REQUEST,
                "CLI login expired — re-run `gw auth login`",
            );
        }
        Err(err) => {
            tracing::warn!(error = %err, "cli_login lookup");
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "cli_login lookup failed");
        }
    };
    if row.expires_at < Timestamp::now() {
        return text_response(
            StatusCode::BAD_REQUEST,
            "CLI login expired — re-run `gw auth login`",
        );
    }
    // Idempotent: already approved (double-submit / refresh) → success,
    // never mint a second token.
    if row.token_plain.is_some() {
        return approved_page();
    }

    let (plaintext, hash) = token::mint();
    let now = Timestamp::now();
    let ttl_days = state.config.gateway.token_ttl_days.max(1);
    let expires_at = now + jiff::SignedDuration::from_hours(24 * ttl_days);
    let insert = tokens::insert(
        &state.db,
        &tokens::Token {
            id: Uuid::new_v4().to_string(),
            user_id: subject,
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
        return text_response(StatusCode::INTERNAL_SERVER_ERROR, "could not store token");
    }
    if let Err(err) = cli_logins::set_token(&state.db, &cli_state, &plaintext).await {
        tracing::warn!(error = %err, "staging CLI plaintext");
        return text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not stage token for CLI",
        );
    }
    approved_page()
}

/// POST /auth/cli/deny — the user rejected the consent page. Drops the
/// in-flight row so the polling CLI fails fast instead of timing out.
pub async fn deny(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (_, body) = req.into_parts();
    let bytes = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return text_response(StatusCode::BAD_REQUEST, &msg),
    };
    let form: ApprovalForm = match serde_urlencoded::from_bytes(&bytes) {
        Ok(f) => f,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "malformed approval form"),
    };
    if let Some((cli_state, _subject)) = parse_approval(&state.sessions, &form.approval) {
        let _ = cli_logins::delete(&state.db, &cli_state).await;
    }
    text_response(
        StatusCode::OK,
        "CLI sign-in denied. You can close this tab and return to your terminal.",
    )
}

fn approved_page() -> Response {
    let html = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Authorized</title>
<style>body{font:14px system-ui;background:#0f1115;color:#e6e8eb;text-align:center;padding-top:6rem}h1{font-weight:600}p{color:#8a93a6}</style>
</head><body>
<h1>Command-line sign-in authorized</h1>
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

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        out.push((val(pair[0])? << 4) | val(pair[1])?);
    }
    Some(out)
}

// ---------- helpers (same as the axum side) ----------

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .expect("OS RNG must succeed");
    let mut out = String::with_capacity(bytes * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in &buf {
        out.push(HEX[(*b >> 4) as usize] as char);
        out.push(HEX[(*b & 0x0f) as usize] as char);
    }
    out
}

fn pkce_s256_challenge(verifier: &str) -> String {
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    base64url_no_pad(&digest)
}

fn base64url_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() >= 2 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() >= 3 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn json_ok<T: serde::Serialize>(value: &T) -> Response {
    let body = serde_json::to_string(value).expect("serialize");
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn json_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({
        "error": {
            "message": message,
            "type": code,
            "code": code,
        }
    });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn text_response(status: StatusCode, body: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body.to_string(),
    )
        .into_response()
}

async fn read_body_to_bytes(body: rama::http::Body) -> Result<rama::bytes::Bytes, String> {
    use rama::http::body::util::BodyExt;
    body.collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| format!("reading body: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_rfc7636_example() {
        let challenge = pkce_s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn random_hex_correct_length() {
        let s = random_hex(16);
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
