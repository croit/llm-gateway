// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `POST /v1/messages` — the Anthropic Messages API, served by the same
//! pipeline as `/v1/chat/completions`.
//!
//! This is what lets Claude Code (and any other Anthropic-format client) run
//! against the gateway: point `ANTHROPIC_BASE_URL` here, hand it a `gwk_…`
//! token, and it talks to whatever model the operator has configured — with
//! the gateway's routing, per-token model allowlists, rate limits, usage
//! accounting and tool loop all applying unchanged, because *none of that is
//! reimplemented here*. This module is the format edge and nothing else:
//!
//! ```text
//!   Anthropic request  ──[anthropic::request]──▶  OpenAI request
//!                                                      │
//!            (the existing routing / limits / tool loop / usage path)
//!                                                      │
//!   Anthropic response ◀─[anthropic::response]──  OpenAI response
//!   Anthropic SSE      ◀─[AnthropicSink]────────  OpenAI SSE
//! ```
//!
//! ## Gateway tools
//!
//! An Anthropic-format client brings its own tools — Claude Code brings a
//! dozen — and the gateway's tool loop already knows how to split a turn
//! between the tools it owns and the ones the client owns. So the behaviour
//! here is the same as on `/v1/chat/completions`: gateway tools are merged in
//! only when the caller's token has tool use enabled (off by default), in
//! which case the gateway runs its own tools server-side, invisibly, and
//! hands client-owned calls back for the client to run. With tool use off,
//! this endpoint is pure translation.
//!
//! ## Token counting
//!
//! `POST /v1/messages/count_tokens` is answered by asking the *serving backend*
//! to tokenize the translated request — vLLM exposes `POST /tokenize`, which
//! runs the model's own chat template over the messages and tool definitions
//! and returns an exact count.
//!
//! A backend without that endpoint gets a `404` rather than a guess: the
//! client then counts context from the `usage` figures on real responses,
//! which is also exact. Estimating from character counts was the other option,
//! and a confidently wrong number driving a client's compaction decisions is
//! worse than no number at all.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rama::bytes::Bytes;
use rama::http::service::web::extract::State;
use rama::http::service::web::response::IntoResponse;
use rama::http::{Request, Response, StatusCode};
use serde_json::{Value, json};

use gateway_core::server::anthropic::{self, stream::StreamEncoder};
use gateway_core::server::upstreams::PoolKind;
use gateway_core::server::upstreams::registry::RouteError;
use gateway_runtime::rama_server::auth::require_bearer;
use gateway_runtime::rama_server::state::RamaState;
use gateway_runtime::server::tools::runner::{LoopError, ToolCallAcc};

use crate::rama_server::proxy::{self, ChunkMeta, StreamFailure, StreamSink, TokenUsage};

/// How often to send an SSE `ping` while a streamed turn is producing no
/// upstream bytes.
///
/// Claude Code aborts a stream that relays nothing for 300 seconds, counting
/// every byte the gateway sends — including pings. A self-hosted backend
/// sends no keep-alives of its own, and a round that runs a gateway tool (a
/// sandbox command, a web fetch) can be silent for minutes, so the gateway
/// has to produce them. Fifteen seconds is far inside the client's window and
/// costs four frames a minute.
const PING_INTERVAL: Duration = Duration::from_secs(15);

/// `POST /v1/messages`.
pub async fn messages(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    // Source IP for `get_user_location`, captured before the request is split
    // so the socket extension is still reachable. Same as the OpenAI path.
    let client_ip = gateway_features::server::geoip::client_ip(req.headers())
        .or_else(|| gateway_features::server::geoip::peer_ip(&req));
    let (parts, body) = req.into_parts();

    let user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        // `require_bearer` hands back *why* it refused rather than a rendered
        // response, so this says the same thing in the Anthropic envelope —
        // including the internal reasons, which a status code alone can't
        // distinguish.
        Err(refusal) => return error_response(refusal.status, &refusal.message),
    };
    if let Some(exceeded) = proxy::limit_exceeded(&state, &user).await {
        return rate_limited(&exceeded);
    }

    let translated = match translated_request(body).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let requested_model = translated.model.clone();

    // The same tool surface `/v1/chat/completions` resolves, from the same
    // place: gateway tools ride along only when the token has tool use
    // enabled, and this endpoint is pure translation when it doesn't.
    let (allowed_tools, user_mcp) = state.api_tool_layer(&user).await;

    // Resolve aliases + the unknown-model fallback once, up front. This is
    // what makes `claude-sonnet-4-6` (a name no self-hosted backend serves)
    // route to whatever the operator aliased it to.
    let access = state.pool_access_for_token(&user);
    let real_model = match state
        .upstreams
        .route_access(&requested_model, PoolKind::Chat, &access)
    {
        Ok(a) => a.resolved_model().to_string(),
        Err(e) => return route_error_response(e),
    };

    // The model's `model` field becomes the resolved id first, because the
    // admin defaults are keyed on it — an alias inherits its target's
    // settings, exactly as on the OpenAI path. The `Value` form of the merge
    // is the one to use here: the OpenAI path holds the client's raw `Bytes`
    // and needs the bytes variant, but this body was just *built* as a
    // `Value`, so serialising it only to parse it back would be pure waste.
    let mut request_body = translated.body;
    proxy::set_model_in_value(&mut request_body, &real_model);
    if let Err(err) =
        gateway_core::server::model_defaults::apply_defaults(&state.db, &mut request_body).await
    {
        // Same posture as the bytes variant: a broken stored default is not a
        // reason to fail the caller's request.
        tracing::warn!(error = %err, model = %real_model, "model_defaults: skipping merge");
    }
    apply_thinking(&state, &real_model, translated.effort, &mut request_body).await;

    let resp = if translated.stream {
        proxy::stream_with_tools(
            state,
            user,
            real_model.clone(),
            parts.headers,
            client_ip,
            request_body,
            allowed_tools,
            user_mcp,
            Box::new(AnthropicSink::new(&requested_model)),
        )
        .await
    } else {
        buffered(
            state,
            user,
            &requested_model,
            &real_model,
            access,
            parts.headers,
            client_ip,
            request_body,
            allowed_tools,
            user_mcp,
        )
        .await
    };
    proxy::with_resolved_model_header(resp, &requested_model, &real_model)
}

/// Read, parse and translate a request body — the identical prologue both
/// handlers run before they can do anything endpoint-specific. Every failure
/// is the caller's, so they all render as `400` in the Anthropic shape.
async fn translated_request(
    body: rama::http::Body,
) -> Result<anthropic::request::TranslatedRequest, Response> {
    let bytes = proxy::read_body_to_bytes(body)
        .await
        .map_err(|msg| error_response(StatusCode::BAD_REQUEST, &msg))?;
    let request: Value = serde_json::from_slice(&bytes).map_err(|err| {
        error_response(
            StatusCode::BAD_REQUEST,
            &format!("body is not valid JSON: {err}"),
        )
    })?;
    anthropic::request::to_openai(&request)
        .map_err(|err| error_response(StatusCode::BAD_REQUEST, &err.0))
}

/// `POST /v1/messages/count_tokens`.
///
/// Translates the request exactly as [`messages`] would, then asks the backend
/// that would serve it to tokenize the result. That makes the count the real
/// one — the model's own chat template, its own tokenizer, the tool
/// definitions included — rather than an approximation of it.
///
/// The count covers what the *client* sent. Gateway tools the loop would inject
/// at inference time (only when the token has tool use enabled) are not in it:
/// resolving that set means talking to the caller's MCP connectors, which is
/// far too much work for a request whose whole job is to be cheap.
pub async fn count_tokens(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(refusal) => return error_response(refusal.status, &refusal.message),
    };
    // No limit check: counting tokens costs a tokenizer pass, not inference,
    // and a client blocked from counting would fall back to guessing at its
    // own context usage — worse for everyone than answering.
    let translated = match translated_request(body).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    let access = state.pool_access_for_token(&user);
    let acquired = match state
        .upstreams
        .route_access(&translated.model, PoolKind::Chat, &access)
    {
        Ok(a) => a,
        Err(e) => return route_error_response(e),
    };
    let real_model = acquired.resolved_model().to_string();
    let backend = acquired.backend().name.clone();
    let url = tokenize_url(&acquired.backend().base_url);
    let api_key = acquired.backend().api_key.clone();
    // Release the in-flight slot before the call: this is not inference, and
    // holding a chat slot for it would let token counting starve real turns.
    drop(acquired);

    if tokenize_unsupported(&url) {
        return tokenize_unavailable();
    }

    // Built field by field rather than by stripping the inference body: the
    // tokenizer endpoint validates its input strictly, and an allowlist can't
    // be outgrown by a field the translator learns to emit later. The two
    // fields are *moved* out of the translated body — on a Claude Code request
    // they are the whole transcript plus a dozen tool schemas, and this
    // endpoint's entire justification is being cheap.
    let mut body = translated.body;
    let mut payload = json!({"model": real_model, "messages": body["messages"].take()});
    if let Some(tools) = body.get_mut("tools").map(Value::take)
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert("tools".into(), tools);
    }

    let mut http = state.http.post(&url).json(&payload);
    if let Some(key) = api_key.as_deref() {
        http = http.bearer_auth(key);
    }
    let resp = match http.send().await {
        Ok(r) => r,
        Err(err) => {
            tracing::debug!(error = %err, %backend, "count_tokens: tokenize call failed");
            return tokenize_unavailable();
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        // Only "this endpoint isn't here" is worth remembering. A 429 or a 503
        // says the backend is busy, and caching that would disable token
        // counting for the rest of the process over a momentary hiccup.
        if matches!(status.as_u16(), 404 | 405 | 501) {
            remember_tokenize_unsupported(&url);
        }
        tracing::debug!(%status, %backend, "count_tokens: tokenize unavailable");
        return tokenize_unavailable();
    }
    let counted = resp
        .json::<Value>()
        .await
        .ok()
        .and_then(|v| v.get("count").and_then(Value::as_i64));
    match counted {
        Some(count) => json_response(StatusCode::OK, &json!({"input_tokens": count})),
        None => {
            tracing::debug!(%backend, "count_tokens: tokenize response carried no count");
            tokenize_unavailable()
        }
    }
}

/// The backend's tokenizer endpoint. vLLM serves `/tokenize` at the server
/// root, *not* under the OpenAI `/v1` prefix that `base_url` carries — asking
/// for `…/v1/tokenize` is a 404.
fn tokenize_url(base_url: &str) -> String {
    let root = base_url.trim_end_matches('/');
    let root = root.strip_suffix("/v1").unwrap_or(root);
    format!("{root}/tokenize")
}

/// Tokenizer endpoints observed not to exist. A negative cache only: the worst
/// case of a stale entry is that a backend which later gained the endpoint keeps
/// being skipped until the gateway restarts, and the client keeps using the
/// fallback it was already using. Only a status that means *absence* gets an
/// entry — a busy backend is not a missing one.
///
/// Keyed by URL rather than backend name, because the URL is what actually
/// determines whether the endpoint is there: repointing a backend at a new
/// address must not inherit the old address's answer.
///
/// `Backend` in the upstream registry is the codebase's usual home for a
/// learned per-backend fact (it already carries `healthy`, the probed model
/// set, and disabled aliases), and this would sit there naturally but for one
/// thing: the answer only arrives *after* the in-flight slot is released
/// above, deliberately, so that counting tokens can't starve real inference.
/// Reaching the backend again to record it would mean re-acquiring a slot for
/// a bookkeeping write. A URL-keyed map costs one hash lookup and needs
/// nothing held open. Revisit if a second "does this backend support X" probe
/// appears — then the pattern, not this instance, is what wants fixing.
static NO_TOKENIZER: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

fn tokenize_unsupported(url: &str) -> bool {
    NO_TOKENIZER
        .lock()
        .map(|s| s.contains(url))
        .unwrap_or(false)
}

fn remember_tokenize_unsupported(url: &str) {
    if let Ok(mut s) = NO_TOKENIZER.lock() {
        s.insert(url.to_string());
    }
}

/// `404` — the documented way to tell an Anthropic-format client that this
/// optional endpoint isn't available, so it falls back to counting context
/// from response `usage` instead of trusting a number we can't produce.
fn tokenize_unavailable() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        concat!(
            "token counting is not available for this model — the serving backend ",
            "exposes no tokenizer. Count context from the `usage` field of a real ",
            "response instead.",
        ),
    )
}

/// `HEAD /api/hello` — the connection-warming probe an Anthropic-format
/// client sends at startup. Answering it costs nothing and keeps a puzzling
/// `404` out of the gateway's own logs; a client that gets one carries on
/// regardless.
pub async fn hello() -> Response {
    StatusCode::OK.into_response()
}

/// The non-streaming path: run the turn through the shared buffered loop, then
/// translate its result.
#[allow(clippy::too_many_arguments)]
async fn buffered(
    state: Arc<RamaState>,
    user: gateway_core::server::auth::UserCtx,
    requested_model: &str,
    real_model: &str,
    access: gateway_core::server::upstreams::PoolAccess,
    headers: rama::http::HeaderMap,
    client_ip: Option<String>,
    request_body: Value,
    allowed_tools: Vec<String>,
    user_mcp: gateway_runtime::server::tools::mcp::manager::UserMcpLayer,
) -> Response {
    let outcome = match proxy::buffered_with_tools(
        &state,
        &user,
        real_model,
        access,
        headers,
        client_ip,
        request_body,
        &allowed_tools,
        &user_mcp,
    )
    .await
    {
        Ok(o) => o,
        Err(err) => return loop_error_response(err),
    };

    if outcome.status >= 400 {
        // The upstream's own wording survives: the client's
        // retry-without-the-capability recovery matches on it.
        return json_response(
            StatusCode::from_u16(outcome.status).unwrap_or(StatusCode::BAD_GATEWAY),
            &anthropic::error::from_upstream(outcome.status, &outcome.body),
        );
    }

    let completion: Value = match serde_json::from_slice(&outcome.body) {
        Ok(v) => v,
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("upstream returned unparseable JSON: {err}"),
            );
        }
    };
    let message = anthropic::response::from_openai(&completion, requested_model);
    let mut resp = json_response(StatusCode::OK, &message);
    if let Ok(rounds) = rama::http::HeaderValue::from_str(&outcome.rounds.to_string()) {
        resp.headers_mut().insert("x-gateway-tool-rounds", rounds);
    }
    resp
}

/// Translate the request's thinking configuration into the serving model's
/// own reasoning parameter.
///
/// The `thinking` field never reaches the upstream — a vLLM backend rejects
/// it — so the request's *intent* is carried across instead: the gateway's
/// effort levels already know how each backend family spells "think harder"
/// (`chat_template_kwargs.enable_thinking`, `reasoning_effort`,
/// `thinking.budget_tokens`), and an admin can retune the per-level budgets
/// per model on `/admin/models` without touching this path.
async fn apply_thinking(
    state: &RamaState,
    real_model: &str,
    effort: Option<gateway_core::server::reasoning::Effort>,
    body: &mut Value,
) {
    // No `thinking` and no `output_config` in the request: say nothing about
    // reasoning, matching what `/v1/chat/completions` does for a client that
    // sets no reasoning parameter of its own.
    let Some(effort) = effort else {
        return;
    };
    let (style, overrides) =
        gateway_core::server::reasoning::resolve_for_model(&state.db, real_model).await;
    gateway_core::server::reasoning::apply_effort(style, effort, &overrides, body);
}

/// The Anthropic-format [`StreamSink`]: re-encodes the loop's OpenAI chunks
/// as the Anthropic SSE event sequence.
struct AnthropicSink {
    encoder: StreamEncoder,
}

impl AnthropicSink {
    fn new(model: &str) -> Self {
        // The message id is minted here rather than derived from an upstream
        // id: `message_start` goes out before the first backend has answered.
        Self {
            encoder: StreamEncoder::new(
                anthropic::message_id(Some(&uuid::Uuid::new_v4().simple().to_string())),
                model,
            ),
        }
    }
}

/// SSE frames the encoder produced, as body chunks.
fn frames(out: Vec<String>) -> Vec<Bytes> {
    out.into_iter().map(Bytes::from).collect()
}

impl StreamSink for AnthropicSink {
    fn prologue(&mut self) -> Vec<Bytes> {
        frames(self.encoder.start())
    }

    fn visible_chunk(&mut self, chunk: Option<&Value>, _raw: Vec<u8>) -> Vec<Bytes> {
        // An event with no parseable `data:` payload (a comment, a provider's
        // own keep-alive) has nothing to re-encode; our own pings cover the
        // gap it was there to fill.
        chunk
            .map(|c| frames(self.encoder.chunk(c)))
            .unwrap_or_default()
    }

    fn round_usage(&mut self, tokens: TokenUsage) {
        self.encoder
            .absorb_round_usage(tokens.0.unwrap_or(0), tokens.1.unwrap_or(0));
    }

    fn client_tool_calls(
        &mut self,
        _meta: &ChunkMeta,
        acc: &BTreeMap<usize, ToolCallAcc>,
    ) -> Vec<Bytes> {
        acc.values()
            .filter(|call| !call.name.is_empty())
            .flat_map(|call| frames(self.encoder.tool_use(&call.id, &call.name, &call.arguments)))
            .collect()
    }

    fn finish(&mut self) -> Vec<Bytes> {
        frames(self.encoder.finish())
    }

    fn error(&mut self, failure: &StreamFailure) -> Vec<Bytes> {
        // The headers shipped with `message_start`, so the status can only
        // reach the client as the error's *type*. Keeping it is what lets a
        // client tell "the backend was busy" (retry) from "this request is
        // wrong" (don't) — the same mapping the buffered path applies.
        let body = match failure.status {
            Some(status) => anthropic::error::for_status(status, &failure.message),
            None => anthropic::error::envelope("api_error", &failure.message),
        };
        frames(self.encoder.error(&body))
    }

    fn heartbeat(&self) -> Option<(Duration, Bytes)> {
        Some((PING_INTERVAL, Bytes::from(StreamEncoder::ping())))
    }
}

/// A JSON response carrying an already-built Anthropic body.
fn json_response(status: StatusCode, body: &Value) -> Response {
    (
        status,
        [
            ("content-type", "application/json"),
            ("anthropic-version", anthropic::ANTHROPIC_VERSION),
        ],
        body.to_string(),
    )
        .into_response()
}

/// An Anthropic error envelope with the type implied by `status`.
fn error_response(status: StatusCode, message: &str) -> Response {
    json_response(
        status,
        &anthropic::error::for_status(status.as_u16(), message),
    )
}

/// `429` with the `Retry-After` the enforcer computed, in the Anthropic shape.
fn rate_limited(e: &gateway_core::server::limits::LimitExceeded) -> Response {
    let body = anthropic::error::envelope("rate_limit_error", &proxy::limit_message(e));
    let mut resp = json_response(StatusCode::TOO_MANY_REQUESTS, &body);
    if let Ok(secs) = rama::http::HeaderValue::from_str(&e.retry_after_secs.to_string()) {
        resp.headers_mut()
            .insert(rama::http::header::RETRY_AFTER, secs);
    }
    resp
}

/// Routing failures, in the Anthropic shape. The status and the sentence come
/// from the error itself ([`RouteError::status_and_message`]), so this and its
/// OpenAI counterpart cannot disagree about what a given failure means — only
/// about the envelope they put it in.
fn route_error_response(err: RouteError) -> Response {
    let (status, message) = err.status_and_message();
    error_response(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        &message,
    )
}

/// Tool-loop failures, in the Anthropic shape. Same split as above.
fn loop_error_response(err: LoopError) -> Response {
    let (status, message) = err.status_and_message();
    error_response(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        &message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// vLLM serves `/tokenize` at the server root while `base_url` carries the
    /// OpenAI `/v1` prefix — asking for `…/v1/tokenize` is a 404, which would
    /// silently disable token counting against every vLLM backend we run.
    #[test]
    fn the_tokenize_url_drops_the_openai_prefix() {
        assert_eq!(
            tokenize_url("http://backend:8005/v1"),
            "http://backend:8005/tokenize"
        );
        assert_eq!(
            tokenize_url("http://backend:8005/v1/"),
            "http://backend:8005/tokenize"
        );
        // A backend served without the prefix keeps its own root.
        assert_eq!(
            tokenize_url("http://backend:8005"),
            "http://backend:8005/tokenize"
        );
        // Only a trailing `/v1` is a prefix; one inside a path is part of the
        // address the operator configured.
        assert_eq!(
            tokenize_url("https://host/v1/openai/v1"),
            "https://host/v1/openai/tokenize"
        );
    }
}
