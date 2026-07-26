// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Rama-side handlers for the OpenAI-compatible proxy routes.
//!
//! Reuses the `UpstreamRegistry` from `gateway_core::server::upstreams` verbatim
//! — that module has no axum coupling. Only the request/response edges are
//! rewritten for rama's body model.
//!
//! The header policy mirrors `gateway_core::server::api::proxy`: client
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

use crate::rama_server::vad;
use gateway_core::rama_server::auth::require_bearer;
use gateway_core::rama_server::state::RamaState;
use gateway_core::server::auth::UserCtx;
use gateway_core::server::db::usage::{self, UnitUsage, UsageKind, UsageRecord, UsageSource};
use gateway_core::server::speech::{self, SpokenMarkers};
use gateway_core::server::tools::ToolContext;
use gateway_core::server::tools::ToolSource;
use gateway_core::server::tools::runner::ToolCallAcc;
use gateway_core::server::tools::runner::{self, LoopError};
use gateway_core::server::upstreams::registry::{Acquired, RouteError};
use gateway_core::server::upstreams::{AcquireError, PoolKind};
use gateway_core::server::usage::UsageHandle;
use session_core::i18n::{Lang, t};

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
    /// Whether the serving pool is metered (counts toward limits). Resolved
    /// from the registry once the real model id is known, and stamped onto
    /// every emitted row.
    enforce_limits: bool,
    input_units: Option<f64>,
    output_units: Option<f64>,
}

impl RecordParams {
    /// A `/v1` (bearer) measurement: the access method is `v1_api` and the
    /// token id/name carry through for the per-token breakdown. `enforce_limits` is
    /// the serving pool's flag (resolve via `upstreams.enforce_limits_for_model`).
    fn v1(user: &UserCtx, kind: UsageKind, model: String, enforce_limits: bool) -> Self {
        Self {
            user_id: user.user_id.clone(),
            user_email: user.user_email.clone(),
            token_id: Some(user.token_id.clone()),
            token_name: Some(user.token_name.clone()),
            source: UsageSource::V1Api,
            kind,
            model,
            enforce_limits,
            input_units: None,
            output_units: None,
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
            input_units: self.input_units,
            output_units: self.output_units,
            enforce_limits: self.enforce_limits,
        });
    }
}

/// Pre-flight rate-limit / quota gate for a bearer (`/v1`) call. Returns the
/// `429` to send when the caller is over a limit, else `None`. Resolves the
/// caller's role ids and consults the shared [`gateway_core::server::limits::Enforcer`].
async fn limit_check(state: &RamaState, user: &UserCtx) -> Option<Response> {
    let role_ids = state.role_ids_for(&user.roles);
    match state.enforcer.check(&user.user_id, &role_ids).await {
        Ok(()) => None,
        Err(exceeded) => Some(limit_exceeded_response(&exceeded)),
    }
}

/// Compact number for limit messages: whole values without a trailing `.0`,
/// otherwise two decimals (cost).
fn fmt_limit_num(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        format!("{n:.2}")
    }
}

/// A `429 Too Many Requests` with an OpenAI-shaped error envelope and a
/// `Retry-After` header, naming the breached limit.
fn limit_exceeded_response(e: &gateway_core::server::limits::LimitExceeded) -> Response {
    let scope = e
        .model
        .as_deref()
        .map(|m| format!(" for model `{m}`"))
        .unwrap_or_default();
    let msg = format!(
        "{} limit reached{scope}: {} per {} (used {}). Try again later.",
        e.dimension.as_str(),
        fmt_limit_num(e.limit),
        e.window.as_str(),
        fmt_limit_num(e.used),
    );
    let body = json!({
        "error": {
            "message": msg,
            "type": "rate_limit_exceeded",
            "code": "rate_limit_exceeded",
        }
    });
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header(rama::http::header::CONTENT_TYPE, "application/json")
        .header(
            rama::http::header::RETRY_AFTER,
            e.retry_after_secs.to_string(),
        )
        .body(body.to_string().into())
        .unwrap_or_else(|_| {
            error_response(StatusCode::TOO_MANY_REQUESTS, "rate_limit_exceeded", &msg)
        })
}

/// Token counts from a buffered JSON body (or `(None, None, None)` if it
/// doesn't parse / carries no `usage`).
fn tokens_from_bytes(bytes: &Bytes) -> (Option<i64>, Option<i64>, Option<i64>) {
    serde_json::from_slice::<Value>(bytes)
        .map(|v| usage::usage_from_value(&v))
        .unwrap_or((None, None, None))
}

type TokenUsage = (Option<i64>, Option<i64>, Option<i64>);

fn response_metrics(kind: UsageKind, bytes: &Bytes) -> (TokenUsage, UnitUsage) {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return ((None, None, None), UnitUsage::default());
    };
    let mut units = usage::units_from_value(&value);
    if kind == UsageKind::Transcription && units.input.is_none() {
        units.input = value.get("duration").and_then(Value::as_f64);
    }
    if kind == UsageKind::Image {
        units.output =
            gateway_core::server::image_gen::image_output_units_from_value(&value).or(units.output);
    }
    (usage::usage_from_value(&value), units)
}

fn request_units(kind: UsageKind, body: &Bytes) -> UnitUsage {
    match kind {
        UsageKind::Speech => serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("input")
                    .and_then(Value::as_str)
                    .map(|text| UnitUsage {
                        input: Some(text.chars().count() as f64),
                        output: None,
                    })
            })
            .unwrap_or_default(),
        _ => UnitUsage::default(),
    }
}

fn image_edit_units(fields: &[MultipartField]) -> Option<f64> {
    gateway_core::server::image_gen::image_units(
        fields
            .iter()
            .filter(|field| field.name == "image" || field.name == "image[]")
            .count(),
    )
}

fn prefer_positive(primary: Option<f64>, fallback: Option<f64>) -> Option<f64> {
    primary.filter(|value| *value > 0.0).or(fallback)
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
/// Build the per-request [`ToolContext`] for the `/v1` proxy tool loops.
///
/// Unlike the chat-UI driver's `build_tool_context`, the proxy paths have no
/// persisted chat turn — so there is no `session_id`/`assistant_turn_id`, no
/// filename reservation set, and no live browser to prompt (`chat_feedback`).
/// Both the buffered (`chat_completions`) and streaming
/// (`forward_streaming_with_tools`) loops build the context through here so
/// the ~15-field literal lives in exactly one place and can't drift.
fn proxy_tool_ctx(
    state: &Arc<RamaState>,
    user_id: String,
    roles: Vec<String>,
    client_ip: Option<String>,
) -> ToolContext {
    ToolContext {
        user_id,
        roles,
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
        // No turn → nothing to reserve filenames against. The upload tools
        // refuse to run here anyway (they require `assistant_turn_id`).
        attachment_reservations: None,
        indexer: state.indexer.clone(),
        image_gen: Some(gateway_core::server::image_gen::ImageGenerator::new(
            state.upstreams.clone(),
            state.http.clone(),
            state.usage.clone(),
            state.db.clone(),
        )),
        // Per-request sandbox lease (the /v1 loop = one turn), so a client's
        // multi-round `run_in_sandbox` reuses one container. `None` when the
        // sandbox isn't configured. Released when the loop ends (see runner).
        sandbox_lease: state
            .sandbox_client
            .clone()
            .map(gateway_core::server::tools::sandbox::SandboxLease::new),
        crypto: Some(state.crypto.clone()),
    }
}

/// One upstream round for the buffered `/v1` tool loop: acquire a slot for
/// `model`, POST the (already model-rewritten) `body_value`, and return the
/// upstream status + raw bytes. Emits exactly one usage row per attempt —
/// including a 502 row on a transport/read failure, matching `forward` — so
/// error accounting stays consistent across paths. Factored out of the
/// `run_with_tools` closure in [`chat_completions`] so that ~70-line
/// acquire/send/emit dance is named instead of nested three closures deep.
async fn forward_one_round(
    state: &Arc<RamaState>,
    model: &str,
    access: &gateway_core::server::upstreams::PoolAccess,
    headers: &HeaderMap,
    rec: &RecordParams,
    body_value: Value,
) -> Result<(u16, Bytes), LoopError> {
    let acquired = state
        .upstreams
        .acquire_for_access(model, PoolKind::Chat, access)
        .map_err(|e| LoopError::Upstream(e.to_string()))?;
    let backend_name = acquired.backend().name.clone();
    let started = Instant::now();
    let serialized = serde_json::to_vec(&body_value)
        .map_err(|e| LoopError::Upstream(format!("serialise: {e}")))?;
    let url = format!("{}/chat/completions", acquired.backend().base_url);
    let mut http = state.http.post(&url);
    for (name, value) in headers {
        if is_request_header_forwarded(name) {
            http = http.header(name.as_str(), value);
        }
    }
    http = http.header("content-type", "application/json");
    if let Some(key) = acquired.backend().api_key.as_deref() {
        http = http.bearer_auth(key);
    }
    // A backend was contacted, so the call is counted either way — a
    // transport/read failure records a 502 row (parallel to `forward`).
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

/// Byte-faithful `/v1/chat/completions` passthrough for a caller with no
/// gateway tool grants: resolve the model, apply admin defaults, rewrite the
/// outgoing `model` to the real id, and stream the upstream response through
/// 1:1 (any client-driven tool loop is left untouched). Factored out of
/// [`chat_completions`] so its prologue reads as prologue + a three-way
/// dispatch.
async fn chat_bytedumb(
    state: &Arc<RamaState>,
    user: &UserCtx,
    model: &str,
    access: &gateway_core::server::upstreams::PoolAccess,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // `route` resolves aliases + the two fallbacks and returns a structured
    // `RouteError`, so `route_error_response` maps an unknown model straight
    // to 404 `model_not_found` (and known-but-down to 503). Acquires the slot
    // up front so the resolved real id is known before we touch the body.
    let acquired = match state.upstreams.route_access(model, PoolKind::Chat, access) {
        Ok(a) => a,
        Err(e) => return route_error_response(e),
    };
    let real_model = acquired.resolved_model().to_string();
    // Admin sampling/reasoning defaults key on the *real* model id (so an
    // alias inherits the target's defaults). Client keys still win —
    // `apply_defaults` only fills missing top-level fields. Then rewrite the
    // outgoing `model` to the real id (upstreams don't know the alias).
    let body =
        gateway_core::server::model_defaults::apply_defaults_to_bytes(&state.db, &real_model, body)
            .await;
    let body = rewrite_model_in_bytes(body, &real_model);
    let rec = RecordParams::v1(
        user,
        UsageKind::Chat,
        real_model.clone(),
        state
            .upstreams
            .enforce_limits_for_model(&real_model, PoolKind::Chat),
    );
    let resp = forward_streaming(
        state,
        acquired,
        Method::POST,
        "chat/completions",
        headers,
        body,
        rec,
    )
    .await;
    with_resolved_model_header(resp, model, &real_model)
}

pub async fn chat_completions(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    // Source IP for `get_user_location`: proxy header (behind a load
    // balancer) first, else the direct TCP socket peer. Captured before
    // we split the request so the socket extension is still reachable.
    let client_ip = gateway_core::server::geoip::client_ip(req.headers())
        .or_else(|| gateway_core::server::geoip::peer_ip(&req));
    let (parts, body) = req.into_parts();
    let user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    if let Some(resp) = limit_check(&state, &user).await {
        return resp;
    }
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
    let mut allowed_tools = state.allowed_tools_for_token(&user).await;

    // Build the caller's connected-connector MCP overlay ONCE for this request
    // and union its ids into the advertised set, so the tools we advertise are
    // exactly the tools the `CompositeToolSource` below can dispatch (no
    // advertise/execute drift). Empty + cheap when nothing is connected. Only
    // when the token's master tool switch is on (else the per-token resolution
    // already returned empty and we keep the byte-dumb path).
    let user_mcp = if user.tools_enabled {
        let role_ids = state.role_ids_for(&user.roles);
        let is_admin = state.rbac.is_admin(&role_ids);
        state
            .mcp
            .layer_for_user(
                &user.user_id,
                &role_ids,
                is_admin,
                gateway_core::server::tools::mcp::manager::AskContext::Api {
                    token_id: &user.token_id,
                },
            )
            .await
    } else {
        gateway_core::server::tools::mcp::manager::UserMcpLayer::default()
    };
    state.union_mcp_tool_ids(&mut allowed_tools, &user_mcp);

    // Drop chat-session-only tools: the `/v1` proxy paths carry no session
    // (`assistant_turn_id`/`session_id` are None below), so the typst render
    // family, the document-canvas tools, and `upload_attachment` can't run
    // here — advertising them just lets the model pick one and hit a
    // "only available inside a chat session" error instead of a completion.
    allowed_tools.retain(|id| !gateway_core::server::tools::catalog::requires_chat_session(id));

    // Byte-dumb proxy: only when the user has no gateway tool grants.
    // There's nothing to inject, so route bytes 1:1 and leave any
    // client-driven tool loop untouched. When the user *does* have
    // grants we fall through to the gateway-tool path even if the
    // client brought its own `tools` — `inject_tools` unions ours in
    // and the loop runs gateway-owned calls while passing client-owned
    // ones through.
    // Per-user pool access: a model served only by pools this caller can't
    // reach routes as `UnknownModel` → 404, identical to a nonexistent model.
    let access = state.pool_access_for(&user.roles);
    if allowed_tools.is_empty() {
        return chat_bytedumb(&state, &user, &model, &access, parts.headers, body).await;
    }

    // Gateway-tool path. Resolve aliases + fallback once, up front: the tool
    // loops acquire per round (flattening errors into `LoopError::Upstream` →
    // 503, so they can't distinguish an unknown model), and every round must
    // dispatch the *same* resolved real id. `route` here both maps the OpenAI
    // 404/503 before streaming starts and yields the real id to forward.
    let real_model = match state
        .upstreams
        .route_access(&model, PoolKind::Chat, &access)
    {
        Ok(a) => a.resolved_model().to_string(),
        Err(e) => return route_error_response(e),
    };

    // Defaults key on the resolved real id, same as the byte-dumb path.
    let body =
        gateway_core::server::model_defaults::apply_defaults_to_bytes(&state.db, &real_model, body)
            .await;

    // Gateway-tool path: inject definitions, then run either the
    // streaming intercept or the buffered runner.
    let mut request_body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                &format!("body is not valid JSON: {err}"),
            );
        }
    };
    // Rewrite the body's `model` to the real id so every upstream round in the
    // loop (which serialises `request_body`) targets the resolved model.
    set_model_in_value(&mut request_body, &real_model);

    let wants_streaming = request_body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    if wants_streaming {
        let resp = forward_streaming_with_tools(
            state.clone(),
            user.clone(),
            real_model.clone(),
            parts.headers.clone(),
            client_ip.clone(),
            request_body,
            allowed_tools,
            user_mcp,
        )
        .await;
        return with_resolved_model_header(resp, &model, &real_model);
    }
    let tool_ctx = proxy_tool_ctx(
        &state,
        user.user_id.clone(),
        user.roles.clone(),
        client_ip.clone(),
    );
    let state_clone = state.clone();
    let model_clone = real_model.clone();
    let access_clone = access.clone();
    let headers_clone = parts.headers.clone();
    // One usage row per upstream round — built per request, finished off
    // with backend/status/latency/tokens inside the loop closure.
    let rec = RecordParams::v1(
        &user,
        UsageKind::Chat,
        real_model.clone(),
        state
            .upstreams
            .enforce_limits_for_model(&real_model, PoolKind::Chat),
    );

    // Reuse the layer built once above (same ids we advertised) for dispatch.
    let comfyui = state
        .comfyui
        .as_ref()
        .map(|h| gateway_core::server::comfyui::ComfyuiToolSource::new((**h).clone()));
    let tool_source = gateway_core::server::tools::mcp::manager::CompositeToolSource::new(
        state.tools.as_ref(),
        &user_mcp,
    )
    .with_comfyui(comfyui.as_ref());

    let outcome =
        runner::run_with_tools(
            &tool_source,
            &allowed_tools,
            &tool_ctx,
            request_body,
            move |body_value| {
                let state = state_clone.clone();
                let model = model_clone.clone();
                let access = access_clone.clone();
                let headers = headers_clone.clone();
                let rec = rec.clone();
                async move {
                    forward_one_round(&state, &model, &access, &headers, &rec, body_value).await
                }
            },
        )
        .await;

    let outcome = match outcome {
        Ok(o) => o,
        Err(err) => return loop_error_response(err),
    };

    if outcome.status >= 400 {
        maybe_learn_capability(&state, &real_model, outcome.status, &outcome.body);
    }

    let resp = Response::builder()
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
        });
    with_resolved_model_header(resp, &model, &real_model)
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
    if let Some(resp) = limit_check(&state, &user).await {
        return resp;
    }
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };
    // Model is parsed inside; `handle_transcription` fills the real model id
    // and the resolved `enforce_limits` flag into `rec` once routing has run.
    let rec = RecordParams::v1(&user, UsageKind::Transcription, String::new(), true);
    let access = state.pool_access_for(&user.roles);
    handle_transcription(state, parts.headers, body, rec, access).await
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
    // Load the user once: for the usage-row email AND to resolve their gateway
    // groups so the transcription model is gated to pools they may access, same
    // as the API path.
    let user_row = gateway_core::server::db::users::find_by_id(&state.db, &session.user_id)
        .await
        .ok()
        .flatten();
    let user_email = if state.usage.is_enabled() {
        user_row
            .as_ref()
            .map(|u| u.email.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let access =
        state.pool_access_for(user_row.as_ref().map(|u| u.roles.as_slice()).unwrap_or(&[]));
    let rec = RecordParams {
        user_id: session.user_id.clone(),
        user_email,
        token_id: None,
        token_name: None,
        source: UsageSource::Chat,
        kind: UsageKind::Transcription,
        model: String::new(),
        // Overwritten in `handle_transcription` once the real model resolves.
        enforce_limits: true,
        input_units: None,
        output_units: None,
    };
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };
    handle_transcription(state, parts.headers, body, rec, access).await
}

/// Shared body of both transcription handlers: parse → VAD-trim → rebuild
/// multipart → forward. Pulled out because the bearer/session paths
/// only differ in auth.
async fn handle_transcription(
    state: Arc<RamaState>,
    mut headers: HeaderMap,
    body: Bytes,
    mut rec: RecordParams,
    access: gateway_core::server::upstreams::PoolAccess,
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

    let mut trimmed_fields = trim_audio_field(fields);

    // Resolve aliases + fallback before rebuilding the multipart body, so the
    // `model` part we forward carries the real id the upstream knows. The slot
    // is held across the (in-memory) VAD check + rebuild below.
    let acquired = match state
        .upstreams
        .route_access(&model, PoolKind::Transcription, &access)
    {
        Ok(a) => a,
        Err(e) => return route_error_response(e),
    };
    let real_model = acquired.resolved_model().to_string();
    if real_model != model
        && let Some(field) = trimmed_fields.iter_mut().find(|f| f.name == "model")
    {
        field.bytes = Bytes::from(real_model.clone());
    }

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
    // Decode the PCM duration at most once: it gates the sub-threshold check
    // below and doubles as the billable duration when the upload is 16 kHz
    // mono PCM (the common browser-worklet case).
    let file_bytes = trimmed_fields
        .iter()
        .find(|f| f.name == "file")
        .map(|f| &f.bytes);
    let pcm_seconds = file_bytes.and_then(|b| vad::pcm16_mono_16k_duration_seconds(b));
    if let Some(secs) = pcm_seconds
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

    rec.model = real_model.clone();
    rec.input_units = file_bytes.and_then(|b| {
        pcm_seconds
            .or_else(|| vad::wav_duration_seconds(b))
            .or_else(|| vad::encoded_audio_duration_seconds(b.clone()))
    });
    rec.enforce_limits = state
        .upstreams
        .enforce_limits_for_model(&real_model, PoolKind::Transcription);
    let resp = forward(
        &state,
        acquired,
        Method::POST,
        "audio/transcriptions",
        headers,
        new_body,
        rec,
    )
    .await;
    with_resolved_model_header(resp, &model, &real_model)
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
    if let Some(resp) = limit_check(&state, &user).await {
        return resp;
    }
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
    // `route` resolves aliases + fallback and maps an unknown model → 404
    // `model_not_found` / all-down → 503 via `route_error_response`.
    let access = state.pool_access_for(&user.roles);
    let acquired = match state
        .upstreams
        .route_access(&model, PoolKind::Embedding, &access)
    {
        Ok(a) => a,
        Err(e) => return route_error_response(e),
    };
    let real_model = acquired.resolved_model().to_string();
    let body = rewrite_model_in_bytes(body, &real_model);
    let rec = RecordParams::v1(
        &user,
        UsageKind::Embedding,
        real_model.clone(),
        state
            .upstreams
            .enforce_limits_for_model(&real_model, PoolKind::Embedding),
    );
    let resp = forward(
        &state,
        acquired,
        Method::POST,
        "embeddings",
        parts.headers,
        body,
        rec,
    )
    .await;
    with_resolved_model_header(resp, &model, &real_model)
}

/// `POST /v1/images/generations` — OpenAI-compatible image generation.
/// Byte-dumb proxy, exactly like [`embeddings`]: authenticate, read the
/// `model`, pick a healthy backend from the **Image** pool, and relay the
/// request/response 1:1. The client gets the provider's response verbatim
/// (a `b64_json` payload or a `url`), which is the correct OpenAI-compatible
/// behaviour — we don't server-side-fetch a URL the client asked us to proxy.
/// (The chat `generate_image` tool takes a different path — it needs the
/// bytes in hand to re-host in S3 — via `server::image_gen`.)
pub async fn images_generations(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    if let Some(resp) = limit_check(&state, &user).await {
        return resp;
    }
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
    let access = state.pool_access_for(&user.roles);
    let acquired = match state
        .upstreams
        .route_access(&model, PoolKind::Image, &access)
    {
        Ok(a) => a,
        Err(e) => return route_error_response(e),
    };
    let real_model = acquired.resolved_model().to_string();
    let body = rewrite_model_in_bytes(body, &real_model);
    let rec = RecordParams::v1(
        &user,
        UsageKind::Image,
        real_model.clone(),
        state
            .upstreams
            .enforce_limits_for_model(&real_model, PoolKind::Image),
    );
    let resp = forward(
        &state,
        acquired,
        Method::POST,
        "images/generations",
        parts.headers,
        body,
        rec,
    )
    .await;
    with_resolved_model_header(resp, &model, &real_model)
}

/// `POST /v1/images/edits` — OpenAI-compatible image editing (multipart:
/// `image` file + `prompt` + `model`). Mirrors [`transcribe`]'s multipart
/// relay: parse fields, resolve the `model` alias, rewrite that field to the
/// real id, rebuild the body, and forward 1:1 to the Image pool. Whether the
/// chosen backend actually supports editing is the upstream's concern here
/// (the chat `edit_image` tool gates on `supports_edit`; this raw API surface
/// stays a thin proxy).
pub async fn images_edits(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    if let Some(resp) = limit_check(&state, &user).await {
        return resp;
    }
    let mut headers = parts.headers;
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };
    let mut fields = match parse_multipart_fields(&headers, body).await {
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
    let access = state.pool_access_for(&user.roles);
    let acquired = match state
        .upstreams
        .route_access(&model, PoolKind::Image, &access)
    {
        Ok(a) => a,
        Err(e) => return route_error_response(e),
    };
    let real_model = acquired.resolved_model().to_string();
    if real_model != model
        && let Some(field) = fields.iter_mut().find(|f| f.name == "model")
    {
        field.bytes = Bytes::from(real_model.clone());
    }
    let (new_body, content_type) = match build_multipart(&fields) {
        Ok(v) => v,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };
    headers.remove(rama::http::header::CONTENT_TYPE);
    if let Ok(val) = rama::http::HeaderValue::from_str(&content_type) {
        headers.insert(rama::http::header::CONTENT_TYPE, val);
    }
    let mut rec = RecordParams::v1(
        &user,
        UsageKind::Image,
        real_model.clone(),
        state
            .upstreams
            .enforce_limits_for_model(&real_model, PoolKind::Image),
    );
    rec.input_units = image_edit_units(&fields);
    let resp = forward(
        &state,
        acquired,
        Method::POST,
        "images/edits",
        headers,
        new_body,
        rec,
    )
    .await;
    with_resolved_model_header(resp, &model, &real_model)
}

/// Bounded in-memory TTS cache for the voice-mode session path. Identical
/// `(model, voice, text)` always synthesises to identical audio, so repeats —
/// above all the fixed greeting spoken on *every* modal open — are served from
/// memory instead of re-billing the TTS backend. Keyed `model|voice|text`; only
/// short inputs (greetings + single sentences) are cached, and the map is
/// capped so a long session can't grow it without bound. Not used by the raw
/// `/v1/audio/speech` proxy, which stays a 1:1 passthrough.
static TTS_CACHE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Bytes>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
const TTS_CACHE_MAX_ENTRIES: usize = 256;
const TTS_CACHE_MAX_TEXT_CHARS: usize = 400;

fn tts_cache_get(key: &str) -> Option<Bytes> {
    TTS_CACHE.lock().ok()?.get(key).cloned()
}

fn tts_cache_put(key: String, bytes: Bytes) {
    if let Ok(mut m) = TTS_CACHE.lock() {
        // Crude cap: clear when full. The greeting + common sentences re-cache
        // on their next use, so this is cheap and keeps memory bounded.
        if m.len() >= TTS_CACHE_MAX_ENTRIES {
            m.clear();
        }
        m.insert(key, bytes);
    }
}

/// A cached-audio response: 200 + `audio/mpeg` (voice mode always requests mp3)
/// + a private browser cache so a reopen in the same session is free too.
fn audio_response(bytes: Bytes) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(rama::http::header::CONTENT_TYPE, "audio/mpeg")
        .header(rama::http::header::CACHE_CONTROL, "private, max-age=86400")
        .body(bytes.into())
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "building audio response",
            )
        })
}

/// `POST /v1/audio/speech` — OpenAI-compatible text-to-speech. Byte-dumb
/// proxy: authenticate, read the `model`, pick a healthy backend from the
/// **Speech** pool, relay request/response 1:1 (the response body is audio).
/// No sanitising — `/v1` callers get exactly what they sent, matching every
/// other `/v1` endpoint. Voice mode's own sanitised, voice-mapped path is
/// `POST /api/v0/speech`.
pub async fn speech(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    if let Some(resp) = limit_check(&state, &user).await {
        return resp;
    }
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
    let access = state.pool_access_for(&user.roles);
    let acquired = match state
        .upstreams
        .route_access(&model, PoolKind::Speech, &access)
    {
        Ok(a) => a,
        Err(e) => return route_error_response(e),
    };
    let real_model = acquired.resolved_model().to_string();
    let body = rewrite_model_in_bytes(body, &real_model);
    let rec = RecordParams::v1(
        &user,
        UsageKind::Speech,
        real_model.clone(),
        state
            .upstreams
            .enforce_limits_for_model(&real_model, PoolKind::Speech),
    );
    let resp = forward(
        &state,
        acquired,
        Method::POST,
        "audio/speech",
        parts.headers,
        body,
        rec,
    )
    .await;
    with_resolved_model_header(resp, &model, &real_model)
}

/// `POST /api/v0/speech` — session-authed voice-mode TTS. Body:
/// `{"text": "...", "language": "de"}`. The text (a sentence, for streaming) is
/// sanitised to speakable prose (Markdown/code/tables → localised spoken
/// markers), the voice is resolved from the operator's language→voice map, and
/// the request is forwarded to the speech pool. Returns audio bytes for the
/// browser to play; `204 No Content` when nothing speakable remains.
pub async fn speech_session(State(state): State<Arc<RamaState>>, req: Request) -> Response {
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
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, "invalid_request", &msg),
    };
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                &format!("body must be JSON: {e}"),
            );
        }
    };
    let text = parsed
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing `text` field",
        );
    }
    let language = parsed
        .get("language")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();

    // Markers are spoken, so localise them to the spoken language (fall back to
    // the request's UI language, then English). Bound to locals so the
    // SpokenMarkers borrows outlive the `to_spoken` call.
    let marker_lang =
        Lang::from_code(&language).unwrap_or_else(|| Lang::from_headers(&parts.headers));
    let code_marker = t(marker_lang, "voice-code-marker");
    let table_marker = t(marker_lang, "voice-table-marker");
    let spoken = speech::to_spoken(
        &text,
        &SpokenMarkers {
            code: &code_marker,
            table: &table_marker,
        },
    );
    if spoken.trim().is_empty() {
        // Chunk was entirely non-speakable (e.g. a bare code fence). Nothing to
        // synthesize — tell the client to skip it.
        return (StatusCode::NO_CONTENT, "").into_response();
    }

    let Some((model, voice)) = state.upstreams.speech_target(&language) else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_speech_backend",
            "no speech (TTS) pool is configured",
        );
    };
    // Cache lookup BEFORE routing, so a hit costs no inflight slot and no TTS
    // spend. Key on the pre-resolution model + voice + spoken text.
    let cache_key = format!("{model}|{}|{spoken}", voice.as_deref().unwrap_or(""));
    if let Some(bytes) = tts_cache_get(&cache_key) {
        return audio_response(bytes);
    }
    let spoken_len = spoken.chars().count();
    // Gate the TTS pool to the session user's groups, same as every other route.
    let access = {
        let roles = gateway_core::server::db::users::find_by_id(&state.db, &session.user_id)
            .await
            .ok()
            .flatten()
            .map(|u| u.roles)
            .unwrap_or_default();
        state.pool_access_for(&roles)
    };
    let acquired = match state
        .upstreams
        .route_access(&model, PoolKind::Speech, &access)
    {
        Ok(a) => a,
        Err(e) => return route_error_response(e),
    };
    let real_model = acquired.resolved_model().to_string();
    let mut req_body = json!({
        "model": real_model,
        "input": spoken,
        "response_format": "mp3",
    });
    if let Some(v) = voice {
        req_body["voice"] = json!(v);
    }

    let user_email = if state.usage.is_enabled() {
        gateway_core::server::db::users::find_by_id(&state.db, &session.user_id)
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
        kind: UsageKind::Speech,
        model: real_model.clone(),
        enforce_limits: state
            .upstreams
            .enforce_limits_for_model(&real_model, PoolKind::Speech),
        input_units: None,
        output_units: None,
    };
    let serialized = match serde_json::to_vec(&req_body) {
        Ok(v) => Bytes::from(v),
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &format!("serialising speech request: {e}"),
            );
        }
    };
    // JSON body — set Content-Type accordingly (the inbound one is already
    // application/json for our own UI, but be explicit).
    let mut headers = parts.headers;
    headers.remove(rama::http::header::CONTENT_TYPE);
    headers.insert(
        rama::http::header::CONTENT_TYPE,
        rama::http::HeaderValue::from_static("application/json"),
    );
    let resp = forward(
        &state,
        acquired,
        Method::POST,
        "audio/speech",
        headers,
        serialized,
        rec,
    )
    .await;
    // Buffer the audio so a successful, short synthesis is cached for next time
    // (the greeting + repeated sentences). Non-2xx / oversized inputs pass
    // through uncached. Small clips, so buffering is cheap.
    let (parts, body) = resp.into_parts();
    let cacheable = parts.status.as_u16() == 200 && spoken_len <= TTS_CACHE_MAX_TEXT_CHARS;
    let bytes = read_body_to_bytes(body).await.unwrap_or_default();
    if cacheable {
        tts_cache_put(cache_key, bytes.clone());
    }
    Response::from_parts(parts, bytes.into())
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
    let user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    // Per-user model visibility: only models served by a pool this caller may
    // access. A withheld model is also unroutable for them (see `route_access`),
    // so the list is a true capability view, not a cosmetic filter.
    let access = state.pool_access_for(&user.roles);
    let data: Vec<Value> = state
        .upstreams
        .all_models_for(&access)
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
    let user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let raw = parts
        .uri
        .path()
        .strip_prefix("/v1/models/")
        .unwrap_or_default();
    let id = percent_decode(raw);
    let access = state.pool_access_for(&user.roles);
    if id.is_empty() || !state.upstreams.knows_any_for(&id, &access) {
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
/// Fire-and-forget auto-learning: if an upstream error (`status` + `body`)
/// pattern-matches a rejected capability (image / tool / `response_format`),
/// record it against `model` so the gateway stops sending that content type to
/// it. `model` is the *resolved* real id — the same key the capability read
/// path uses (see [`gateway_core::server::capabilities`]) — so a learned flag is found
/// again. Admin-set `Enabled` values are never overwritten (see
/// [`gateway_core::server::db::model_defaults::mark_unsupported`]). No-op when the
/// error doesn't classify.
fn maybe_learn_capability(state: &RamaState, model: &str, status: u16, body: &[u8]) {
    // Only 400/422 carry capability rejections. Gate on status *before* touching
    // the body so the common error statuses (429/5xx) don't allocate a String
    // that classify_error would immediately discard — this runs on every failed
    // upstream response.
    if status != 400 && status != 422 {
        return;
    }
    // Classify a generous prefix, not a tiny 500-char one: providers often lead
    // with a long error object and put the actual rejection phrase further in,
    // so a 500-char cap misses it. 16 KiB comfortably covers any real error body
    // while still bounding work on a pathological one.
    let body_str = String::from_utf8_lossy(body)
        .chars()
        .take(16 * 1024)
        .collect::<String>();
    let Some(cap) =
        gateway_core::server::upstreams::error_classify::classify_error(status, &body_str)
    else {
        return;
    };
    let model_key = model.to_string();
    let db = state.db.clone();
    tokio::spawn(async move {
        if let Err(e) =
            gateway_core::server::db::model_defaults::mark_unsupported(&db, &model_key, cap).await
        {
            tracing::warn!(error = %e, "auto-learning: failed to record capability");
        } else {
            tracing::info!(
                model = %model_key,
                capability = cap.column(),
                "auto-learning: marked capability as unsupported"
            );
        }
    });
}

async fn forward(
    state: &RamaState,
    acquired: Acquired,
    method: Method,
    upstream_path: &str,
    client_headers: HeaderMap,
    body: Bytes,
    mut rec: RecordParams,
) -> Response {
    // Outbound HTTP via reqwest, by design. Rama serves the inbound
    // side; reqwest handles outbound. Same split most rust web
    // projects use. `EasyHttpWebClient::default()` works for the
    // outbound role too, but it needs `rama/rustls,tls` (which pulls
    // aws-lc-sys → cmake) and its concrete type is ugly enough as a
    // struct field that the maintenance cost doesn't pay for itself.
    let backend = acquired.backend();
    let backend_name = backend.name.clone();
    let model_key = acquired.resolved_model().to_string();
    let url = format!("{}/{}", backend.base_url, upstream_path);
    let started = Instant::now();
    let request_usage = request_units(rec.kind, &body);

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
            rec.input_units = None;
            rec.output_units = None;
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
            rec.input_units = None;
            rec.output_units = None;
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

    let (tokens, response_usage) = response_metrics(rec.kind, &bytes);
    if status.is_success() {
        rec.input_units = prefer_positive(
            response_usage.input,
            prefer_positive(request_usage.input, rec.input_units),
        );
        rec.output_units = response_usage
            .output
            .or(request_usage.output)
            .or(rec.output_units);
    } else {
        rec.input_units = None;
        rec.output_units = None;
    }

    // One row per upstream call. Token and modality-specific usage are
    // normalized before the row is handed to the asynchronous writer.
    rec.emit(
        &state.usage,
        &backend_name,
        status.as_u16(),
        started,
        tokens,
    );

    // Auto-learn a rejected capability before relaying the error verbatim.
    if !status.is_success() {
        maybe_learn_capability(state, &model_key, status.as_u16(), &bytes);
    }

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
        json!({"error": {"message": gateway_core::loop_guard::LOOP_MESSAGE, "type": "loop_detected"}})
    ))
}

/// Force a streaming chat request to ask the upstream for a trailing `usage`
/// frame (`stream_options.include_usage = true`), so token/cost accounting
/// works even when the client didn't opt in — otherwise a streamed `/v1` call
/// records zero tokens (and thus zero cost), a silent hole in spend tracking.
///
/// Returns the (possibly rewritten) body plus `suppress_usage_frame`: `true`
/// when we injected the option the client hadn't asked for, so the relay taps
/// the usage frame for accounting but drops it from the client-facing stream
/// (keeping the passthrough byte-faithful). Non-streaming bodies are returned
/// untouched (`stream_options` is invalid without `stream:true`), as are
/// bodies that don't parse as JSON.
fn force_usage_in_body(body: Bytes) -> (Bytes, bool) {
    let Ok(mut v) = serde_json::from_slice::<Value>(&body) else {
        return (body, false);
    };
    if !v.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        return (body, false);
    }
    let client_opted = v
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(obj) = v.as_object_mut() {
        let so = obj.entry("stream_options").or_insert_with(|| json!({}));
        if let Some(m) = so.as_object_mut() {
            m.insert("include_usage".into(), Value::Bool(true));
        }
    }
    match serde_json::to_vec(&v) {
        Ok(b) => (Bytes::from(b), !client_opted),
        Err(_) => (body, false),
    }
}

/// Streaming variant of `forward` — used by /v1/chat/completions so SSE
/// (`stream: true`) responses unfold token-by-token to the client
/// instead of buffering. Relays each upstream frame 1:1 while tapping the
/// deltas through a [`gateway_core::loop_guard::LoopGuard`]; the `Acquired` RAII
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
    let model_key = acquired.resolved_model().to_string();
    let url = format!("{}/{}", backend.base_url, upstream_path);
    let started = Instant::now();

    // Ensure streaming calls report a trailing usage frame (else zero
    // tokens/cost); hide it from the client if they didn't opt in.
    let (body, suppress_usage_frame) = force_usage_in_body(body);

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

    // An upstream error on a streaming request isn't SSE — it's a plain JSON
    // error body. Buffer it so we can classify a rejected capability (the
    // byte-dumb stream relay below never inspects status), then relay the same
    // status + body verbatim. Auto-learning fires before the client sees it.
    if !status.is_success() {
        let bytes = upstream.bytes().await.unwrap_or_default();
        drop(acquired);
        rec.emit(
            &state.usage,
            &backend_name,
            status.as_u16(),
            started,
            tokens_from_bytes(&bytes),
        );
        maybe_learn_capability(state, &model_key, status.as_u16(), &bytes);
        let mut builder = Response::builder().status(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        );
        for (name, value) in forwarded_headers {
            builder = builder.header(name, value);
        }
        return builder.body(bytes.into()).unwrap_or_else(|err| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &format!("building response: {err}"),
            )
        });
    }

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
        let mut content_guard = gateway_core::loop_guard::LoopGuard::new();
        let mut reasoning_guard = gateway_core::loop_guard::LoopGuard::new();
        let mut looped = false;
        // Token counts ride the trailing `usage` frame, which we forced on via
        // `force_usage_in_body`. We forward at SSE-event granularity (not raw
        // network chunks) so that, when `suppress_usage_frame` is set (the
        // client didn't ask for usage), we can tap that frame for accounting
        // but drop it from the relayed stream — keeping the passthrough
        // byte-faithful. Re-assembled event bytes are identical to the
        // upstream's, just re-chunked on `\n\n` boundaries.
        let mut tokens: (Option<i64>, Option<i64>, Option<i64>) = (None, None, None);
        'frames: while let Some(frame) = upstream_stream.next().await {
            let Ok(frame) = frame else { break };
            buf.extend_from_slice(&frame);
            while let Some(event) = gateway_core::server::sse::next_event(&mut buf) {
                let text = String::from_utf8_lossy(&event);
                let mut is_usage_only = false;
                for line in text.lines() {
                    let Some(payload) = gateway_core::server::sse::data_payload(line) else {
                        continue;
                    };
                    let Ok(v) = serde_json::from_str::<Value>(payload) else {
                        continue;
                    };
                    if let Some(t) = gateway_core::server::sse::usage_tokens(&v) {
                        tokens = t;
                        // OpenAI's trailing usage frame carries empty `choices`;
                        // a usage riding a content chunk keeps its choice.
                        is_usage_only = v.pointer("/choices/0").is_none();
                    }
                    let delta = gateway_core::server::sse::ChatDelta::new(&v);
                    if let Some(t) = delta.content()
                        && content_guard.push(t)
                    {
                        looped = true;
                    }
                    if let Some(t) = delta.reasoning()
                        && reasoning_guard.push(t)
                    {
                        looped = true;
                    }
                }
                // Relay the event verbatim, unless it's the usage frame we're
                // hiding from a client that didn't opt in.
                if !(suppress_usage_frame && is_usage_only)
                    && tx.unbounded_send(Ok(Bytes::from(event))).is_err()
                {
                    // Client disconnected — still record the call (partial).
                    rec.emit(&usage_sink, &backend_name, status.as_u16(), started, tokens);
                    return;
                }
                if looped {
                    break 'frames;
                }
            }
        }
        if looped {
            let _ = tx.unbounded_send(Ok(loop_error_chunk()));
            let _ = tx.unbounded_send(Ok(Bytes::from("data: [DONE]\n\n")));
        } else if !buf.is_empty() {
            // Non-streaming requests also take this path: the upstream replies
            // with one plain JSON body (no `\n\n`-framed events), so `buf`
            // holds it whole here. Parse it for the `usage` block (if we
            // haven't already) and forward it verbatim.
            if tokens == (None, None, None)
                && let Ok(v) = serde_json::from_slice::<Value>(&buf)
            {
                tokens = usage::usage_from_value(&v);
            }
            let _ = tx.unbounded_send(Ok(Bytes::from(std::mem::take(&mut buf))));
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
#[allow(clippy::too_many_arguments)]
async fn forward_streaming_with_tools(
    state: Arc<RamaState>,
    user: UserCtx,
    model: String,
    client_headers: HeaderMap,
    client_ip: Option<String>,
    mut request_body: Value,
    allowed_tools: Vec<String>,
    user_mcp: gateway_core::server::tools::mcp::manager::UserMcpLayer,
) -> Response {
    // Use the layer built once by the caller (same ids it advertised) for
    // injection here and for dispatch inside the loop.
    let comfyui = state
        .comfyui
        .as_ref()
        .map(|h| gateway_core::server::comfyui::ComfyuiToolSource::new((**h).clone()));
    let tool_source = gateway_core::server::tools::mcp::manager::CompositeToolSource::new(
        state.tools.as_ref(),
        &user_mcp,
    )
    .with_comfyui(comfyui.as_ref());
    // Inject gateway tools, force stream:true. `stream_options` can
    // stay (vLLM accepts it with stream:true).
    if let Err(err) = runner::inject_tools(&mut request_body, &tool_source, &allowed_tools) {
        return loop_error_response(err);
    }
    if let Some(obj) = request_body.as_object_mut() {
        obj.insert("stream".into(), Value::Bool(true));
    }

    let tool_ctx = proxy_tool_ctx(&state, user.user_id.clone(), user.roles.clone(), client_ip);

    // One usage row per upstream round; built here where the bearer's
    // identity + token are known, finished off inside the loop.
    let rec = RecordParams::v1(
        &user,
        UsageKind::Chat,
        model.clone(),
        state
            .upstreams
            .enforce_limits_for_model(&model, PoolKind::Chat),
    );

    // rama::futures::channel::mpsc::unbounded matches the pattern used by
    // the chat-page SSE producer (`pages/chat/mod.rs`).
    let (mut tx, rx) = mpsc::unbounded::<Result<Bytes, std::io::Error>>();
    let access = state.pool_access_for(&user.roles);

    tokio::spawn(async move {
        if let Err(err) = drive_streaming_tool_loop(
            state,
            model,
            request_body,
            client_headers,
            tool_ctx,
            rec,
            user_mcp,
            access,
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
    tool_acc: &BTreeMap<usize, ToolCallAcc>,
) -> Vec<Bytes> {
    let tool_calls: Vec<Value> = tool_acc
        .iter()
        .map(|(index, acc)| {
            json!({
                "index": index,
                "id": acc.id,
                "type": "function",
                "function": {"name": acc.name, "arguments": runner::normalize_tool_arguments(&acc.arguments)},
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
    request_body: Value,
    client_headers: HeaderMap,
    tool_ctx: ToolContext,
    rec: RecordParams,
    user_mcp: gateway_core::server::tools::mcp::manager::UserMcpLayer,
    access: gateway_core::server::upstreams::PoolAccess,
    tx: &mut mpsc::UnboundedSender<Result<Bytes, std::io::Error>>,
) -> Result<(), String> {
    // Free the turn's sandbox lease on every exit of the loop below — the
    // explicit, awaited counterpart to the chat path's `run_turn` and the
    // buffered path's `run_with_tools` wrapper (the `Drop` guard is only the
    // backstop). Clone the lease handle out before `tool_ctx` is moved in.
    let lease = tool_ctx.sandbox_lease.clone();
    let out = drive_streaming_tool_loop_inner(
        state,
        model,
        request_body,
        client_headers,
        tool_ctx,
        rec,
        user_mcp,
        access,
        tx,
    )
    .await;
    if let Some(lease) = &lease {
        lease.release().await;
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn drive_streaming_tool_loop_inner(
    state: Arc<RamaState>,
    model: String,
    mut request_body: Value,
    client_headers: HeaderMap,
    tool_ctx: ToolContext,
    rec: RecordParams,
    user_mcp: gateway_core::server::tools::mcp::manager::UserMcpLayer,
    access: gateway_core::server::upstreams::PoolAccess,
    tx: &mut mpsc::UnboundedSender<Result<Bytes, std::io::Error>>,
) -> Result<(), String> {
    use rama::futures::StreamExt;

    // The caller's connected-connector MCP tools, unioned onto the registry so
    // the ownership split + dispatch below recognise + run them too.
    let comfyui = state
        .comfyui
        .as_ref()
        .map(|h| gateway_core::server::comfyui::ComfyuiToolSource::new((**h).clone()));
    let tool_source = gateway_core::server::tools::mcp::manager::CompositeToolSource::new(
        state.tools.as_ref(),
        &user_mcp,
    )
    .with_comfyui(comfyui.as_ref());

    // Force a trailing usage frame each round so token/cost accounting works
    // even when the client didn't opt in; hide it from the client stream
    // unless they asked (fits the existing per-round hide policy below).
    let client_wants_usage = request_body
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(obj) = request_body.as_object_mut() {
        let so = obj.entry("stream_options").or_insert_with(|| json!({}));
        if let Some(m) = so.as_object_mut() {
            m.insert("include_usage".into(), Value::Bool(true));
        }
    }
    let suppress_usage_frame = !client_wants_usage;

    for _round in 0..STREAM_TOOL_LOOP_MAX_ROUNDS {
        let acquired = state
            .upstreams
            .acquire_for_access(&model, PoolKind::Chat, &access)
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
            let body_str = String::from_utf8_lossy(&bytes)
                .chars()
                .take(500)
                .collect::<String>();
            maybe_learn_capability(&state, &model, status.as_u16(), &bytes);
            return Err(format!("upstream {status}: {body_str}"));
        }
        let status_code = upstream.status().as_u16();

        let mut tool_acc: BTreeMap<usize, ToolCallAcc> = BTreeMap::new();
        let mut chunk_meta = ChunkMeta::default();
        let mut byte_buf: Vec<u8> = Vec::new();
        // Per-round repetition guards. A degenerate loop in either channel
        // ends the stream cleanly (error chunk + [DONE]) instead of running
        // until the token ceiling. Repetition-based only, so a long but
        // progressing answer is never cut short.
        let mut content_guard = gateway_core::loop_guard::LoopGuard::new();
        let mut reasoning_guard = gateway_core::loop_guard::LoopGuard::new();
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
            while let Some(event_bytes) = gateway_core::server::sse::next_event(&mut byte_buf) {
                let event_str = String::from_utf8_lossy(&event_bytes);

                let mut is_done = false;
                let mut hide_event = false;

                for line in event_str.lines() {
                    // NB: this loop needs the `[DONE]` sentinel itself (it
                    // suppresses the upstream terminator and emits its own),
                    // so it can't use `sse::data_payload`, which folds `[DONE]`
                    // into "no payload".
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
                    if let Some(t) = gateway_core::server::sse::usage_tokens(&v) {
                        round_tokens = t;
                        // Hide the trailing usage-only frame (empty `choices`)
                        // from a client that didn't opt into it.
                        if suppress_usage_frame && v.pointer("/choices/0").is_none() {
                            hide_event = true;
                        }
                    }
                    let delta = gateway_core::server::sse::ChatDelta::new(&v);
                    if let Some(t) = delta.content()
                        && content_guard.push(t)
                    {
                        return Err(gateway_core::loop_guard::LOOP_MESSAGE.to_string());
                    }
                    if let Some(t) = delta.reasoning()
                        && reasoning_guard.push(t)
                    {
                        return Err(gateway_core::loop_guard::LOOP_MESSAGE.to_string());
                    }
                    if let Some(tcs) = delta.tool_calls() {
                        hide_event = true;
                        for tc in tcs {
                            let index =
                                tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                            tool_acc.entry(index).or_default().absorb(tc);
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
            .any(|acc| !acc.name.is_empty() && !tool_source.contains(&acc.name));
        if has_client_owned {
            for chunk in synth_client_tool_call_chunks(&chunk_meta, &tool_acc) {
                tx.unbounded_send(Ok(chunk))
                    .map_err(|e| format!("client disconnected: {e}"))?;
            }
            return Ok(());
        }

        let gateway_owned: Vec<runner::ToolCallRef> = tool_acc
            .values()
            .filter(|acc| tool_source.contains(&acc.name))
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

        let results = runner::execute_tool_calls(&tool_source, &tool_ctx, &gateway_owned).await;

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
                        // Normalise before replaying upstream: an empty/garbage
                        // args string (common for no-arg tools) 400s a strict
                        // re-parse (Mistral/`mistral_common`'s `json.loads`).
                        "arguments": runner::normalize_tool_arguments(&call.arguments_raw),
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

/// Rewrite a JSON request body's `model` field to `real_model` — the real id an
/// alias/fallback resolved to — so the upstream (which only knows its own model
/// ids) accepts the request. A no-op when `real_model` already matches. On a
/// JSON parse failure the original bytes pass through unchanged (the upstream
/// then reports the error), mirroring `apply_defaults_to_bytes`.
fn rewrite_model_in_bytes(body: Bytes, real_model: &str) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    match v.get("model").and_then(|m| m.as_str()) {
        Some(current) if current == real_model => body,
        _ => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("model".into(), json!(real_model));
            }
            serde_json::to_vec(&v).map(Bytes::from).unwrap_or(body)
        }
    }
}

/// Set the `model` field of a parsed JSON body to `real_model` (the tool-loop
/// paths carry the body as a `Value`, so no re-parse is needed).
fn set_model_in_value(body: &mut Value, real_model: &str) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".into(), json!(real_model));
    }
}

/// Advertise which real model actually served the request via the
/// `X-Gateway-Resolved-Model` response header — but only when it differs from
/// what the client asked for (i.e. an alias or a fallback fired). Set on the
/// `Response` before it's returned, so it lands in the header block ahead of
/// any streamed body. A non-ASCII model id (never the case for real ids) is
/// silently skipped rather than failing the response.
fn with_resolved_model_header(mut resp: Response, requested: &str, resolved: &str) -> Response {
    if requested != resolved
        && let Ok(val) = rama::http::HeaderValue::from_str(resolved)
    {
        resp.headers_mut().insert("x-gateway-resolved-model", val);
    }
    resp
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
    fn force_usage_injects_on_streaming_and_reports_client_opt_in() {
        // Streaming client that DIDN'T ask for usage → we inject it and flag
        // suppression (tap-but-hide the frame).
        let body = Bytes::from(r#"{"model":"m","stream":true,"messages":[]}"#);
        let (out, suppress) = force_usage_in_body(body);
        assert_eq!(parse(&out)["stream_options"]["include_usage"], json!(true));
        assert!(
            suppress,
            "client didn't opt in → hide the injected usage frame"
        );

        // Streaming client that DID ask → keep it, don't suppress.
        let body = Bytes::from(
            r#"{"model":"m","stream":true,"stream_options":{"include_usage":true},"messages":[]}"#,
        );
        let (out, suppress) = force_usage_in_body(body);
        assert_eq!(parse(&out)["stream_options"]["include_usage"], json!(true));
        assert!(!suppress, "client opted in → forward the usage frame");
    }

    #[test]
    fn force_usage_leaves_non_streaming_untouched() {
        // `stream_options` is invalid without `stream:true`, so a non-streaming
        // body must pass through unchanged (no injection, no suppression).
        let body = Bytes::from(r#"{"model":"m","messages":[]}"#);
        let (out, suppress) = force_usage_in_body(body);
        assert!(parse(&out).get("stream_options").is_none());
        assert!(!suppress);
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
    fn image_edit_units_counts_both_multipart_image_names() {
        let fields = vec![
            MultipartField {
                name: "image[]".into(),
                filename: None,
                content_type: None,
                bytes: Bytes::new(),
            },
            MultipartField {
                name: "image".into(),
                filename: None,
                content_type: None,
                bytes: Bytes::new(),
            },
        ];
        assert_eq!(image_edit_units(&fields), Some(2.0));
    }

    #[test]
    fn zero_provider_duration_uses_measured_duration() {
        assert_eq!(prefer_positive(Some(0.0), Some(4.5)), Some(4.5));
        assert_eq!(prefer_positive(Some(2.0), Some(4.5)), Some(2.0));
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
            ToolCallAcc {
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
