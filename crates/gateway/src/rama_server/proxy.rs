// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Rama-side handlers for the OpenAI-compatible proxy routes.
//!
//! Reuses the `UpstreamRegistry` from `crate::server::upstreams` verbatim
//! — that module has no axum coupling. Only the request/response edges are
//! rewritten for rama's body model.
//!
//! The header policy mirrors `crate::server::api::proxy`: client
//! `Authorization` is stripped, upstream `api_key_env` is injected,
//! hop-by-hop headers are filtered both directions.

use std::collections::BTreeMap;
use std::sync::Arc;

use rama::bytes::Bytes;
use rama::futures::channel::mpsc;
use rama::futures::stream;
use rama::http::service::web::extract::State;
use rama::http::service::web::response::IntoResponse;
use rama::http::{HeaderMap, HeaderName, Method, Request, Response, StatusCode};
use serde_json::{Value, json};

use std::time::Instant;

use jiff::Timestamp;

use crate::rama_server::auth::require_bearer;
use crate::rama_server::state::RamaState;
use crate::rama_server::vad;
use crate::server::auth::UserCtx;
use crate::server::db::usage::{self, UsageKind, UsageRecord, UsageSource};
use crate::server::tools::ToolContext;
use crate::server::tools::runner::{self, LoopError};
use crate::server::upstreams::registry::{Acquired, RouteError};
use crate::server::upstreams::{AcquireError, PoolKind};
use crate::server::usage::UsageHandle;

/// Identity + classification for a usage measurement, built once per
/// request and finished off (backend, status, latency, tokens) at each
/// upstream call. The `model` is carried here because the byte-dumb
/// `forward`/`forward_streaming` helpers are generic over the path.
#[derive(Clone)]
struct RecordParams {
    user_id: String,
    user_email: String,
    token_id: Option<String>,
    token_name: Option<String>,
    source: UsageSource,
    kind: UsageKind,
    model: String,
}

impl RecordParams {
    /// A `/v1` (bearer) measurement: the access method is `v1_api` and the
    /// token id/name carry through for the per-token breakdown.
    fn v1(user: &UserCtx, kind: UsageKind, model: String) -> Self {
        Self {
            user_id: user.user_id.clone(),
            user_email: user.user_email.clone(),
            token_id: Some(user.token_id.clone()),
            token_name: Some(user.token_name.clone()),
            source: UsageSource::V1Api,
            kind,
            model,
        }
    }

    /// Finish the measurement and hand it to the (fire-and-forget) sink.
    /// `tokens` is `(prompt, completion, total)` — any may be `None` when
    /// the upstream didn't report usage.
    fn emit(
        &self,
        sink: &UsageHandle,
        backend: &str,
        status: u16,
        started: Instant,
        tokens: (Option<i64>, Option<i64>, Option<i64>),
    ) {
        let (prompt_tokens, completion_tokens, total_tokens) = tokens;
        sink.emit(UsageRecord {
            created_at: Timestamp::now(),
            user_id: self.user_id.clone(),
            user_email: Some(self.user_email.clone()).filter(|s| !s.is_empty()),
            token_id: self.token_id.clone(),
            token_name: self.token_name.clone(),
            source: self.source,
            kind: self.kind,
            backend: backend.to_string(),
            model: self.model.clone(),
            status,
            duration_ms: started.elapsed().as_millis() as i64,
            prompt_tokens,
            completion_tokens,
            total_tokens,
        });
    }
}

/// Token counts from a buffered JSON body (or `(None, None, None)` if it
/// doesn't parse / carries no `usage`).
fn tokens_from_bytes(bytes: &Bytes) -> (Option<i64>, Option<i64>, Option<i64>) {
    serde_json::from_slice::<Value>(bytes)
        .map(|v| usage::usage_from_value(&v))
        .unwrap_or((None, None, None))
}

/// `POST /v1/chat/completions`. Two paths under one handler:
///
/// * **Byte-dumb proxy** — forwards the upstream response 1:1 to the
///   client (streaming or not). Used only when the user has no gateway
///   tool grants at all: there's nothing to inject, so we just route
///   bytes (and never intercept a client's own tool loop).
/// * **Gateway-tool path** — taken whenever the user has gateway tool
///   grants, *including* when the client also brought its own `tools`
///   array. `runner::inject_tools` unions the gateway definitions into
///   the client's set (de-duped by name), then we either stream (via
///   `forward_streaming_with_tools`) or buffer (via
///   `runner::run_with_tools`). Both flavours intercept gateway-owned
///   `tool_calls`, run the tool server-side, and continue the loop —
///   the client never sees those calls. Client-owned `tool_calls` are
///   passed straight through so the client keeps driving its own tools;
///   a turn that mixes both yields back to the client (see
///   `run_with_tools` / `drive_streaming_tool_loop` for the why).
///
/// Responses on the buffered gateway-tool path carry an
/// `x-gateway-tool-rounds` header so operators can tell at a glance
/// whether the loop fired.
pub async fn chat_completions(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    // Source IP for `get_user_location`: proxy header (behind a load
    // balancer) first, else the direct TCP socket peer. Captured before
    // we split the request so the socket extension is still reachable.
    let client_ip = crate::server::geoip::client_ip(req.headers())
        .or_else(|| crate::server::geoip::peer_ip(&req));
    let (parts, body) = req.into_parts();
    let user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };
    let body = strip_stream_options_when_not_streaming(body);
    let Some(model) = parse_model_field(&body) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request body is missing a string `model` field",
        );
    };

    // Per-token resolution: RBAC − the user's global /tools toggles −
    // this token's disabled capabilities, gated behind the token's master
    // "tool use" switch (off by default → empty → byte-dumb passthrough
    // below). One call covers buffered, streaming, and passthrough.
    let allowed_tools = state.allowed_tools_for_token(&user).await;

    // Apply admin-configured sampling defaults for the named model
    // before either branch consumes the body. Client keys win
    // (`apply_defaults` only fills in *missing* top-level fields);
    // bad stored TOML / DB hiccups log + pass the original bytes
    // through. Cheap when no row exists (one indexed lookup).
    let body =
        crate::server::model_defaults::apply_defaults_to_bytes(&state.db, &model, body).await;

    // Byte-dumb proxy: only when the user has no gateway tool grants.
    // There's nothing to inject, so route bytes 1:1 and leave any
    // client-driven tool loop untouched. When the user *does* have
    // grants we fall through to the gateway-tool path even if the
    // client brought its own `tools` — `inject_tools` unions ours in
    // and the loop runs gateway-owned calls while passing client-owned
    // ones through.
    if allowed_tools.is_empty() {
        // `acquire_for` returns a structured `RouteError`, so `route_error_
        // response` maps an unknown model straight to 404 `model_not_found`
        // (and known-but-down to 503) — no pre-check needed on this path.
        let acquired = match state.upstreams.acquire_for(&model, PoolKind::Chat) {
            Ok(a) => a,
            Err(e) => return route_error_response(e),
        };
        let rec = RecordParams::v1(&user, UsageKind::Chat, model.clone());
        return forward_streaming(
            &state,
            acquired,
            Method::POST,
            "chat/completions",
            parts.headers,
            body,
            rec,
        )
        .await;
    }

    // Gateway-tool path. Unlike the byte-dumb path above, the tool loops
    // flatten an `acquire_for` error into `LoopError::Upstream` (→ 503), so
    // they can't tell an unknown model from a transient outage. Pre-check
    // here so they return the OpenAI 404 too. Health-agnostic: a *known*
    // model whose replicas are all down still falls through to 503 at
    // `acquire_for` inside the loop.
    if !state.upstreams.knows_model(&model, PoolKind::Chat) {
        return model_not_found_response(&model);
    }

    // Gateway-tool path: inject definitions, then run either the
    // streaming intercept or the buffered runner. Defaults are
    // already merged into `body` above.
    let request_body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                &format!("body is not valid JSON: {err}"),
            );
        }
    };

    let wants_streaming = request_body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    if wants_streaming {
        return forward_streaming_with_tools(
            state.clone(),
            user.clone(),
            model.clone(),
            parts.headers.clone(),
            client_ip.clone(),
            request_body,
            allowed_tools,
        )
        .await;
    }
    let tool_ctx = ToolContext {
        user_id: user.user_id.clone(),
        roles: user.roles.clone(),
        db: state.db.clone(),
        s3: state
            .config
            .chat
            .s3
            .as_ref()
            .map(|cfg| std::sync::Arc::new(cfg.clone())),
        // No persistent chat turn on the proxy paths.
        assistant_turn_id: None,
        session_id: None,
        client_ip: client_ip.clone(),
        geoip: state.geoip.clone(),
        // No live turn / browser to prompt on the proxy paths.
        chat_feedback: None,
        // No turn → nothing to reserve filenames against. The
        // upload tools refuse to run here anyway (they require
        // `assistant_turn_id`), so this branch never fires.
        attachment_reservations: None,
        indexer: state.indexer.clone(),
    };
    let state_clone = state.clone();
    let model_clone = model.clone();
    let headers_clone = parts.headers.clone();
    // One usage row per upstream round — built per request, finished off
    // with backend/status/latency/tokens inside the loop closure.
    let rec = RecordParams::v1(&user, UsageKind::Chat, model.clone());

    let outcome = runner::run_with_tools(
        &state.tools,
        &allowed_tools,
        &tool_ctx,
        request_body,
        move |body_value| {
            let state = state_clone.clone();
            let model = model_clone.clone();
            let headers = headers_clone.clone();
            let rec = rec.clone();
            async move {
                let acquired = state
                    .upstreams
                    .acquire_for(&model, PoolKind::Chat)
                    .map_err(|e| LoopError::Upstream(e.to_string()))?;
                let backend_name = acquired.backend().name.clone();
                let started = Instant::now();
                let serialized = serde_json::to_vec(&body_value)
                    .map_err(|e| LoopError::Upstream(format!("serialise: {e}")))?;
                let url = format!("{}/chat/completions", acquired.backend().base_url);
                let mut http = state.http.post(&url);
                for (name, value) in &headers {
                    if is_request_header_forwarded(name) {
                        http = http.header(name.as_str(), value);
                    }
                }
                http = http.header("content-type", "application/json");
                if let Some(key) = acquired.backend().api_key.as_deref() {
                    http = http.bearer_auth(key);
                }
                // A backend was contacted, so the call is counted either way
                // — a transport/read failure records a 502 row (parallel to
                // `forward`), keeping error accounting consistent across paths.
                let resp = match http.body(serialized).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        drop(acquired);
                        rec.emit(
                            &state.usage,
                            &backend_name,
                            StatusCode::BAD_GATEWAY.as_u16(),
                            started,
                            (None, None, None),
                        );
                        return Err(LoopError::Upstream(e.to_string()));
                    }
                };
                let status = resp.status().as_u16();
                let bytes = match resp.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        drop(acquired);
                        rec.emit(
                            &state.usage,
                            &backend_name,
                            StatusCode::BAD_GATEWAY.as_u16(),
                            started,
                            (None, None, None),
                        );
                        return Err(LoopError::Upstream(e.to_string()));
                    }
                };
                drop(acquired);
                rec.emit(
                    &state.usage,
                    &backend_name,
                    status,
                    started,
                    tokens_from_bytes(&bytes),
                );
                Ok((status, bytes))
            }
        },
    )
    .await;

    let outcome = match outcome {
        Ok(o) => o,
        Err(err) => return loop_error_response(err),
    };

    Response::builder()
        .status(StatusCode::from_u16(outcome.status).unwrap_or(StatusCode::OK))
        .header(rama::http::header::CONTENT_TYPE, "application/json")
        .header("x-gateway-tool-rounds", outcome.rounds.to_string())
        .body(outcome.body.into())
        .unwrap_or_else(|err| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &format!("building response: {err}"),
            )
        })
}

fn loop_error_response(err: LoopError) -> Response {
    match err {
        LoopError::MalformedRequest(m) => {
            error_response(StatusCode::BAD_REQUEST, "invalid_request", &m)
        }
        LoopError::Upstream(m) => {
            error_response(StatusCode::SERVICE_UNAVAILABLE, "upstream_unreachable", &m)
        }
        LoopError::MalformedUpstream(m) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("upstream returned unparseable JSON: {m}"),
        ),
        LoopError::LoopExhausted(n) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("tool-call loop exhausted after {n} rounds"),
        ),
    }
}

/// Buffered passthrough for `POST /v1/audio/transcriptions`. Parses the
/// multipart body, runs the `file` part through `vad::trim_silence`
/// (silence trimming before Whisper, see `vad.rs` for the why), then
/// rebuilds the multipart body with a fresh boundary before forwarding
/// upstream.
pub async fn transcribe(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };
    // Model is parsed inside; `handle_transcription` fills it into `rec`.
    let rec = RecordParams::v1(&user, UsageKind::Transcription, String::new());
    handle_transcription(state, parts.headers, body, rec).await
}

/// `POST /api/v0/transcriptions` — session-authed mirror of
/// `/v1/audio/transcriptions` for the in-browser voice composer. Same
/// multipart shape; auth is the signed session cookie instead of a
/// bearer. Returns the upstream JSON (`{"text": "…"}`) verbatim so
/// `app.js` can drop it into the chat textarea.
pub async fn transcribe_session(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let session = match state.sessions.lookup_from_headers(&parts.headers).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "no active session — sign in at /auth/login",
            );
        }
        Err(err) => {
            tracing::warn!(error = %err, "session lookup");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "session lookup failed",
            );
        }
    };
    // Browser-composer transcription is part of chat-UI usage: source
    // `chat`, no API token. Email is best-effort (one indexed read) so the
    // per-user breakdown reads nicely; user_id is always present. Skipped
    // when metrics are disabled (no extra DB read on the kill-switched path).
    let user_email = if state.usage.is_enabled() {
        crate::server::db::users::find_by_id(&state.db, &session.user_id)
            .await
            .ok()
            .flatten()
            .map(|u| u.email)
            .unwrap_or_default()
    } else {
        String::new()
    };
    let rec = RecordParams {
        user_id: session.user_id.clone(),
        user_email,
        token_id: None,
        token_name: None,
        source: UsageSource::Chat,
        kind: UsageKind::Transcription,
        model: String::new(),
    };
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };
    handle_transcription(state, parts.headers, body, rec).await
}

/// Shared body of both transcription handlers: parse → VAD-trim → rebuild
/// multipart → forward. Pulled out because the bearer/session paths
/// only differ in auth.
async fn handle_transcription(
    state: Arc<RamaState>,
    mut headers: HeaderMap,
    body: Bytes,
    mut rec: RecordParams,
) -> Response {
    let fields = match parse_multipart_fields(&headers, body).await {
        Ok(f) => f,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };
    let Some(model) = fields
        .iter()
        .find(|f| f.name == "model")
        .and_then(|f| std::str::from_utf8(&f.bytes).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "multipart body missing required `model` field",
        );
    };

    let trimmed_fields = trim_audio_field(fields);

    // Sub-threshold recording guard. Voxtral (and the other
    // realtime audio LLMs we serve) embed audio at ~25 tokens/s,
    // so anything below a couple hundred milliseconds either
    // produces zero embeddings outright (`Realtime model received
    // empty multimodal embeddings for 1 input tokens` in voxtral's
    // log — repeated thousands of times as the decode loop wedges)
    // or a token sequence too short to anchor meaningful output.
    // Reject in the gateway so the upstream never gets to spin its
    // wheels on a stray mis-click. 0.4 s is the floor; even a clipped
    // single-word utterance ("hi", "yes") comfortably clears that.
    //
    // Only enforces the floor for recordings we *can* measure (16 kHz
    // mono PCM-16 — the format the browser worklet emits and the
    // only one VAD accepts). API clients submitting other formats
    // bypass this check; their failure mode is bounded by the
    // upstream's own validation.
    const MIN_AUDIO_SECONDS: f64 = 0.4;
    if let Some(file) = trimmed_fields.iter().find(|f| f.name == "file")
        && let Some(secs) = vad::pcm16_mono_16k_duration_seconds(&file.bytes)
        && secs < MIN_AUDIO_SECONDS
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "audio_too_short",
            "Recording too short — speak for at least half a second.",
        );
    }

    let (new_body, content_type) = match build_multipart(&trimmed_fields) {
        Ok(v) => v,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };

    // Replace the inbound `Content-Type` so the boundary in the body
    // matches what we just generated. `Content-Length` is in the
    // request-header denylist (rebuilt-body length differs) so we don't
    // need to touch it.
    headers.remove(rama::http::header::CONTENT_TYPE);
    if let Ok(val) = rama::http::HeaderValue::from_str(&content_type) {
        headers.insert(rama::http::header::CONTENT_TYPE, val);
    }

    let acquired = match state.upstreams.acquire_for(&model, PoolKind::Transcription) {
        Ok(a) => a,
        Err(e) => return route_error_response(e),
    };
    rec.model = model;
    forward(
        &state,
        acquired,
        Method::POST,
        "audio/transcriptions",
        headers,
        new_body,
        rec,
    )
    .await
}

/// A single parsed multipart field. We hold everything in memory — the
/// existing handler already buffered the whole body to extract `model`,
/// so this just makes the same buffering reusable for the rebuild.
struct MultipartField {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    bytes: Bytes,
}

async fn parse_multipart_fields(
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Vec<MultipartField>, String> {
    let ct = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            "missing Content-Type; transcription requires multipart/form-data".to_string()
        })?;
    let boundary = multer::parse_boundary(ct)
        .map_err(|e| format!("Content-Type is not a multipart/form-data: {e}"))?;
    let stream_once = stream::once(async move { Ok::<_, std::io::Error>(body) });
    let mut mp = multer::Multipart::new(stream_once, boundary);
    let mut fields = Vec::new();
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| format!("malformed multipart: {e}"))?
    {
        let name = field.name().unwrap_or("").to_string();
        let filename = field.file_name().map(str::to_owned);
        let content_type = field.content_type().map(|m| m.essence_str().to_string());
        let bytes = field
            .bytes()
            .await
            .map_err(|e| format!("reading multipart field `{name}`: {e}"))?;
        fields.push(MultipartField {
            name,
            filename,
            content_type,
            bytes,
        });
    }
    Ok(fields)
}

/// If the field set contains a `file` part, run it through VAD trim and
/// overwrite the bytes/filename/content-type with the trimmed WAV.
/// Falls back to the original part on any rejection (wrong format,
/// nothing to trim, pure silence) so this path can't break a
/// transcription that would otherwise have succeeded.
fn trim_audio_field(fields: Vec<MultipartField>) -> Vec<MultipartField> {
    let mut out = Vec::with_capacity(fields.len());
    for mut f in fields {
        if f.name == "file"
            && !f.bytes.is_empty()
            && let Some(trimmed) = vad::trim_silence(&f.bytes)
        {
            f.bytes = trimmed.bytes;
            f.filename = Some(trimmed.filename.to_string());
            f.content_type = Some(trimmed.content_type.to_string());
        }
        out.push(f);
    }
    out
}

/// Serialise a parsed field set back into a multipart body. Returns the
/// body bytes and the matching `Content-Type` header value (boundary
/// included).
fn build_multipart(fields: &[MultipartField]) -> Result<(Bytes, String), String> {
    let boundary = format!("------rama-vad-{}", uuid::Uuid::new_v4().simple());
    let mut out: Vec<u8> = Vec::with_capacity(
        fields
            .iter()
            .map(|f| f.bytes.len() + f.name.len() + 64)
            .sum::<usize>()
            + boundary.len() * (fields.len() + 1),
    );
    for f in fields {
        // multer hands us the field name verbatim; we don't accept
        // arbitrary user input here (the chat composer + the API
        // client are the only writers), so a quote in the name is a
        // bug, not a security concern — reject loudly rather than
        // emit a malformed Content-Disposition.
        if f.name.contains('"') || f.name.contains('\r') || f.name.contains('\n') {
            return Err(format!(
                "multipart field name `{}` contains invalid characters",
                f.name
            ));
        }
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(b"Content-Disposition: form-data; name=\"");
        out.extend_from_slice(f.name.as_bytes());
        out.push(b'"');
        if let Some(fname) = f.filename.as_deref() {
            if fname.contains('"') || fname.contains('\r') || fname.contains('\n') {
                return Err(format!(
                    "multipart filename `{fname}` contains invalid characters"
                ));
            }
            out.extend_from_slice(b"; filename=\"");
            out.extend_from_slice(fname.as_bytes());
            out.push(b'"');
        }
        out.extend_from_slice(b"\r\n");
        if let Some(ct) = f.content_type.as_deref() {
            out.extend_from_slice(b"Content-Type: ");
            out.extend_from_slice(ct.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&f.bytes);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"--\r\n");
    let content_type = format!("multipart/form-data; boundary={boundary}");
    Ok((Bytes::from(out), content_type))
}

/// Drains a rama HTTP body into a single `Bytes`. The upstream relay
/// works on whole buffers right now; SSE streaming will need a different
/// shape that consumes the body progressively.
async fn read_body_to_bytes(body: rama::http::Body) -> Result<Bytes, String> {
    use rama::http::body::util::BodyExt;
    body.collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| format!("reading request body: {e}"))
}

/// `POST /v1/embeddings` — OpenAI-compatible text embeddings. Byte-dumb
/// proxy: authenticate, read the `model`, pick a healthy backend from the
/// **Embedding** pool, and relay the request/response 1:1. No streaming and
/// no tool injection — an embeddings request is a single round-trip. This is
/// a shared embedding surface other services can call instead of wiring their
/// own backend; routing/health/keying all go through the gateway like chat and
/// transcription do.
pub async fn embeddings(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    // Bearer required; no per-model RBAC gate here, matching the chat path.
    let user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };
    let Some(model) = parse_model_field(&body) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request body is missing a string `model` field",
        );
    };
    // `acquire_for` maps an unknown model → 404 `model_not_found` and a
    // known-but-all-down pool → 503, via `route_error_response`.
    let acquired = match state.upstreams.acquire_for(&model, PoolKind::Embedding) {
        Ok(a) => a,
        Err(e) => return route_error_response(e),
    };
    let rec = RecordParams::v1(&user, UsageKind::Embedding, model);
    forward(
        &state,
        acquired,
        Method::POST,
        "embeddings",
        parts.headers,
        body,
        rec,
    )
    .await
}

/// `GET /v1/models` — lists *every* model served by any healthy backend in
/// any pool, de-duplicated by id, in OpenAI envelope shape. Synthesised from
/// the registry's cached model sets (probe-reported, with the configured
/// fallback for backends that don't expose `/models`); no upstream
/// round-trip, no inflight-slot consumption.
///
/// OpenAI parity: capability is *not* a filter here — chat, transcription,
/// embedding models all appear in one flat list, and clients select by id.
/// Bearer auth still required because the model list itself can be sensitive.
pub async fn list_models(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (parts, _body) = req.into_parts();
    let _user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let data: Vec<Value> = state
        .upstreams
        .all_models()
        .into_iter()
        .map(model_object)
        .collect();
    let body = json!({ "object": "list", "data": data });
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// `GET /v1/models/{id}` — retrieve a single model object, or `404
/// model_not_found` if no backend (in any pool, any kind) serves the id.
/// OpenAI exposes this and some clients (incl. the Vercel AI SDK) call it.
/// The route is a `{*id}` catch-all because model ids contain `/`
/// (e.g. `mistralai/Voxtral-Mini-4B-Realtime-2602`).
///
/// We deliberately do *not* use the `Path` extractor: rama's router
/// lowercases the matched path, which would mangle case-sensitive ids like
/// `Qwen/...`. Instead we read the id from the original (case-preserving)
/// request URI and strip the static prefix.
pub async fn retrieve_model(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (parts, _body) = req.into_parts();
    let _user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let raw = parts
        .uri
        .path()
        .strip_prefix("/v1/models/")
        .unwrap_or_default();
    let id = percent_decode(raw);
    if id.is_empty() || !state.upstreams.knows_any(&id) {
        return model_not_found_response(&id);
    }
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        model_object(id).to_string(),
    )
        .into_response()
}

/// Minimal percent-decoder for a path segment. Model ids are sent verbatim
/// (raw `/`) by the clients we care about, but decode anyway so an id a
/// client *did* encode (e.g. `%2F`) still resolves. ASCII-only ids; invalid
/// `%XX` sequences are left as-is, and the result is UTF-8-lossy.
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A full OpenAI model object: `{ id, object:"model", created, owned_by }`.
/// We have no real per-model creation time; clients don't depend on the exact
/// `created` value, only that it's a sane unix-seconds integer.
fn model_object(id: String) -> Value {
    json!({
        "id": id,
        "object": "model",
        "created": jiff::Timestamp::now().as_second(),
        "owned_by": "llm-gateway",
    })
}

/// Forwards a request body to the chosen upstream backend and relays the
/// response. Reads the *full* upstream body into memory before responding
/// — for the SSE-streaming chat path we'll need a different shape (next
/// slice). This works for `/v1/models` and any non-streaming response.
async fn forward(
    state: &RamaState,
    acquired: Acquired,
    method: Method,
    upstream_path: &str,
    client_headers: HeaderMap,
    body: Bytes,
    rec: RecordParams,
) -> Response {
    // Outbound HTTP via reqwest, by design. Rama serves the inbound
    // side; reqwest handles outbound. Same split most rust web
    // projects use. `EasyHttpWebClient::default()` works for the
    // outbound role too, but it needs `rama/rustls,tls` (which pulls
    // aws-lc-sys → cmake) and its concrete type is ugly enough as a
    // struct field that the maintenance cost doesn't pay for itself.
    let backend = acquired.backend();
    let backend_name = backend.name.clone();
    let url = format!("{}/{}", backend.base_url, upstream_path);
    let started = Instant::now();

    let mut req = state.http.request(method, &url);
    for (name, value) in &client_headers {
        if is_request_header_forwarded(name) {
            req = req.header(name.as_str(), value);
        }
    }
    if let Some(key) = backend.api_key.as_deref() {
        req = req.bearer_auth(key);
    }

    let upstream = match req.body(body).send().await {
        Ok(r) => r,
        Err(err) => {
            drop(acquired);
            rec.emit(
                &state.usage,
                &backend_name,
                StatusCode::BAD_GATEWAY.as_u16(),
                started,
                (None, None, None),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                &err.to_string(),
            );
        }
    };
    let status = upstream.status();
    let forwarded_headers: Vec<(HeaderName, _)> = upstream
        .headers()
        .iter()
        .filter(|(n, _)| is_response_header_forwarded(n))
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => {
            drop(acquired);
            rec.emit(
                &state.usage,
                &backend_name,
                StatusCode::BAD_GATEWAY.as_u16(),
                started,
                (None, None, None),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                &err.to_string(),
            );
        }
    };
    drop(acquired);

    // One row per upstream call. Embeddings carry a `usage` block;
    // transcription responses don't, so tokens come back `None` there.
    rec.emit(
        &state.usage,
        &backend_name,
        status.as_u16(),
        started,
        tokens_from_bytes(&bytes),
    );

    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    for (name, value) in forwarded_headers {
        builder = builder.header(name, value);
    }
    builder.body(bytes.into()).unwrap_or_else(|err| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("building response: {err}"),
        )
    })
}

/// SSE chunk emitted when the loop guard stops a degenerate repetition on
/// a streaming proxy response — an OpenAI-shaped error envelope so SDKs
/// surface it rather than silently truncating.
fn loop_error_chunk() -> Bytes {
    Bytes::from(format!(
        "data: {}\n\n",
        json!({"error": {"message": crate::loop_guard::LOOP_MESSAGE, "type": "loop_detected"}})
    ))
}

/// Streaming variant of `forward` — used by /v1/chat/completions so SSE
/// (`stream: true`) responses unfold token-by-token to the client
/// instead of buffering. Relays each upstream frame 1:1 while tapping the
/// deltas through a [`crate::loop_guard::LoopGuard`]; the `Acquired` RAII
/// guard rides along in the relay task so the in-flight slot stays
/// reserved for the stream's lifetime. Same header policy as `forward`.
async fn forward_streaming(
    state: &RamaState,
    acquired: Acquired,
    method: Method,
    upstream_path: &str,
    client_headers: HeaderMap,
    body: Bytes,
    rec: RecordParams,
) -> Response {
    use rama::futures::StreamExt;

    let backend = acquired.backend();
    let backend_name = backend.name.clone();
    let url = format!("{}/{}", backend.base_url, upstream_path);
    let started = Instant::now();

    let mut req = state.http.request(method, &url);
    for (name, value) in &client_headers {
        if is_request_header_forwarded(name) {
            req = req.header(name.as_str(), value);
        }
    }
    if let Some(key) = backend.api_key.as_deref() {
        req = req.bearer_auth(key);
    }

    let upstream = match req.body(body).send().await {
        Ok(r) => r,
        Err(err) => {
            drop(acquired);
            rec.emit(
                &state.usage,
                &backend_name,
                StatusCode::BAD_GATEWAY.as_u16(),
                started,
                (None, None, None),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                &err.to_string(),
            );
        }
    };
    let status = upstream.status();
    let forwarded_headers: Vec<(HeaderName, _)> = upstream
        .headers()
        .iter()
        .filter(|(n, _)| is_response_header_forwarded(n))
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    let usage_sink = state.usage.clone();

    // Relay each upstream SSE frame 1:1, but tap the deltas through a
    // repetition guard in parallel. If the model collapses into a loop we
    // stop relaying, emit a terminating error chunk + [DONE], and close —
    // capping a runaway that would otherwise stream until the token
    // ceiling. The guard is repetition-based only, so a long but
    // progressing answer streams through untouched (legitimate long API
    // requests are never cut short). `acquired` moves into the task so the
    // in-flight slot stays reserved for the stream's lifetime.
    let (tx, rx) = mpsc::unbounded::<Result<Bytes, std::io::Error>>();
    tokio::spawn(async move {
        let _slot = acquired;
        let mut upstream_stream = upstream.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut content_guard = crate::loop_guard::LoopGuard::new();
        let mut reasoning_guard = crate::loop_guard::LoopGuard::new();
        let mut looped = false;
        // Token counts ride the trailing `usage` frame — present only when
        // the *client* asked for `stream_options.include_usage`. We tap it
        // passively (never inject the option) so a passthrough client's
        // stream is unchanged; callers who don't opt in get NULL tokens.
        let mut tokens: (Option<i64>, Option<i64>, Option<i64>) = (None, None, None);
        'frames: while let Some(frame) = upstream_stream.next().await {
            let Ok(frame) = frame else { break };
            // Forward first so the client gets output with no added latency.
            if tx.unbounded_send(Ok(frame.clone())).is_err() {
                // Client disconnected — still record the call (partial).
                rec.emit(&usage_sink, &backend_name, status.as_u16(), started, tokens);
                return;
            }
            buf.extend_from_slice(&frame);
            while let Some(idx) = buf.windows(2).position(|w| w == b"\n\n") {
                let event: Vec<u8> = buf.drain(..idx + 2).collect();
                let event = String::from_utf8_lossy(&event);
                for line in event.lines() {
                    let Some(payload) = line.strip_prefix("data:").map(str::trim_start) else {
                        continue;
                    };
                    if payload == "[DONE]" {
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(payload) else {
                        continue;
                    };
                    if v.get("usage").is_some_and(|u| !u.is_null()) {
                        tokens = usage::usage_from_value(&v);
                    }
                    if let Some(t) = v
                        .pointer("/choices/0/delta/content")
                        .and_then(|c| c.as_str())
                        && content_guard.push(t)
                    {
                        looped = true;
                        break 'frames;
                    }
                    if let Some(t) = v
                        .pointer("/choices/0/delta/reasoning_content")
                        .and_then(|c| c.as_str())
                        .or_else(|| {
                            v.pointer("/choices/0/delta/reasoning")
                                .and_then(|c| c.as_str())
                        })
                        && reasoning_guard.push(t)
                    {
                        looped = true;
                        break 'frames;
                    }
                }
            }
        }
        if looped {
            let _ = tx.unbounded_send(Ok(loop_error_chunk()));
            let _ = tx.unbounded_send(Ok(Bytes::from("data: [DONE]\n\n")));
        }
        // Non-streaming requests also take this path: the upstream replies
        // with one plain JSON body (no `data:` frames), so `buf` holds it
        // whole at the end. If we never saw an SSE usage frame, try parsing
        // that body for the `usage` block. For real SSE streams `buf` is
        // drained frame-by-frame and empty here, so this is a no-op.
        if tokens == (None, None, None)
            && let Ok(v) = serde_json::from_slice::<Value>(&buf)
        {
            tokens = usage::usage_from_value(&v);
        }
        rec.emit(&usage_sink, &backend_name, status.as_u16(), started, tokens);
    });
    let body = rama::http::Body::from_stream(rx);

    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    for (name, value) in forwarded_headers {
        builder = builder.header(name, value);
    }
    builder.body(body).unwrap_or_else(|err| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("building response: {err}"),
        )
    })
}

/// Streaming variant of the tool path. Forwards each upstream SSE
/// chunk to the client *and* accumulates `delta.tool_calls` in
/// parallel; when an upstream round ends, runs gateway-owned tools
/// and re-issues the next round upstream, with the whole thing
/// hidden behind a single client-facing SSE stream.
///
/// Hide policy in the forwarded stream:
///
/// * **Hide** every chunk that carries `delta.tool_calls` — those are
///   gateway-owned tools the client neither defined nor can execute,
///   so leaking them just confuses the SDK.
/// * **Hide** every chunk whose `finish_reason` is `tool_calls`,
///   for the same reason.
/// * **Hide** the upstream `[DONE]` between rounds; emit a single
///   final `[DONE]` once the whole loop terminates.
///
/// Errors mid-stream surface as a `data: {"error": …}` chunk
/// followed by `[DONE]` — we can't change the response status after
/// the headers have shipped.
async fn forward_streaming_with_tools(
    state: Arc<RamaState>,
    user: UserCtx,
    model: String,
    client_headers: HeaderMap,
    client_ip: Option<String>,
    mut request_body: Value,
    allowed_tools: Vec<String>,
) -> Response {
    // Inject gateway tools, force stream:true. `stream_options` can
    // stay (vLLM accepts it with stream:true).
    if let Err(err) = runner::inject_tools(&mut request_body, &state.tools, &allowed_tools) {
        return loop_error_response(err);
    }
    if let Some(obj) = request_body.as_object_mut() {
        obj.insert("stream".into(), Value::Bool(true));
    }

    let tool_ctx = ToolContext {
        user_id: user.user_id.clone(),
        roles: user.roles.clone(),
        db: state.db.clone(),
        s3: state
            .config
            .chat
            .s3
            .as_ref()
            .map(|cfg| std::sync::Arc::new(cfg.clone())),
        // No persistent chat turn on the proxy paths.
        assistant_turn_id: None,
        session_id: None,
        client_ip,
        geoip: state.geoip.clone(),
        // No live turn / browser to prompt on the proxy paths.
        chat_feedback: None,
        // No turn → no reservations needed. See sibling site above.
        attachment_reservations: None,
        indexer: state.indexer.clone(),
    };

    // One usage row per upstream round; built here where the bearer's
    // identity + token are known, finished off inside the loop.
    let rec = RecordParams::v1(&user, UsageKind::Chat, model.clone());

    // rama::futures::channel::mpsc::unbounded matches the pattern used by
    // the chat-page SSE producer (`pages/chat/mod.rs`).
    let (mut tx, rx) = mpsc::unbounded::<Result<Bytes, std::io::Error>>();

    tokio::spawn(async move {
        if let Err(err) = drive_streaming_tool_loop(
            state,
            model,
            request_body,
            client_headers,
            tool_ctx,
            rec,
            &mut tx,
        )
        .await
        {
            let err_chunk = format!(
                "data: {}\n\n",
                json!({"error": {"message": err, "type": "internal_error"}})
            );
            let _ = tx.unbounded_send(Ok(Bytes::from(err_chunk)));
        }
        let _ = tx.unbounded_send(Ok(Bytes::from("data: [DONE]\n\n")));
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(rama::http::header::CONTENT_TYPE, "text/event-stream")
        .header(rama::http::header::CACHE_CONTROL, "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .body(rama::http::Body::from_stream(rx))
        .unwrap_or_else(|err| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &format!("building response: {err}"),
            )
        })
}

// Shared cap (one source of truth) so the streaming proxy, the buffered
// runner, and the chat driver can't drift apart on round limits.
use runner::MAX_TOOL_ROUNDS as STREAM_TOOL_LOOP_MAX_ROUNDS;

#[derive(Default)]
struct StreamToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

/// Top-level envelope fields lifted off the upstream's own SSE chunks so
/// any chunk we synthesize (see [`synth_client_tool_call_chunks`]) carries
/// the same `id` / `created` / `model` / `system_fingerprint` the client
/// has already been seeing this turn. Absorbed field-by-field because not
/// every chunk repeats every field (`system_fingerprint` often rides only
/// the first).
#[derive(Default)]
struct ChunkMeta {
    id: Value,
    created: Value,
    model: Value,
    system_fingerprint: Value,
}

impl ChunkMeta {
    fn absorb(&mut self, chunk: &Value) {
        for (field, slot) in [
            ("id", &mut self.id),
            ("created", &mut self.created),
            ("model", &mut self.model),
            ("system_fingerprint", &mut self.system_fingerprint),
        ] {
            if let Some(v) = chunk.get(field) {
                *slot = v.clone();
            }
        }
    }

    fn envelope(&self, choices: Value) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "system_fingerprint": self.system_fingerprint,
            "choices": choices,
        })
    }
}

/// Re-materialise the tool_calls we suppressed during streaming as the SSE
/// the client would have seen had the gateway not been intercepting: one
/// assistant delta carrying every accumulated call (arguments already
/// complete), then a `finish_reason: "tool_calls"` chunk. Used when a turn
/// carries a client-owned tool_call — the client must run its tools and
/// re-submit, so it needs the full turn back. Both `data:` frames are
/// returned ready to send; the caller appends the terminating `[DONE]`.
fn synth_client_tool_call_chunks(
    meta: &ChunkMeta,
    tool_acc: &BTreeMap<usize, StreamToolCallAcc>,
) -> Vec<Bytes> {
    let tool_calls: Vec<Value> = tool_acc
        .iter()
        .map(|(index, acc)| {
            json!({
                "index": index,
                "id": acc.id,
                "type": "function",
                "function": {"name": acc.name, "arguments": acc.arguments},
            })
        })
        .collect();

    let delta = meta.envelope(json!([{
        "index": 0,
        "delta": {"role": "assistant", "content": Value::Null, "tool_calls": tool_calls},
        "finish_reason": Value::Null,
    }]));
    let finish = meta.envelope(json!([{
        "index": 0,
        "delta": {},
        "finish_reason": "tool_calls",
    }]));

    vec![
        Bytes::from(format!("data: {delta}\n\n")),
        Bytes::from(format!("data: {finish}\n\n")),
    ]
}

#[allow(clippy::too_many_arguments)]
async fn drive_streaming_tool_loop(
    state: Arc<RamaState>,
    model: String,
    mut request_body: Value,
    client_headers: HeaderMap,
    tool_ctx: ToolContext,
    rec: RecordParams,
    tx: &mut mpsc::UnboundedSender<Result<Bytes, std::io::Error>>,
) -> Result<(), String> {
    use rama::futures::StreamExt;

    for _round in 0..STREAM_TOOL_LOOP_MAX_ROUNDS {
        let acquired = state
            .upstreams
            .acquire_for(&model, PoolKind::Chat)
            .map_err(|e| e.to_string())?;
        let backend_name = acquired.backend().name.clone();
        let started = Instant::now();
        let url = format!("{}/chat/completions", acquired.backend().base_url);
        let serialized = serde_json::to_vec(&request_body).map_err(|e| e.to_string())?;

        let mut http = state
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            // `accept-encoding: identity` prevents reqwest from
            // requesting gzip — a compressed SSE response is
            // buffered until the upstream closes, which kills
            // streaming.
            .header("accept-encoding", "identity")
            .body(serialized);
        for (name, value) in &client_headers {
            if is_request_header_forwarded(name) {
                http = http.header(name.as_str(), value);
            }
        }
        if let Some(key) = acquired.backend().api_key.as_deref() {
            http = http.bearer_auth(key);
        }

        let upstream = match http.send().await {
            Ok(u) => u,
            Err(e) => {
                drop(acquired);
                rec.emit(
                    &state.usage,
                    &backend_name,
                    StatusCode::BAD_GATEWAY.as_u16(),
                    started,
                    (None, None, None),
                );
                return Err(e.to_string());
            }
        };
        if !upstream.status().is_success() {
            let status = upstream.status();
            let bytes = upstream.bytes().await.unwrap_or_default();
            drop(acquired);
            rec.emit(
                &state.usage,
                &backend_name,
                status.as_u16(),
                started,
                (None, None, None),
            );
            return Err(format!(
                "upstream {status}: {}",
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(500)
                    .collect::<String>()
            ));
        }
        let status_code = upstream.status().as_u16();

        let mut tool_acc: BTreeMap<usize, StreamToolCallAcc> = BTreeMap::new();
        let mut chunk_meta = ChunkMeta::default();
        let mut byte_buf: Vec<u8> = Vec::new();
        // Per-round repetition guards. A degenerate loop in either channel
        // ends the stream cleanly (error chunk + [DONE]) instead of running
        // until the token ceiling. Repetition-based only, so a long but
        // progressing answer is never cut short.
        let mut content_guard = crate::loop_guard::LoopGuard::new();
        let mut reasoning_guard = crate::loop_guard::LoopGuard::new();
        // Token counts ride the trailing `usage` frame when the client opted
        // into `stream_options.include_usage` (we never inject it on /v1).
        let mut round_tokens: (Option<i64>, Option<i64>, Option<i64>) = (None, None, None);
        let mut sse = upstream.bytes_stream();

        while let Some(chunk) = sse.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            byte_buf.extend_from_slice(&chunk);

            // SSE events are separated by `\n\n`. Parse each complete
            // event out of the buffer; whatever's left is a partial
            // event for the next chunk to extend.
            while let Some(idx) = byte_buf.windows(2).position(|w| w == b"\n\n") {
                let event_bytes: Vec<u8> = byte_buf.drain(..idx + 2).collect();
                let event_str = String::from_utf8_lossy(&event_bytes);

                let mut is_done = false;
                let mut hide_event = false;

                for line in event_str.lines() {
                    let Some(payload) = line.strip_prefix("data:").map(str::trim_start) else {
                        continue;
                    };
                    if payload == "[DONE]" {
                        is_done = true;
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(payload) else {
                        continue;
                    };
                    chunk_meta.absorb(&v);
                    if v.get("usage").is_some_and(|u| !u.is_null()) {
                        round_tokens = usage::usage_from_value(&v);
                    }
                    if let Some(t) = v
                        .pointer("/choices/0/delta/content")
                        .and_then(|c| c.as_str())
                        && content_guard.push(t)
                    {
                        return Err(crate::loop_guard::LOOP_MESSAGE.to_string());
                    }
                    if let Some(t) = v
                        .pointer("/choices/0/delta/reasoning_content")
                        .and_then(|c| c.as_str())
                        .or_else(|| {
                            v.pointer("/choices/0/delta/reasoning")
                                .and_then(|c| c.as_str())
                        })
                        && reasoning_guard.push(t)
                    {
                        return Err(crate::loop_guard::LOOP_MESSAGE.to_string());
                    }
                    if let Some(tcs) = v
                        .pointer("/choices/0/delta/tool_calls")
                        .and_then(|t| t.as_array())
                    {
                        hide_event = true;
                        for tc in tcs {
                            let index =
                                tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                            let entry = tool_acc.entry(index).or_default();
                            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                entry.id = id.to_string();
                            }
                            if let Some(name) =
                                tc.pointer("/function/name").and_then(|n| n.as_str())
                            {
                                entry.name = name.to_string();
                            }
                            if let Some(args) =
                                tc.pointer("/function/arguments").and_then(|a| a.as_str())
                            {
                                entry.arguments.push_str(args);
                            }
                        }
                    }
                    if let Some(fr) = v
                        .pointer("/choices/0/finish_reason")
                        .and_then(|f| f.as_str())
                        && fr == "tool_calls"
                    {
                        hide_event = true;
                    }
                }

                if is_done || hide_event {
                    continue;
                }
                tx.unbounded_send(Ok(Bytes::from(event_bytes)))
                    .map_err(|e| format!("client disconnected: {e}"))?;
            }
        }
        rec.emit(
            &state.usage,
            &backend_name,
            status_code,
            started,
            round_tokens,
        );
        drop(acquired);

        if tool_acc.is_empty() {
            // Model finished without tool calls — final round.
            return Ok(());
        }

        // A turn that calls any tool we don't own (the client's own tool,
        // or a hallucinated name) goes back to the client — same rule as
        // the buffered path: the client owns the message history here, so
        // it must run its tools and re-submit. Re-emit the tool_calls we
        // hid during streaming as one synthesized assistant delta + a
        // `finish_reason:"tool_calls"` chunk so the client sees the whole
        // turn, then end the stream (the caller appends `[DONE]`).
        let has_client_owned = tool_acc
            .values()
            .any(|acc| !acc.name.is_empty() && !state.tools.contains(&acc.name));
        if has_client_owned {
            for chunk in synth_client_tool_call_chunks(&chunk_meta, &tool_acc) {
                tx.unbounded_send(Ok(chunk))
                    .map_err(|e| format!("client disconnected: {e}"))?;
            }
            return Ok(());
        }

        let gateway_owned: Vec<runner::ToolCallRef> = tool_acc
            .values()
            .filter(|acc| state.tools.contains(&acc.name))
            .map(|acc| runner::ToolCallRef {
                id: acc.id.clone(),
                name: acc.name.clone(),
                arguments_raw: acc.arguments.clone(),
            })
            .collect();

        if gateway_owned.is_empty() {
            // Only unnamed/garbage tool-call fragments survived — nothing
            // to run and nothing the client needs. End cleanly rather than
            // loop with empty tool results.
            return Ok(());
        }

        let results = runner::execute_tool_calls(&state.tools, &tool_ctx, &gateway_owned).await;

        // No client-owned calls here (handled above), so every accumulated
        // call is gateway-owned — build the assistant turn straight off
        // `gateway_owned` to keep each tool_call paired with its result.
        let assistant_tool_calls: Vec<Value> = gateway_owned
            .iter()
            .map(|call| {
                json!({
                    "id": call.id.clone(),
                    "type": "function",
                    "function": {
                        "name": call.name.clone(),
                        "arguments": call.arguments_raw.clone(),
                    }
                })
            })
            .collect();

        let messages = request_body
            .get_mut("messages")
            .and_then(|m| m.as_array_mut())
            .ok_or_else(|| "request body missing messages array".to_string())?;
        messages.push(json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": assistant_tool_calls,
        }));
        for (call, result) in gateway_owned.iter().zip(results.iter()) {
            let output_str =
                serde_json::to_string(&result.body).unwrap_or_else(|_| "{}".to_string());
            messages.push(json!({
                "role": "tool",
                "tool_call_id": &call.id,
                "content": output_str,
            }));
        }
    }

    Err(format!(
        "tool-call loop exhausted after {STREAM_TOOL_LOOP_MAX_ROUNDS} rounds"
    ))
}

/// vLLM hard-rejects `stream_options` when `stream` isn't `true`
/// (`Value error, Stream options can only be defined when stream=True`).
/// OpenAI's reference accepts the combination as a no-op, and some
/// SDKs — notably `@ai-sdk/openai` — always include `stream_options`
/// even for non-streaming calls. Strip the field defensively at the
/// gateway boundary so those clients work against vLLM-backed pools.
///
/// The string-scan fast-path keeps the cost at ~one memchr-style
/// pass for the (overwhelming) majority of requests that don't
/// carry the field.
fn strip_stream_options_when_not_streaming(body: Bytes) -> Bytes {
    const NEEDLE: &[u8] = b"stream_options";
    if !body.windows(NEEDLE.len()).any(|w| w == NEEDLE) {
        return body;
    }
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&body) else {
        // Malformed JSON — let the downstream parser surface the
        // error instead of swallowing it here.
        return body;
    };
    let streaming = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    if streaming {
        return body;
    }
    if let Some(obj) = v.as_object_mut() {
        obj.remove("stream_options");
    }
    serde_json::to_vec(&v).map(Bytes::from).unwrap_or(body)
}

/// Pulls a string `model` field out of a JSON body without deserialising
/// the rest of the (model-specific, possibly very large) payload.
fn parse_model_field(body: &Bytes) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model")?.as_str().map(str::to_owned)
}

fn route_error_response(err: RouteError) -> Response {
    match err {
        // No backend serves this id at all → OpenAI's 404 `model_not_found`,
        // not a transient 5xx. (The chat path also pre-checks `knows_model`
        // so the tool branches surface this too; this arm covers the
        // byte-dumb path and the transcription handler.)
        RouteError::UnknownModel(m) => model_not_found_response(&m),
        RouteError::Acquire(AcquireError::NoHealthyBackend { pool }) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_unreachable",
            &format!("no healthy backend in `{pool}`"),
        ),
        RouteError::Acquire(AcquireError::Saturated { pool }) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_unreachable",
            &format!("`{pool}` is saturated"),
        ),
    }
}

/// OpenAI's `404 model_not_found` for a model no backend serves. Distinct
/// shape from `error_response`: `type` is `invalid_request_error` and it
/// carries `param: "model"`, matching OpenAI exactly so clients (incl. the
/// Vercel AI SDK) treat it as a request error, not a retryable 5xx.
fn model_not_found_response(model: &str) -> Response {
    let body = json!({
        "error": {
            "message": format!(
                "The model `{model}` does not exist or you do not have access to it."
            ),
            "type": "invalid_request_error",
            "param": "model",
            "code": "model_not_found",
        }
    });
    (
        StatusCode::NOT_FOUND,
        [("content-type", "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// OpenAI-shaped error envelope. Matches the axum side so existing
/// clients don't need to special-case the rama path.
fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({
        "error": {
            "message": message,
            "type": code,
            "code": code,
        }
    });
    (
        status,
        [("content-type", "application/json")],
        body.to_string(),
    )
        .into_response()
}

const REQUEST_HEADER_DENYLIST: &[&str] = &[
    "authorization",
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "expect",
];

const RESPONSE_HEADER_DENYLIST: &[&str] = &[
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn is_request_header_forwarded(name: &HeaderName) -> bool {
    !REQUEST_HEADER_DENYLIST
        .iter()
        .any(|n| n.eq_ignore_ascii_case(name.as_str()))
}

fn is_response_header_forwarded(name: &HeaderName) -> bool {
    !RESPONSE_HEADER_DENYLIST
        .iter()
        .any(|n| n.eq_ignore_ascii_case(name.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &Bytes) -> serde_json::Value {
        serde_json::from_slice(body).unwrap()
    }

    #[test]
    fn percent_decode_passes_through_unencoded() {
        // Raw `/` (the form real clients send) is untouched.
        assert_eq!(
            percent_decode("mistralai/Voxtral-Mini-4B-Realtime-2602"),
            "mistralai/Voxtral-Mini-4B-Realtime-2602"
        );
    }

    #[test]
    fn percent_decode_decodes_encoded_slash_preserving_case() {
        assert_eq!(
            percent_decode("Qwen%2FQwen3.6-35B-A3B-FP8"),
            "Qwen/Qwen3.6-35B-A3B-FP8"
        );
        // Lowercase hex digits too.
        assert_eq!(percent_decode("a%2fb"), "a/b");
    }

    #[test]
    fn percent_decode_leaves_truncated_sequences_alone() {
        assert_eq!(percent_decode("ends-with-%2"), "ends-with-%2");
        assert_eq!(percent_decode("bad-%zz-seq"), "bad-%zz-seq");
    }

    #[test]
    fn model_not_found_response_is_openai_shaped_404() {
        let resp = model_not_found_response("ghost-model");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn strip_drops_stream_options_when_stream_missing() {
        let body =
            Bytes::from(r#"{"model":"m","messages":[],"stream_options":{"include_usage":true}}"#);
        let out = strip_stream_options_when_not_streaming(body);
        let v = parse(&out);
        assert!(v.get("stream_options").is_none());
        assert_eq!(v["model"], "m");
    }

    #[test]
    fn strip_drops_stream_options_when_stream_false() {
        let body =
            Bytes::from(r#"{"model":"m","stream":false,"stream_options":{"include_usage":true}}"#);
        let out = strip_stream_options_when_not_streaming(body);
        let v = parse(&out);
        assert!(v.get("stream_options").is_none());
        assert_eq!(v["stream"], false);
    }

    #[test]
    fn strip_keeps_stream_options_when_stream_true() {
        let body =
            Bytes::from(r#"{"model":"m","stream":true,"stream_options":{"include_usage":true}}"#);
        let out = strip_stream_options_when_not_streaming(body);
        let v = parse(&out);
        assert!(v.get("stream_options").is_some());
        assert_eq!(v["stream"], true);
    }

    #[test]
    fn strip_passes_through_when_field_absent() {
        // Fast-path: same Bytes instance returned (no parse, no realloc).
        let body = Bytes::from(r#"{"model":"m","messages":[]}"#);
        let ptr_before = body.as_ptr();
        let out = strip_stream_options_when_not_streaming(body);
        assert_eq!(out.as_ptr(), ptr_before);
    }

    #[test]
    fn strip_passes_malformed_json_through_unchanged() {
        // Garbage body should reach the downstream parser, not get
        // silently rewritten here. (The needle still has to match —
        // otherwise we hit the fast path.)
        let body = Bytes::from(r#"{ not valid json stream_options here"#);
        let out = strip_stream_options_when_not_streaming(body.clone());
        assert_eq!(&out[..], &body[..]);
    }

    #[test]
    fn chunk_meta_absorbs_envelope_fields_field_by_field() {
        let mut meta = ChunkMeta::default();
        // First chunk carries id + system_fingerprint; later chunks drop
        // the fingerprint but keep id. Absorb must retain each field's
        // last seen value rather than clobbering with the missing ones.
        meta.absorb(&serde_json::json!({
            "id": "chatcmpl-1", "created": 100, "model": "qwen",
            "system_fingerprint": "fp_x"
        }));
        meta.absorb(&serde_json::json!({"id": "chatcmpl-1", "created": 100, "model": "qwen"}));
        let env = meta.envelope(serde_json::json!([]));
        assert_eq!(env["id"], "chatcmpl-1");
        assert_eq!(env["system_fingerprint"], "fp_x");
        assert_eq!(env["object"], "chat.completion.chunk");
    }

    #[test]
    fn synth_client_tool_call_chunks_reemits_full_turn() {
        let mut meta = ChunkMeta::default();
        meta.absorb(&serde_json::json!({"id": "chatcmpl-9", "created": 1, "model": "m"}));
        let mut acc = BTreeMap::new();
        acc.insert(
            0,
            StreamToolCallAcc {
                id: "call_a".into(),
                name: "client_tool".into(),
                arguments: r#"{"x":1}"#.into(),
            },
        );
        let chunks = synth_client_tool_call_chunks(&meta, &acc);
        assert_eq!(chunks.len(), 2);

        // First frame: assistant delta carrying the complete tool_call.
        let first = String::from_utf8(chunks[0].to_vec()).unwrap();
        let payload = first.strip_prefix("data: ").unwrap().trim_end();
        let v: Value = serde_json::from_str(payload).unwrap();
        let tc = &v["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["id"], "call_a");
        assert_eq!(tc["function"]["name"], "client_tool");
        assert_eq!(tc["function"]["arguments"], r#"{"x":1}"#);
        assert!(v["choices"][0]["finish_reason"].is_null());

        // Second frame: the finish_reason terminator the client expects.
        let second = String::from_utf8(chunks[1].to_vec()).unwrap();
        let payload = second.strip_prefix("data: ").unwrap().trim_end();
        let v: Value = serde_json::from_str(payload).unwrap();
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    }
}
