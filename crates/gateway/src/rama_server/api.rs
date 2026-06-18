// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/api/v0/*` — session-authenticated endpoints used by the web UI.
//!
//! Ported from `crate::server::api::tokens` and `chat`. Same wire shapes
//! (the wire types live in `shared::api`); the only difference is the
//! auth boundary: tower-sessions → our hand-rolled `SessionStore`, and
//! axum extractors → rama `Request` + path/query extractors.
//!
//! Endpoints:
//!   - GET /api/v0/me                       → user identity + role grants
//!   - GET /api/v0/tokens                   → list caller's tokens
//!   - POST /api/v0/tokens                  → mint a new token
//!   - DELETE /api/v0/tokens/{id}           → hard-delete a revoked token
//!   - POST /api/v0/tokens/{id}/revoke      → revoke an active token
//!
//! Chat / transcription / models session mirrors will land alongside
//! the UI port — they only matter once the rama-side UI is hitting them.

use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};
use rama::http::service::web::extract::{Path, State};
use rama::http::service::web::response::IntoResponse;
use rama::http::{Request, Response, StatusCode, header};
use serde_json::json;
use shared::api::{
    CreateTokenRequest, CreateTokenResponse, DeleteResponse, Me, RevokeResponse, TokenSummary,
};
use uuid::Uuid;

use crate::rama_server::session::Session;
use crate::rama_server::state::RamaState;
use crate::server::auth::token;
use crate::server::db::{tokens, users};

// ---------------------------------------------------------------------------
// Session gate

/// Pull the signed session cookie off the request and resolve the user.
/// `Err(Response)` is the 401-with-OpenAI-envelope a missing/expired
/// session produces; callers `return` it directly.
async fn require_session(state: &RamaState, req: &Request) -> Result<Session, Response> {
    match state.sessions.lookup_from_headers(req.headers()).await {
        Ok(Some(session)) => Ok(session),
        Ok(None) => Err(unauthorized("no active session — sign in at /auth/login")),
        Err(err) => {
            tracing::warn!(error = %err, "session lookup");
            Err(internal_error("session lookup failed"))
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers

/// GET /api/v0/me — caller identity, role IDs after RBAC mapping, and the
/// set of tools their roles grant. The web UI uses the `allowed_tools`
/// field to render a "what can I do" panel.
pub async fn me(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let session = match require_session(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let user = match users::find_by_id(&state.db, &session.user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            tracing::warn!(user_id = %session.user_id, "session references missing user");
            return internal_error("session references missing user");
        }
        Err(err) => {
            tracing::warn!(error = %err, "user lookup");
            return internal_error("user lookup failed");
        }
    };
    let role_ids = state.rbac.role_ids_for(&user.roles);
    let allowed_tool_ids = state.rbac.allowed_tools(&role_ids, &state.tools);
    let allowed_tools = state.tools.summaries_for(&allowed_tool_ids);
    json_ok(&Me {
        id: user.id,
        email: user.email,
        name: user.name,
        roles: user.roles,
        role_ids,
        allowed_tools,
    })
}

/// GET /api/v0/tokens — list the caller's tokens (no hashes, no plaintext).
pub async fn list_tokens(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let session = match require_session(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let list = match tokens::list_for_user(&state.db, &session.user_id).await {
        Ok(l) => l,
        Err(err) => {
            tracing::warn!(error = %err, "listing tokens");
            return internal_error("listing tokens failed");
        }
    };
    let out: Vec<TokenSummary> = list.into_iter().map(to_summary).collect();
    json_ok(&out)
}

/// POST /api/v0/tokens — mint a new bearer. Plaintext returned **once**.
pub async fn create_token(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let session = match require_session(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let body_bytes = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return invalid_request(&msg),
    };
    let body: CreateTokenRequest = match serde_json::from_slice(&body_bytes) {
        Ok(b) => b,
        Err(err) => return invalid_request(&format!("body is not a CreateTokenRequest: {err}")),
    };

    let name = body.name.trim();
    if name.is_empty() || name.len() > 128 {
        return invalid_request("token name must be 1..=128 characters");
    }
    let ttl_days = body
        .ttl_days
        .unwrap_or(state.config.gateway.token_ttl_days)
        .clamp(1, 365 * 5);

    let now = Timestamp::now();
    let expires_at = now + SignedDuration::from_hours(24 * ttl_days);
    let (plaintext, hash) = token::mint();
    let row = tokens::Token {
        id: Uuid::new_v4().to_string(),
        user_id: session.user_id.clone(),
        name: name.to_string(),
        hash,
        created_at: now,
        last_used_at: None,
        expires_at,
        revoked_at: None,
    };
    if let Err(err) = tokens::insert(&state.db, &row).await {
        tracing::warn!(error = %err, "storing token");
        return internal_error("storing token failed");
    }
    json_ok(&CreateTokenResponse {
        token: to_summary(row),
        plaintext,
    })
}

/// POST /api/v0/tokens/{id}/revoke — flip `revoked_at` on an owned active row.
pub async fn revoke_token(
    State(state): State<Arc<RamaState>>,
    Path(token_id): Path<String>,
    req: Request,
) -> Response {
    let session = match require_session(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let revoked = match tokens::revoke(&state.db, &session.user_id, &token_id).await {
        Ok(b) => b,
        Err(err) => {
            tracing::warn!(error = %err, %token_id, "revoke token");
            return internal_error("revoke failed");
        }
    };
    json_ok(&RevokeResponse { revoked })
}

/// GET /api/v0/transcription_models — names of every `[[models]]`
/// rule whose pool is a `PoolKind::Transcription`. The chat composer
/// fetches this on render to populate the voice-model dropdown; the
/// list is empty when no transcription pool is configured (and the UI
/// hides the mic button in that case).
pub async fn transcription_models(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_session(&state, &req).await {
        return resp;
    }
    let models = state
        .upstreams
        .models_for_kind(crate::server::upstreams::PoolKind::Transcription);
    json_ok(&json!({ "data": models }))
}

/// POST /api/v0/me/timezone — store the caller's IANA timezone on
/// their session + user row. Posted from `app.js` once per page load
/// after reading `Intl.DateTimeFormat().resolvedOptions().timeZone`.
/// Body: `{ "timezone": "Europe/Berlin" }`.
///
/// Validates the IANA name via `jiff::tz::TimeZone::get` so we don't
/// persist garbage. We update *both* the session row (per-device
/// scope) and the user row (fallback for bearer-authed callers that
/// never have a session) — tools that care about wall-clock time
/// read from the user row.
pub async fn set_timezone(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    use jiff::tz::TimeZone;

    let session = match require_session(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return invalid_request(&msg),
    };
    #[derive(serde::Deserialize)]
    struct Body {
        timezone: String,
    }
    let parsed: Body = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(err) => return invalid_request(&format!("expected {{\"timezone\":\"…\"}}: {err}")),
    };
    if TimeZone::get(&parsed.timezone).is_err() {
        return invalid_request(&format!(
            "`{}` is not a known IANA timezone",
            parsed.timezone
        ));
    }
    if let Err(err) = state
        .sessions
        .set_timezone(&session.id, &parsed.timezone)
        .await
    {
        tracing::warn!(error = %err, "session set_timezone");
        return internal_error("could not save timezone");
    }
    if let Err(err) = users::set_timezone(&state.db, &session.user_id, &parsed.timezone).await {
        tracing::warn!(error = %err, "users set_timezone");
        return internal_error("could not save timezone");
    }
    json_ok(&json!({ "ok": true, "timezone": parsed.timezone }))
}

/// POST /api/v0/me/location — store the caller's browser-reported
/// position on their user row. Posted from `geo.ts` once
/// `navigator.geolocation.getCurrentPosition` resolves (the `/tools`
/// "share location" button, or the chat feedback-loop prompt). Body:
/// `{ "lat": 52.52, "lon": 13.405, "accuracy": 25.0 }` — `accuracy`
/// (metres) optional. The `get_user_location` tool reads it back,
/// preferring a fresh fix over coarse GeoIP.
pub async fn set_location(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let session = match require_session(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return invalid_request(&msg),
    };
    #[derive(serde::Deserialize)]
    struct Body {
        lat: f64,
        lon: f64,
        #[serde(default)]
        accuracy: Option<f64>,
    }
    let parsed: Body = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(err) => return invalid_request(&format!("expected {{\"lat\":…,\"lon\":…}}: {err}")),
    };
    let accuracy = match validate_lat_lon(parsed.lat, parsed.lon, parsed.accuracy) {
        Ok(a) => a,
        Err(msg) => return invalid_request(msg),
    };
    if let Err(err) = users::set_location(
        &state.db,
        &session.user_id,
        parsed.lat,
        parsed.lon,
        accuracy,
    )
    .await
    {
        tracing::warn!(error = %err, "users set_location");
        return internal_error("could not save location");
    }
    json_ok(&json!({ "ok": true }))
}

/// DELETE /api/v0/me/location — forget the caller's stored position (the
/// "stop sharing" affordance on `/tools`).
pub async fn clear_location(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let session = match require_session(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Err(err) = users::clear_location(&state.db, &session.user_id).await {
        tracing::warn!(error = %err, "users clear_location");
        return internal_error("could not clear location");
    }
    json_ok(&json!({ "ok": true }))
}

/// POST /api/v0/me/location/feedback/{turn_id} — reply to an in-flight
/// `get_user_location` prompt for assistant turn `turn_id`. Posted by
/// `geo.ts` when the user clicks "share" (body `{lat, lon, accuracy}`)
/// or "not now" (body `{ "denied": true }`) on the prompt the tool
/// injected. Resolves the parked tool via the feedback hub; a shared
/// position is also persisted so the next turn skips the prompt.
pub async fn location_feedback(
    State(state): State<Arc<RamaState>>,
    Path(turn_id): Path<String>,
    req: Request,
) -> Response {
    use crate::server::tools::feedback::BrowserFix;

    let session = match require_session(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return invalid_request(&msg),
    };
    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(default)]
        lat: Option<f64>,
        #[serde(default)]
        lon: Option<f64>,
        #[serde(default)]
        accuracy: Option<f64>,
        #[serde(default)]
        denied: bool,
    }
    let parsed: Body = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(err) => {
            return invalid_request(&format!(
                "expected a position or {{\"denied\":true}}: {err}"
            ));
        }
    };

    let fix = if parsed.denied {
        BrowserFix::Declined
    } else {
        let (Some(lat), Some(lon)) = (parsed.lat, parsed.lon) else {
            return invalid_request("need {lat, lon} or {\"denied\": true}");
        };
        let accuracy = match validate_lat_lon(lat, lon, parsed.accuracy) {
            Ok(a) => a,
            Err(msg) => return invalid_request(msg),
        };
        // Persist so a follow-up turn within the freshness window reuses
        // it without re-prompting.
        if let Err(err) = users::set_location(&state.db, &session.user_id, lat, lon, accuracy).await
        {
            tracing::warn!(error = %err, "location_feedback set_location");
        }
        BrowserFix::Position { lat, lon, accuracy }
    };
    // Whoever's parked on this turn (if anyone — the tool may have timed
    // out) gets the reply. We don't treat "no one waiting" as an error.
    state.location_feedback.resolve(&turn_id, fix);
    json_ok(&json!({ "ok": true }))
}

/// DELETE /api/v0/tokens/{id} — hard-delete an already-revoked row.
/// Active tokens have to be revoked first (DB layer enforces).
pub async fn delete_token(
    State(state): State<Arc<RamaState>>,
    Path(token_id): Path<String>,
    req: Request,
) -> Response {
    let session = match require_session(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let deleted = match tokens::delete_if_revoked(&state.db, &session.user_id, &token_id).await {
        Ok(b) => b,
        Err(err) => {
            tracing::warn!(error = %err, %token_id, "delete token");
            return internal_error("delete failed");
        }
    };
    json_ok(&DeleteResponse { deleted })
}

// ---------------------------------------------------------------------------
// Shared helpers (response builders + utilities)

fn to_summary(t: tokens::Token) -> TokenSummary {
    TokenSummary {
        id: t.id,
        name: t.name,
        created_at: t.created_at,
        last_used_at: t.last_used_at,
        expires_at: t.expires_at,
        revoked: t.revoked_at.is_some(),
    }
}

/// Validate a browser-reported lat/lon pair. On success returns a
/// sanitised `accuracy` (a NaN/negative one is dropped — the position is
/// still usable without it); on failure, the message naming the bad
/// field. Shared by `set_location` and `location_feedback`.
fn validate_lat_lon(
    lat: f64,
    lon: f64,
    accuracy: Option<f64>,
) -> Result<Option<f64>, &'static str> {
    if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
        return Err("lat must be a number between -90 and 90");
    }
    if !lon.is_finite() || !(-180.0..=180.0).contains(&lon) {
        return Err("lon must be a number between -180 and 180");
    }
    Ok(accuracy.filter(|a| a.is_finite() && *a >= 0.0))
}

fn json_ok<T: serde::Serialize>(value: &T) -> Response {
    let body = match serde_json::to_string(value) {
        Ok(s) => s,
        Err(err) => return internal_error(&format!("serialising response: {err}")),
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn invalid_request(message: &str) -> Response {
    error_envelope(StatusCode::BAD_REQUEST, "invalid_request", message)
}

fn unauthorized(message: &str) -> Response {
    error_envelope(StatusCode::UNAUTHORIZED, "unauthorized", message)
}

fn internal_error(message: &str) -> Response {
    error_envelope(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
}

fn error_envelope(status: StatusCode, code: &str, message: &str) -> Response {
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

async fn read_body_to_bytes(body: rama::http::Body) -> Result<rama::bytes::Bytes, String> {
    use rama::http::body::util::BodyExt;
    body.collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| format!("reading request body: {e}"))
}
