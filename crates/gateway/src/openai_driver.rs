// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `SessionDriver` implementation that drives OpenAI-compatible
//! upstream chat-completion calls.
//!
//! This is the body of what used to live in
//! `rama_server::pages::chat::worker::run_inner`: build the message
//! list from DB history, POST a streaming request to whichever
//! upstream backend serves the requested model, parse the SSE chunks
//! into reasoning / content / tool-call deltas, append them to the
//! `chat_turns` row, and — if the model emitted tool calls — execute
//! the gateway-owned ones and round-trip a second model call. Up to
//! `MAX_ROUNDS` rounds per turn.
//!
//! The outer lifecycle (finalize, freeze-reasoning, broadcast
//! `Finalized`) lives in `session_core::worker::run_session_turn`;
//! this file is purely the per-turn work.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use rama::futures::StreamExt;
use session_core::db::{self as chat, ToolCallStatus, Turn, TurnRole, TurnStatus};
use session_core::driver::{SessionContext, SessionDriver, TurnError};
use session_core::workers::TurnUpdate;

use crate::rama_server::state::RamaState;
use crate::server::db::usage::{self, UsageKind, UsageRecord, UsageSource};
use crate::server::tools::{ToolContext, runner};

/// Reasoning tags some vLLM reasoning-parser configs leak into the *content*
/// channel even though reasoning is delivered separately via
/// `reasoning_content`. We strip them so a stray `</think>` never shows up in
/// the rendered answer.
const THINK_TAGS: [&str; 2] = ["<think>", "</think>"];

/// Max gap between upstream SSE chunks before we treat the stream as wedged
/// and finalize the turn as errored. Without it, a provider that opens the
/// response then goes silent (network black-hole, hung worker) leaves the
/// turn `in_progress` forever — the 24h "stuck" turns. It's an *idle* timeout,
/// reset on every chunk, so a long-but-progressing stream (deep reasoning,
/// many tool rounds) is never cut — only a truly silent one. Generous enough
/// to cover queueing + slow time-to-first-token on a loaded backend.
const UPSTREAM_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Pull the safe-to-emit prefix out of `buf`, removing any complete
/// `<think>`/`</think>` tags and holding back a trailing run that could be the
/// start of one (so a tag split across stream deltas is still removed). The
/// held-back tail stays in `buf` for the next delta; flush it at stream end.
fn take_safe_content(buf: &mut String) -> String {
    for tag in THINK_TAGS {
        if buf.contains(tag) {
            *buf = buf.replace(tag, "");
        }
    }
    // Longest suffix of `buf` that is a strict prefix of some tag — keep it.
    let mut hold = 0;
    for tag in THINK_TAGS {
        for k in (1..tag.len()).rev() {
            let cut = buf.len().saturating_sub(k);
            if buf.is_char_boundary(cut) && buf[cut..] == tag[..k] {
                hold = hold.max(k);
                break;
            }
        }
    }
    let split = buf.len() - hold;
    let emit = buf[..split].to_string();
    *buf = buf[split..].to_string();
    emit
}

// Bounded tool-call rounds so a runaway model can't keep us in the loop
// forever. Shared with the `/v1` proxy + buffered runner (one source of
// truth). The *last* round withholds tools (see the loop) so the model is
// forced to produce a final answer instead of ending the turn empty. The cap
// is now per-conversation (derived from the effort level); see
// `server::reasoning::Effort::max_rounds`.

#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

/// Ensure every tool call in one round has a non-empty id that is unique
/// across the whole turn. Some OpenAI-compatible backends (qwen / vLLM are
/// the usual offenders) emit `tool_call_id`s that are empty or recycled
/// per response (`call_0`, `call_0`, … reset every round). Because
/// `chat_tool_calls.id` is the PRIMARY KEY, a duplicate insert aborts the
/// whole turn with `UNIQUE constraint failed: chat_tool_calls.id`. Rewrite
/// any empty or colliding id to a synthesised, turn-unique one *before* it
/// is used for the DB row, the assistant message replayed upstream, and the
/// tool result — so all three keep referring to the same id. `seen`
/// accumulates across rounds of a single turn. Pure so it's unit-tested.
fn ensure_unique_tool_call_ids(
    round_calls: &mut [ToolCallAcc],
    round: usize,
    seen: &mut std::collections::HashSet<String>,
) {
    for (i, acc) in round_calls.iter_mut().enumerate() {
        // `insert` returns false when the id is already taken this turn.
        if acc.id.is_empty() || !seen.insert(acc.id.clone()) {
            let mut synth = format!("call_{round}_{i}");
            while !seen.insert(synth.clone()) {
                synth.push('_');
            }
            acc.id = synth;
        }
    }
}

/// Per-turn driver. Built once by the chat-message handler with the
/// caller's tool context, then boxed into a `dyn SessionDriver` and handed
/// to `session_core::worker::run_session_turn`. Holding `Arc<RamaState>`
/// makes the upstream pool, HTTP client, DB pool, and tool registry
/// reachable inside `run_turn` without taking them as separate
/// arguments.
///
/// `allowed_tools` is re-resolved at the top of every round (cheap SQLite
/// hit) so a mid-turn `enable_tools` call surfaces the newly-enabled
/// schemas on the next round.
pub struct OpenAiDriver {
    pub state: Arc<RamaState>,
    pub tool_ctx: ToolContext,
    /// Which access method this turn belongs to for usage accounting:
    /// `Chat` for the interactive UI, `Scheduled` for a cron-fired run.
    /// (`/v1` callers go through `rama_server::proxy`, not this driver.)
    pub source: UsageSource,
    /// Cap on how many prior turns to replay as history. `None` = replay the
    /// whole session (interactive /chat, and fresh-session scheduled runs).
    /// `Some(n)` keeps only the most recent `n` turns — used by reuse-mode
    /// scheduled runs to bound a long-lived conversation's context.
    pub history_limit: Option<usize>,
    /// Voice-conversation turn: inject a brevity/spoken-style directive so the
    /// reply is short, plain prose the TTS can speak (no markdown/lists/code/
    /// tables). Never stored in `user_content` — it's a per-turn request-time
    /// overlay, so continuing the same thread in text mode is unaffected.
    pub voice_mode: bool,
}

/// Build the per-turn [`ToolContext`] for a persisted chat session. This
/// is the single home for the chat-page and headless-scheduler wirings,
/// which agree on everything except the two interactive-only handles:
///
/// - `client_ip` — the caller's source IP (chat path has one; the
///   scheduler has no request, so `None`).
/// - `chat_feedback` — the live SSE + feedback-hub handles that let
///   `get_user_location` prompt the browser mid-turn (chat path only;
///   `None` headless, where nobody is watching to answer).
///
/// `roles` carries the user's RBAC grant, which is also the tool gate:
/// pass the real roles to grant the user's normal tools, or an empty
/// slice to run with no tools at all (the scheduler's "tools off").
pub fn build_tool_context(
    state: &Arc<RamaState>,
    user_id: String,
    roles: Vec<String>,
    session_id: String,
    assistant_turn_id: String,
    client_ip: Option<String>,
    chat_feedback: Option<crate::server::tools::ChatFeedback>,
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
        assistant_turn_id: Some(assistant_turn_id),
        session_id: Some(session_id),
        client_ip,
        geoip: state.geoip.clone(),
        chat_feedback,
        // Fresh per-turn set so concurrent uploaders (typst,
        // upload_attachment) serialize their filename picks and each get
        // a unique S3 key — see ToolContext docs.
        attachment_reservations: Some(crate::server::chat_attachments::new_reservation_set()),
        indexer: state.indexer.clone(),
        image_gen: Some(crate::server::image_gen::ImageGenerator::new(
            state.upstreams.clone(),
            state.http.clone(),
            state.usage.clone(),
            state.db.clone(),
        )),
        // One lease per turn: successive `run_in_sandbox` calls reuse the same
        // container (so `/work` persists across rounds). `None` when the
        // sandbox isn't configured. Released in `run_turn` at turn end.
        sandbox_lease: state
            .sandbox_client
            .clone()
            .map(crate::server::tools::sandbox::SandboxLease::new),
    }
}

#[async_trait]
impl SessionDriver for OpenAiDriver {
    async fn run_turn(&self, ctx: SessionContext) -> Result<(), TurnError> {
        let result = run_one_turn(self, ctx.clone()).await;
        // Free the turn's sandbox container (if any) here, the single choke
        // point that covers every way `run_one_turn` exits — success, error,
        // and the several early `Ok(())` cancel returns inside it. The
        // `SandboxLease` `Drop` guard + the runner's TTL sweeper are the
        // backstops; this is the prompt, normal path.
        if let Some(lease) = &self.tool_ctx.sandbox_lease {
            lease.release().await;
        }
        // On a clean, non-cancelled completion, check whether this session's
        // context has grown past the compaction threshold and, if so, summarise
        // its oldest turns in the background so the *next* turn replays a smaller
        // prompt. Fire-and-forget: never on the turn's critical path (the worker
        // broadcasts `Finalized` the moment `run_turn` returns), exactly like
        // title generation.
        if result.is_ok() && !ctx.cancel.load(Ordering::SeqCst) {
            let state = self.state.clone();
            let session_id = ctx.session_id.clone();
            // Resolve any alias to the real id so the context window (and thus the
            // auto-compaction trigger) keys on the model that actually ran — an
            // alias carries no settings of its own.
            let model = self
                .state
                .upstreams
                .resolve_model(&ctx.model, crate::server::upstreams::PoolKind::Chat)
                .unwrap_or_else(|| ctx.model.clone());
            tokio::spawn(async move {
                crate::server::compaction::maybe_autocompact(&state, &session_id, &model).await;
            });
        }
        result
    }
}

async fn run_one_turn(d: &OpenAiDriver, ctx: SessionContext) -> Result<(), TurnError> {
    // Build the upstream message list from DB. We include every
    // completed turn before the in-progress one. Tool calls aren't
    // included in the prior-history payload — the old client-side
    // history-collection did the same simplification, and replaying
    // `tool_calls` in OpenAI-format would need their results too,
    // which we'd have to invent if we didn't have them.
    //
    // Attachments — current turn and past turns alike — go upstream
    // as `[attached file=… mime=… size=… id="<turn>/<file>"]` stubs;
    // the model uses the `fetch_attachment` tool to pull the bytes
    // it actually needs. Saves tokens when only a subset of an
    // N-attachment turn matters, keeps S3 reachable only from the
    // gateway (no presigned URLs ever go to the LLM provider), and
    // collapses the "current vs past" branch in `message_for_history`.
    // The user's connected-connector MCP tools, overlaid on the registry for
    // this turn. Built once (cache-warm across rounds); empty + cheap when the
    // user has nothing connected. Built up front so `build_request_context`
    // can advertise the connectors the model could turn on (progressive
    // disclosure — the tools themselves stay out of the request until the
    // model, or the user via the composer, enables the connector).
    let mcp_role_ids = d.state.role_ids_for(&d.tool_ctx.roles);
    let mcp_is_admin = d.state.rbac.is_admin(&mcp_role_ids);
    let user_mcp = d
        .state
        .mcp
        .layer_for_user(
            &d.tool_ctx.user_id,
            &mcp_role_ids,
            mcp_is_admin,
            crate::server::tools::mcp::manager::AskContext::Chat,
        )
        .await;
    let tool_source = crate::server::tools::mcp::manager::CompositeToolSource::new(
        d.state.tools.as_ref(),
        &user_mcp,
    );

    // The conversation's model may be an alias (the picker lists them). Resolve
    // it to the real upstream id ONCE per turn and key every per-model lookup
    // below on it — reasoning style/budgets, sampling defaults, and the outgoing
    // `model` field. An alias therefore carries no settings of its own: it
    // inherits the target's, exactly like cost accounting, which meters the
    // resolved id. Falls through to the requested name when it isn't an alias
    // (or isn't currently served — `route` below then maps the error).
    // Gate model resolution + routing to pools the signed-in user's groups
    // permit, so a chat conversation can't route to a restricted pool.
    let access = d.state.pool_access_for(&d.tool_ctx.roles);
    let real_model = d
        .state
        .upstreams
        .resolve_model_for(
            &ctx.model,
            crate::server::upstreams::PoolKind::Chat,
            &access,
        )
        .unwrap_or_else(|| ctx.model.clone());

    // The conversation's effort level ("Denkaufwand") and the selected model's
    // reasoning style drive both the upstream reasoning parameter and the
    // tool-round cap. Loaded once per turn (sticky per conversation).
    let effort = crate::server::reasoning::Effort::from_db(
        crate::server::db::chat_session_settings::get_effort(&d.state.db, &ctx.session_id)
            .await
            .ok()
            .flatten()
            .as_deref(),
    );
    let (reasoning_style, reasoning_overrides) = {
        let row = crate::server::db::model_defaults::get(&d.state.db, &real_model)
            .await
            .ok()
            .flatten();
        let explicit = row.as_ref().and_then(|r| r.reasoning_style.as_deref());
        let style = crate::server::reasoning::ReasoningStyle::resolve(explicit, &real_model);
        let overrides = row
            .as_ref()
            .map(reasoning_overrides_from_row)
            .unwrap_or_default();
        (style, overrides)
    };
    let max_rounds = effort.max_rounds();

    let turns = chat::list_turns(&d.state.db, &ctx.session_id)
        .await
        .map_err(persist_err("list_turns", &ctx.assistant_turn_id))?;
    // Compaction overlay: when a session has been compacted, its oldest turns
    // (seq <= up_to_seq) are represented by one summary message instead of
    // being replayed verbatim. `None` (never compacted) replays everything as
    // before. A read error degrades to "no compaction" — the turn still runs,
    // just with the full (larger) history.
    let compaction = crate::server::db::chat_compactions::get(&d.state.db, &ctx.session_id)
        .await
        .unwrap_or(None);
    // Build the replayed history: drop the in-progress assistant turn, fold out
    // the compacted prefix (seq <= up_to_seq), apply any `history_limit`, and
    // map to OpenAI-shaped messages. Pure so it's unit-tested below.
    let mut messages = build_history_messages(
        &turns,
        &ctx.assistant_turn_id,
        compaction.as_ref(),
        d.history_limit,
    );

    // Prepend a SINGLE leading system message combining:
    //   - the auto-provided request context (caller's real connection IP, a
    //     coarse IP-based location, timezone) — lets the model answer "what's my
    //     IP / where am I" directly instead of flailing through tools, and
    //     reflects the *true* source IP (correct behind a load balancer); and
    //   - the compaction summary standing in for the folded-out oldest turns.
    // These must be merged into one message, not inserted as two separate
    // `system` turns: some backends (e.g. the Qwen3 vLLM chat template) reject a
    // request with more than one leading system message ("System message must be
    // at the beginning"). See `leading_system_message`.
    let request_context = build_request_context(d, &user_mcp).await;
    let voice_directive = d.voice_mode.then_some(VOICE_DIRECTIVE);
    if let Some(system) = leading_system_message(
        voice_directive,
        request_context,
        compaction.as_ref().map(|c| c.summary.as_str()),
    ) {
        messages.insert(0, system);
    }

    let mut started_reasoning: Option<std::time::Instant> = None;
    let mut frozen_reasoning_elapsed = false;
    // Last `reasoning_elapsed_ms` we persisted, in 100ms buckets. The
    // live "Thinking… (Xs)" timer is server-driven: it only moves when
    // the DB value changes and the bubble re-renders. We bump it on
    // every reasoning chunk, but throttle the write to the 0.1s the
    // label actually displays so a fast token stream doesn't issue a
    // redundant UPDATE per delta. `-1` so the first chunk always writes.
    let mut last_timer_decis: i64 = -1;

    // Email for the usage row, looked up once (best-effort; the user_id is
    // always present even if this read fails). The chat/scheduler paths
    // carry no API token, so token fields stay `None`. Skipped entirely when
    // metrics are disabled — no extra DB read on the kill-switched path.
    let metrics_on = d.state.usage.is_enabled();
    let user_email = if metrics_on {
        crate::server::db::users::find_by_id(&d.state.db, &d.tool_ctx.user_id)
            .await
            .ok()
            .flatten()
            .map(|u| u.email)
            .unwrap_or_default()
    } else {
        String::new()
    };

    // Whether compaction wants the trailing usage frame even when usage metrics
    // are off — the trigger sizes the context from `prompt_tokens`, so we must
    // ask for it. (The threshold check itself still re-reads live config later.)
    let compaction_enabled = d.state.config.chat.compaction.enabled;

    // Largest `prompt_tokens` seen across this turn's rounds — a
    // model-tokenizer-accurate measure of how big the replayed context has
    // grown. Persisted to the turn row and read back by the compaction
    // trigger after the turn completes. A tool-using turn reports several
    // usage frames; the last round's is the biggest (it carries the full
    // history plus every prior round's tool traffic), so a running max is
    // the right summary.
    let mut max_prompt_tokens: i64 = 0;

    // Every `tool_call_id` persisted this turn, so `ensure_unique_tool_call_ids`
    // can spot cross-round collisions (a backend that recycles ids per round).
    let mut seen_tool_call_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for round in 0..max_rounds {
        if ctx.cancel.load(Ordering::SeqCst) {
            return Ok(());
        }

        // On the final allowed round, withhold tools so the model is forced
        // to answer from what it already gathered. Without this, a model that
        // keeps calling tools right up to MAX_ROUNDS exits the loop having
        // just fired more calls — with no round left to consume them — and
        // the turn ends with no visible answer (the "stuck after N tool
        // calls" failure). Withholding tools turns that last round into a
        // guaranteed text answer.
        let final_round = round + 1 == max_rounds;

        // Build the request. `stream: true` so we can forward
        // content deltas; tools injected if the user has any
        // granted.
        // `stream_options.include_usage` asks the upstream for a trailing
        // usage frame (prompt/completion token counts) — we own this
        // request, so unlike the /v1 passthrough we always opt in. It's
        // parsed for metrics below and for the compaction trigger (which reads
        // `prompt_tokens` to size the context), and otherwise ignored (its
        // `choices` is empty, so the delta loop skips it). Requested when
        // *either* usage metrics or compaction is on; omitted only when both
        // are off, so a fully-disabled gateway doesn't alter the request.
        let mut request_body = serde_json::json!({
            "model": ctx.model,
            "messages": messages,
            "stream": true,
        });
        if (metrics_on || compaction_enabled)
            && let Some(obj) = request_body.as_object_mut()
        {
            obj.insert(
                "stream_options".into(),
                serde_json::json!({"include_usage": true}),
            );
        }
        // Send the resolved real id upstream (resolved once per turn, above); the
        // picker may have handed us an alias, which the backend won't recognise.
        if let Some(obj) = request_body.as_object_mut() {
            obj.insert("model".into(), serde_json::json!(real_model.clone()));
        }
        // Re-resolve the per-conversation tool overlay each round so a
        // mid-turn `enable_tools` call surfaces the newly-enabled schemas
        // on the next round. Cheap (sub-ms SQLite hit) and the only way
        // to make the model-driven enablement loop work.
        let mut allowed_tools = d
            .state
            .allowed_tools_for_session(&d.tool_ctx.roles, &d.tool_ctx.user_id, &ctx.session_id)
            .await;
        // Union only the per-user MCP tools whose connector this conversation
        // has turned on (via `enable_tools` or the composer's "+" menu). Unlike
        // the registry tools, connected MCP connectors used to be injected
        // unconditionally; gating them behind the same per-conversation overlay
        // makes them progressive too — the model sees the connectors it *could*
        // enable in the system context, and only the enabled ones cost schema
        // tokens. From the SAME layer the executor uses, so an advertised tool
        // is always dispatchable (no advertise/execute drift).
        let enabled_keys = crate::server::db::chat_session_tools::enabled_keys_for_session(
            &d.state.db,
            &ctx.session_id,
        )
        .await
        .unwrap_or_default();
        d.state
            .union_enabled_mcp_tool_ids(&mut allowed_tools, &user_mcp, &enabled_keys);
        if final_round {
            tracing::info!(
                max_rounds,
                "tool-round budget reached; requesting final answer with tools withheld"
            );
        } else {
            runner::inject_tools(&mut request_body, &tool_source, &allowed_tools)
                .map_err(upstream_err)?;
        }
        // Fill in admin-configured sampling defaults (temperature,
        // top_p, etc.) for keys the chat-page composer didn't set.
        // Same call goes through `proxy.rs` for /v1 callers — keeps
        // the two surfaces in sync. Bad TOML on the stored row gets
        // logged and skipped (the request still goes through).
        if let Err(err) =
            crate::server::model_defaults::apply_defaults(&d.state.db, &mut request_body).await
        {
            tracing::warn!(error = %err, model = %ctx.model, "model_defaults: skipping merge");
        }
        // Translate the conversation's effort level into the model's
        // backend-specific reasoning parameter (after defaults, so the
        // client-wins contract still holds against any stored default).
        crate::server::reasoning::apply_effort(
            reasoning_style,
            effort,
            &reasoning_overrides,
            &mut request_body,
        );
        let serialized = serde_json::to_vec(&request_body).map_err(upstream_err)?;

        let acquired = d
            .state
            .upstreams
            .route_access(
                &real_model,
                crate::server::upstreams::PoolKind::Chat,
                &access,
            )
            .map_err(upstream_err)?;
        let backend = acquired.backend();
        let backend_name = backend.name.clone();
        let url = format!("{}/chat/completions", backend.base_url);
        let mut http_req = d
            .state
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            // `accept-encoding: identity` defeats reqwest's default
            // gzip decompression — a compressed SSE response is
            // buffered until the upstream closes, which is the
            // classic "long replies land all at once" bug.
            .header("accept-encoding", "identity")
            .body(serialized);
        if let Some(key) = backend.api_key.as_deref() {
            http_req = http_req.bearer_auth(key);
        }
        let started = std::time::Instant::now();
        let upstream = http_req.send().await.map_err(transport_err)?;
        if !upstream.status().is_success() {
            let status = upstream.status();
            let bytes = upstream.bytes().await.unwrap_or_default();
            drop(acquired);
            emit_usage(
                d,
                &user_email,
                &ctx.model,
                &backend_name,
                status.as_u16(),
                started,
                (None, None, None),
            );
            return Err(TurnError::Upstream {
                message: format!(
                    "upstream {status}: {}",
                    String::from_utf8_lossy(&bytes)
                        .chars()
                        .take(200)
                        .collect::<String>()
                ),
            });
        }
        let status_code = upstream.status().as_u16();

        let mut round_content = String::new();
        let mut tool_acc: std::collections::BTreeMap<usize, ToolCallAcc> =
            std::collections::BTreeMap::new();
        let mut byte_buf: Vec<u8> = Vec::new();
        let mut traced_first_delta = false;
        let mut traced_first_reasoning = false;
        // Per-round repetition guards (content + reasoning channels). A
        // reasoning model can collapse into emitting one phrase forever;
        // without this the turn streams for minutes and ends empty.
        let mut content_guard = crate::loop_guard::LoopGuard::new();
        let mut reasoning_guard = crate::loop_guard::LoopGuard::new();
        // Carry buffer for stripping stray `<think>`/`</think>` tags out of the
        // content channel without breaking on a tag split across deltas.
        let mut content_tag_buf = String::new();
        // Token counts from the trailing `usage` frame (we set include_usage).
        let mut round_tokens: (Option<i64>, Option<i64>, Option<i64>) = (None, None, None);
        let mut upstream_stream = upstream.bytes_stream();

        'chunks: loop {
            // Bound the wait for each chunk so a silently-wedged upstream
            // can't pin the turn `in_progress` forever; a real stall finalizes
            // as errored instead. The timer resets per chunk (see the const).
            let chunk =
                match tokio::time::timeout(UPSTREAM_STALL_TIMEOUT, upstream_stream.next()).await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break 'chunks,
                    Err(_) => {
                        emit_usage(
                            d,
                            &user_email,
                            &ctx.model,
                            &backend_name,
                            status_code,
                            started,
                            round_tokens,
                        );
                        return Err(TurnError::Transport {
                            message: format!(
                                "upstream stalled — no data received for {}s",
                                UPSTREAM_STALL_TIMEOUT.as_secs()
                            ),
                        });
                    }
                };
            if ctx.cancel.load(Ordering::SeqCst) {
                drop(acquired);
                return Ok(());
            }
            let Ok(chunk) = chunk else { break 'chunks };
            byte_buf.extend_from_slice(&chunk);

            while let Some(idx) = byte_buf.windows(2).position(|w| w == b"\n\n") {
                let event_bytes: Vec<u8> = byte_buf.drain(..idx + 2).collect();
                let event = String::from_utf8_lossy(&event_bytes[..event_bytes.len() - 2]);
                for line in event.lines() {
                    let Some(payload) = line.strip_prefix("data:").map(str::trim_start) else {
                        continue;
                    };
                    if payload == "[DONE]" {
                        continue;
                    }
                    let v: serde_json::Value = match serde_json::from_str(payload) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    // The trailing usage frame carries token counts and an
                    // empty `choices` — grab it before the delta guard below
                    // skips choice-less frames.
                    if v.get("usage").is_some_and(|u| !u.is_null()) {
                        round_tokens = usage::usage_from_value(&v);
                    }
                    let delta = match v.pointer("/choices/0/delta") {
                        Some(d) => d,
                        None => continue,
                    };
                    if !traced_first_delta {
                        let keys: Vec<&str> = delta
                            .as_object()
                            .map(|o| o.keys().map(|s| s.as_str()).collect())
                            .unwrap_or_default();
                        tracing::info!(?keys, model = %ctx.model, "chat-stream: first upstream delta");
                        traced_first_delta = true;
                    }

                    // Reasoning. vLLM emits this on its
                    // `--reasoning-parser` adapters as either
                    // `reasoning_content` or `reasoning`.
                    let reasoning_chunk = delta
                        .get("reasoning_content")
                        .and_then(|c| c.as_str())
                        .or_else(|| delta.get("reasoning").and_then(|c| c.as_str()));
                    if let Some(reasoning) = reasoning_chunk {
                        if !traced_first_reasoning {
                            tracing::info!(
                                len = reasoning.len(),
                                "chat-stream: first reasoning chunk"
                            );
                            traced_first_reasoning = true;
                        }
                        if started_reasoning.is_none() {
                            started_reasoning = Some(std::time::Instant::now());
                        }
                        chat::append_reasoning(&d.state.db, &ctx.assistant_turn_id, reasoning)
                            .await
                            .map_err(persist_err("append_reasoning", &ctx.assistant_turn_id))?;
                        // Advance the live thinking timer as reasoning
                        // streams, so the label ticks up instead of
                        // freezing until the first content delta. Frozen
                        // once content arrives (or never, for a
                        // reasoning-only turn — the last bump stands).
                        if let Some(start) = started_reasoning
                            && !frozen_reasoning_elapsed
                        {
                            let elapsed_ms = start.elapsed().as_millis() as i64;
                            if elapsed_ms / 100 != last_timer_decis {
                                last_timer_decis = elapsed_ms / 100;
                                chat::set_reasoning_elapsed(
                                    &d.state.db,
                                    &ctx.assistant_turn_id,
                                    elapsed_ms,
                                )
                                .await
                                .map_err(persist_err(
                                    "set_reasoning_elapsed",
                                    &ctx.assistant_turn_id,
                                ))?;
                            }
                        }
                        let _ = ctx.broadcast.send(TurnUpdate::Tick);
                        if reasoning_guard.push(reasoning) {
                            // Drop the upstream stream (closes the
                            // connection) and finalize the turn as errored
                            // with a clear message. The partial reasoning
                            // already streamed stays visible. The backend
                            // call was real, so still record it.
                            emit_usage(
                                d,
                                &user_email,
                                &ctx.model,
                                &backend_name,
                                status_code,
                                started,
                                round_tokens,
                            );
                            return Err(TurnError::Aborted {
                                message: crate::loop_guard::LOOP_MESSAGE.into(),
                            });
                        }
                    }

                    // Content. The first content delta of the turn
                    // freezes the reasoning timer.
                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                        if let Some(start) = started_reasoning
                            && !frozen_reasoning_elapsed
                        {
                            let elapsed_ms = start.elapsed().as_millis() as i64;
                            chat::set_reasoning_elapsed(
                                &d.state.db,
                                &ctx.assistant_turn_id,
                                elapsed_ms,
                            )
                            .await
                            .map_err(persist_err(
                                "set_reasoning_elapsed",
                                &ctx.assistant_turn_id,
                            ))?;
                            frozen_reasoning_elapsed = true;
                        }
                        // Strip stray reasoning tags leaked into content.
                        content_tag_buf.push_str(content);
                        let emit = take_safe_content(&mut content_tag_buf);
                        if !emit.is_empty() {
                            round_content.push_str(&emit);
                            chat::append_content(&d.state.db, &ctx.assistant_turn_id, &emit)
                                .await
                                .map_err(persist_err("append_content", &ctx.assistant_turn_id))?;
                            let _ = ctx.broadcast.send(TurnUpdate::Tick);
                            if content_guard.push(&emit) {
                                emit_usage(
                                    d,
                                    &user_email,
                                    &ctx.model,
                                    &backend_name,
                                    status_code,
                                    started,
                                    round_tokens,
                                );
                                return Err(TurnError::Aborted {
                                    message: crate::loop_guard::LOOP_MESSAGE.into(),
                                });
                            }
                        }
                    }

                    // tool_calls accumulation.
                    if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
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
                }
            }
        }
        drop(acquired);

        // One usage row per upstream round (a tool-using turn emits several).
        emit_usage(
            d,
            &user_email,
            &ctx.model,
            &backend_name,
            status_code,
            started,
            round_tokens,
        );

        // Track the context size for the compaction trigger. Persisted only
        // when it grows, so a tool-using turn writes at most once per round
        // and a normal turn writes once. Best-effort — a failed write just
        // leaves the trigger reading a slightly stale value next turn.
        if let Some(prompt) = round_tokens.0
            && prompt > max_prompt_tokens
        {
            max_prompt_tokens = prompt;
            if let Err(err) =
                chat::set_context_tokens(&d.state.db, &ctx.assistant_turn_id, prompt).await
            {
                tracing::warn!(error = %err, "chat-stream: persisting context_tokens failed");
            }
        }

        // Flush any held-back content tail (a partial tag that never
        // completed is real content, minus any complete tag still in it).
        if !content_tag_buf.is_empty() {
            for tag in THINK_TAGS {
                content_tag_buf = content_tag_buf.replace(tag, "");
            }
            if !content_tag_buf.is_empty() {
                round_content.push_str(&content_tag_buf);
                chat::append_content(&d.state.db, &ctx.assistant_turn_id, &content_tag_buf)
                    .await
                    .map_err(persist_err("append_content_flush", &ctx.assistant_turn_id))?;
                let _ = ctx.broadcast.send(TurnUpdate::Tick);
            }
        }

        if ctx.cancel.load(Ordering::SeqCst) {
            return Ok(());
        }

        // End of round. If no tool calls, we're done.
        if tool_acc.is_empty() {
            return Ok(());
        }

        // Tool calls fired. Insert each as 'running' and broadcast,
        // then execute concurrently. Each result flips its row to
        // 'completed' / 'errored'.
        let mut collected: Vec<ToolCallAcc> = tool_acc.into_values().collect();
        // Guarantee unique, non-empty ids before they hit the DB (PK),
        // the replayed assistant message, and the tool results.
        ensure_unique_tool_call_ids(&mut collected, round as usize, &mut seen_tool_call_ids);
        // Tool groups the user explicitly switched **off** for this
        // conversation. The model never sees their schemas (they're not in
        // `allowed_tools`), but it can still hallucinate a direct call from
        // training priors — refuse those without executing, so an Off toggle
        // is a hard block, not a soft default. A DB hiccup degrades open.
        let disabled_keys = match d.tool_ctx.session_id.as_deref() {
            Some(sid) => {
                crate::server::db::chat_session_tools::disabled_keys_for_session(&d.state.db, sid)
                    .await
                    .unwrap_or_default()
            }
            None => Default::default(),
        };
        let mut assistant_tool_calls: Vec<serde_json::Value> = Vec::new();
        let mut call_refs: Vec<runner::ToolCallRef> = Vec::new();
        // Calls refused because the tool is user-disabled: (id, reason). Each
        // still needs a `tool` message so the assistant turn's tool_calls all
        // resolve — appended after the assistant message below.
        let mut refused: Vec<(String, String)> = Vec::new();
        for acc in &collected {
            chat::insert_running_tool_call(
                &d.state.db,
                &ctx.assistant_turn_id,
                &acc.id,
                &acc.name,
                &acc.arguments,
            )
            .await
            .map_err(persist_err(
                "insert_running_tool_call",
                &ctx.assistant_turn_id,
            ))?;
            let _ = ctx.broadcast.send(TurnUpdate::Tick);

            assistant_tool_calls.push(serde_json::json!({
                "id": acc.id.clone(),
                "type": "function",
                "function": {
                    "name": acc.name.clone(),
                    // Normalise before replaying upstream: an empty/garbage
                    // args string (common for no-arg tools like
                    // `rag_list_collections`) 400s a strict re-parse.
                    "arguments": runner::normalize_tool_arguments(&acc.arguments),
                }
            }));
            if crate::server::tools::ToolSource::contains(&tool_source, &acc.name) {
                let key = crate::server::tools::catalog::entry_key_for(&acc.name);
                // Hard block: the user switched this tool off for the
                // conversation. Don't run it, don't auto-enable it — answer
                // the call with a refusal the model can read and adapt to.
                if disabled_keys.contains(key) {
                    let reason = "This tool is disabled by the user for this conversation; it cannot be \
                         used here.";
                    if let Err(err) = chat::complete_tool_call(
                        &d.state.db,
                        &acc.id,
                        reason,
                        ToolCallStatus::Errored,
                    )
                    .await
                    {
                        tracing::warn!(error = %err, tool = %acc.name, "recording refused tool call");
                    }
                    let _ = ctx.broadcast.send(TurnUpdate::Tick);
                    refused.push((acc.id.clone(), reason.to_string()));
                    continue;
                }
                // Implicit miss-recovery: the model called a tool whose
                // schema wasn't in this round's tools array — it's
                // guessing from training (`fetch_url(url=...)` is the
                // common case). Write a sticky enablement row so the
                // schema appears in the next round's tools array; the
                // call itself still runs with whatever args the model
                // produced (often correct for well-known tools; if not,
                // the InvalidArgs reply now has a real schema to retry
                // against). Same round-trip cost as if the model had
                // called `enable_tools` itself.
                if !allowed_tools.contains(&acc.name)
                    && let Some(session_id) = d.tool_ctx.session_id.as_deref()
                {
                    if let Err(err) = crate::server::db::chat_session_tools::set(
                        &d.state.db,
                        session_id,
                        key,
                        true,
                        "auto-call",
                    )
                    .await
                    {
                        tracing::warn!(
                            error = %err, tool = %acc.name, key,
                            "auto-enable on direct call: persist failed"
                        );
                    } else {
                        tracing::info!(
                            tool = %acc.name, key,
                            "auto-enabled tool the model called without going through enable_tools"
                        );
                    }
                }
                call_refs.push(runner::ToolCallRef {
                    id: acc.id.clone(),
                    name: acc.name.clone(),
                    arguments_raw: acc.arguments.clone(),
                });
            } else {
                // The model called a tool we don't own — almost always a name
                // it invented (the common case is an MCP capability id called
                // as if it were a tool, instead of through the connector's
                // `invoke_capability`). Left alone, the 'running' row we just
                // inserted renders as "Calling" forever and the call goes
                // unanswered. Complete it as errored and reply with a message
                // the model can recover from — exactly like the user-disabled
                // path above (so the assistant turn's tool_calls all resolve
                // and a single unknown call no longer dead-ends the turn).
                let reason = format!(
                    "No tool named `{}` is available in this conversation. Only call tools that \
                     were provided to you. If you meant to use an MCP capability, call the \
                     connector's invocation tool (e.g. `invoke_capability`) with the capability \
                     id as an argument — do not call the capability id as if it were its own tool.",
                    acc.name
                );
                if let Err(err) =
                    chat::complete_tool_call(&d.state.db, &acc.id, &reason, ToolCallStatus::Errored)
                        .await
                {
                    tracing::warn!(error = %err, tool = %acc.name, "recording unknown tool call");
                }
                let _ = ctx.broadcast.send(TurnUpdate::Tick);
                tracing::debug!(
                    wire_name = %acc.name,
                    "chat-stream got tool_call for a tool we don't own; answered with an error"
                );
                refused.push((acc.id.clone(), reason));
            }
        }
        if call_refs.is_empty() && refused.is_empty() {
            return Ok(());
        }

        let results = runner::execute_tool_calls(&tool_source, &d.tool_ctx, &call_refs).await;
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": serde_json::Value::Null,
            "tool_calls": assistant_tool_calls,
        }));
        // Refused (user-disabled) calls still need a `tool` response so the
        // assistant turn's tool_calls all resolve — emit them as errors the
        // model can read.
        for (id, reason) in &refused {
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": serde_json::Value::String(reason.clone()),
            }));
        }
        for (call, result) in call_refs.iter().zip(results.iter()) {
            // For the operator UI / DB log we always store a
            // pretty-printed JSON snapshot — even when the tool returned
            // mixed content parts (the parts envelope itself is JSON,
            // so this works for both shapes and the operator sees the
            // exact bytes that went upstream).
            let output_str =
                serde_json::to_string_pretty(&result.body).unwrap_or_else(|_| "{}".to_string());
            chat::complete_tool_call(
                &d.state.db,
                &call.id,
                &output_str,
                ToolCallStatus::Completed,
            )
            .await
            .map_err(persist_err("complete_tool_call", &ctx.assistant_turn_id))?;
            let _ = ctx.broadcast.send(TurnUpdate::Tick);
            // If the tool returned a `tool_content_parts(...)` envelope
            // we splice it into the message as array content (so a
            // vision-capable upstream actually gets `image_url` bytes
            // back). Otherwise fall back to stringified JSON — the
            // pre-existing contract.
            let content = match crate::server::tools::extract_content_parts(&result.body) {
                Some(parts) => {
                    let (replaced, notification) =
                        crate::server::capabilities::maybe_replace_image_content(
                            parts,
                            &real_model,
                            &d.state.db,
                            &d.state.http,
                            d.state.upstreams.as_ref(),
                        )
                        .await;
                    if let Some(msg) = notification {
                        tracing::info!(model = %real_model, "vision fallback activated");
                        let _ = ctx.broadcast.send(TurnUpdate::InfoMessage(msg));
                    }
                    serde_json::Value::Array(replaced)
                }
                None => serde_json::Value::String(output_str),
            };
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": &call.id,
                "content": content,
            }));
        }
    }
    Ok(())
}

/// Build the auto-provided request-context system message: the signed-in
/// user's identity (name + email), source IP, a coarse IP-based location,
/// and their timezone — whatever is known. Returns `None` when nothing is
/// known so we don't prepend an empty message. Identity, name and timezone
/// come from the user row (one read); the IP comes from `ToolContext`
/// (proxy header or socket peer); the coarse location reuses the same GeoIP
/// resolver the `get_user_location` tool uses.
async fn build_request_context(
    d: &OpenAiDriver,
    user_mcp: &crate::server::tools::mcp::manager::UserMcpLayer,
) -> Option<String> {
    use std::fmt::Write as _;

    let ip = d.tool_ctx.client_ip.as_deref();
    let geo = ip.and_then(|ip| d.tool_ctx.geoip.as_ref()?.lookup(ip));
    // One user-row read serves identity + timezone (the row is loaded here
    // anyway). Identity (name/email) lets the model act AS the signed-in
    // user — e.g. fill the sender/signature of a letter — without asking.
    let user = crate::server::db::users::find_by_id(&d.state.db, &d.tool_ctx.user_id)
        .await
        .ok()
        .flatten();
    let name = user.as_ref().and_then(|u| u.name.clone());
    let email = user
        .as_ref()
        .map(|u| u.email.clone())
        .filter(|e| !e.is_empty());
    let timezone = user.as_ref().and_then(|u| u.timezone.clone());

    // Skills the caller's roles permit: not-yet-loaded ones advertised as
    // `name: description` (the model loads via `read_skill`); already-loaded
    // ones re-injected with their full guidance so it persists across turns.
    let skills = build_skills_section(d).await;

    // Connected MCP integrations the conversation hasn't turned on yet —
    // advertised cheaply (name + one-liner) so the model knows it can request
    // them via `enable_tools` without their full tool schemas costing tokens
    // every turn (progressive disclosure for per-user MCP).
    let integrations = build_mcp_offer_section(d, user_mcp).await;

    if ip.is_none()
        && geo.is_none()
        && timezone.is_none()
        && name.is_none()
        && email.is_none()
        && skills.is_none()
        && integrations.is_none()
    {
        return None;
    }

    // High-level capability areas this deployment actually offers, derived
    // from the live registry (so we never advertise an absent sandbox/indexer)
    // — domains, not tools, to keep the hint cheap. The model still calls
    // `enable_tools` for the exact keys; connected MCP integrations and skills
    // are listed separately below, so they're excluded here.
    let domains = crate::server::tools::catalog::capability_domains(d.state.tools.as_ref());
    let domains_line = if domains.is_empty() {
        String::new()
    } else {
        format!(
            "Built-in capability areas you can turn on: {}. ",
            domains.join(", ")
        )
    };

    let mut out = format!(
        "Automatically provided context about the signed-in user making this request. \
         When they ask you to act on their behalf — e.g. as the sender/signature of a \
         letter or document — use their name and email below; do not invent a name or \
         ask for it. Also use this to personalise replies and to answer questions about \
         their IP address, approximate location, or local time directly — do not fetch \
         external services or search the web for these.\n\
         \n\
         Your `tools` list is intentionally minimal: only `enable_tools` is on by \
         default; every other capability starts off and must be turned on. \
         {domains_line}Call `enable_tools(keys)` FIRST whenever the user's request needs a \
         capability that isn't already in your tools list — its description lists every \
         available key (and any connected integrations or skills are noted below). \
         Enablement is sticky for this conversation, so you only pay the turn-on cost \
         once per capability.\n",
    );
    if let Some(name) = &name {
        let _ = writeln!(out, "- Name: {name}");
    }
    if let Some(email) = &email {
        let _ = writeln!(out, "- Email: {email}");
    }
    if let Some(ip) = ip {
        let _ = writeln!(out, "- IP address (the request's source): {ip}");
    }
    if let Some(g) = &geo {
        let place = [g.city.as_deref(), g.region.as_deref(), g.country.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
        let coords = match (g.latitude, g.longitude) {
            (Some(la), Some(lo)) => format!(" (lat {la:.4}, lon {lo:.4})"),
            _ => String::new(),
        };
        let place = if place.is_empty() {
            "unknown".to_string()
        } else {
            place
        };
        let _ = writeln!(
            out,
            "- Approximate location (from IP, city-level): {place}{coords}"
        );
    }
    if let Some(tz) = &timezone {
        let _ = writeln!(out, "- Timezone: {tz}");
    }
    if let Some(skills) = skills {
        out.push_str(&skills);
    }
    if let Some(integrations) = integrations {
        out.push_str(&integrations);
    }
    Some(out)
}

/// The integrations section of the request-context message: the user's
/// connected MCP connectors that this conversation hasn't turned on yet, each
/// as `mcp__<key> — <name>: <description>`. Returns `None` when the user has no
/// connected connectors, or all of them are already enabled (nothing to
/// advertise). The model turns one on with `enable_tools(["mcp__<key>"])`; its
/// real tool schemas then appear from the next turn. Connector display copy
/// comes from the admin catalog; only connectors actually present in the live
/// `UserMcpLayer` are listed, so an advertised key is always connectable.
async fn build_mcp_offer_section(
    d: &OpenAiDriver,
    user_mcp: &crate::server::tools::mcp::manager::UserMcpLayer,
) -> Option<String> {
    use std::fmt::Write as _;

    let connector_keys = user_mcp.connector_keys();
    if connector_keys.is_empty() {
        return None;
    }
    // Which connectors this conversation has already enabled — those need no
    // advertising (their tools are already injected). Chat path only.
    let enabled = match d.tool_ctx.session_id.as_deref() {
        Some(session_id) => {
            crate::server::db::chat_session_tools::enabled_keys_for_session(&d.state.db, session_id)
                .await
                .unwrap_or_default()
        }
        None => std::collections::HashSet::new(),
    };

    let mut rows = String::new();
    for key in &connector_keys {
        let toggle_key = format!("{}{key}", crate::server::tools::mcp::MCP_ID_PREFIX);
        if enabled.contains(&toggle_key) {
            continue;
        }
        // Display name + description from the admin catalog; fall back to the
        // connector key alone if the row is gone (deleted connector still
        // connected for this user).
        let (name, desc) = match crate::server::db::mcp_catalog::get(&d.state.db, key).await {
            Ok(Some(c)) => (c.name, c.description.unwrap_or_default()),
            _ => (key.clone(), String::new()),
        };
        if desc.is_empty() {
            let _ = writeln!(rows, "- {toggle_key} — {name}");
        } else {
            let _ = writeln!(rows, "- {toggle_key} — {name}: {desc}");
        }
    }
    if rows.is_empty() {
        return None;
    }
    Some(format!(
        "\nIntegrations the user has connected and you can turn on with \
         `enable_tools([\"<key>\"])` (their tools then appear next turn):\n{rows}"
    ))
}

/// The skills section of the request-context message: every skill the
/// caller's roles permit, as `name: description`. Returns `None` when no
/// skills are loaded or the caller's roles grant none — the listing is then
/// omitted entirely (and the always-on `read_skill` rule in
/// `AppState::allowed_tools_for_session` likewise sees an empty set, so the
/// loader tool isn't injected either). Names come straight from the loaded
/// registry; descriptions are the bundle authors' own, written to trigger
/// the model — so no language-specific keyword matching lives here.
async fn build_skills_section(d: &OpenAiDriver) -> Option<String> {
    // Combined registry (private overlaid on global, private shadows global) so
    // a name resolves to the same bundle the caller was advertised — global
    // operator skills plus this user's own private skills.
    let registry = d.state.combined_skills_for(&d.tool_ctx.user_id)?;
    let allowed = d
        .state
        .allowed_skills_for(&d.tool_ctx.roles, &d.tool_ctx.user_id);
    if allowed.is_empty() {
        return None;
    }
    // Skills already loaded in this conversation (sticky). Chat path only;
    // a DB hiccup degrades to "nothing loaded" (the model just reloads).
    let loaded: Vec<String> = match d.tool_ctx.session_id.as_deref() {
        Some(session_id) => {
            crate::server::db::chat_session_skills::loaded_for_session(&d.state.db, session_id)
                .await
                .unwrap_or_default()
        }
        None => Vec::new(),
    };
    // Loaded ∩ permitted, in load order (RBAC re-checked here, so a
    // since-revoked skill drops out even with a stale row). Not-loaded =
    // the rest of the permitted set, advertised for the model to load.
    let loaded_allowed: Vec<String> = loaded
        .iter()
        .filter(|n| allowed.iter().any(|a| a == *n))
        .cloned()
        .collect();
    let not_loaded: Vec<String> = allowed
        .iter()
        .filter(|n| !loaded_allowed.iter().any(|l| l == *n))
        .cloned()
        .collect();

    let mut out = String::new();
    if let Some(listing) = render_skill_listing(&registry, &not_loaded) {
        out.push_str(&listing);
    }
    if let Some(active) = render_active_skills(&registry, &loaded_allowed) {
        out.push_str(&active);
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Format the skills section from a registry and the caller's permitted
/// skill names. Pure (no `AppState`) so the wiring is unit-testable.
/// Returns `None` when `allowed` is empty — the section is then omitted
/// entirely, and `read_skill` likewise stays out of the tools list.
fn render_skill_listing(
    registry: &crate::server::skills::SkillRegistry,
    allowed: &[String],
) -> Option<String> {
    use std::fmt::Write as _;

    if allowed.is_empty() {
        return None;
    }
    let mut s = String::from(
        "\nInstalled skills — each is operator-provided guidance for a kind of task. When \
         the user's request matches what a skill is for, call `read_skill(name)` to load \
         its full instructions BEFORE you produce the output, then `read_skill(name, path)` \
         for any reference or asset file it names. Available skills:\n",
    );
    for name in allowed {
        if let Some(skill) = registry.get(name) {
            let _ = writeln!(s, "- {}: {}", skill.name, skill.description);
        }
    }
    Some(s)
}

/// Re-inject the full guidance of skills already loaded this conversation, so
/// it keeps applying without the model re-reading (the sticky half of Agent
/// Skills). Each skill's `SKILL.md` body is read fresh and spliced in under a
/// header; a body that fails to read is skipped (the listing path still lets
/// the model reload it). Pure apart from the per-skill file read, so the
/// formatting is unit-testable via [`render_active_skills`] over a temp
/// bundle. Returns `None` when nothing is loaded.
fn render_active_skills(
    registry: &crate::server::skills::SkillRegistry,
    loaded: &[String],
) -> Option<String> {
    use std::fmt::Write as _;

    let mut s = String::new();
    for name in loaded {
        let Some(skill) = registry.get(name) else {
            continue;
        };
        let Ok(body) = skill.body() else {
            continue;
        };
        if s.is_empty() {
            s.push_str(
                "\nActive skills — you have loaded these; apply their guidance to what you \
                 produce. Use `read_skill(name, path)` to pull any reference or asset file \
                 they mention.\n",
            );
        }
        let _ = write!(s, "\n### Skill: {}\n{}\n", skill.name, body.trim_end());
    }
    if s.is_empty() { None } else { Some(s) }
}

/// Convert a persisted turn into the OpenAI-format message for the
/// upstream payload. User turns map to `{role: "user", content: …}`
/// with every `[gw-attachment …]` marker rewritten to an opaque-id
/// stub the model resolves via `fetch_attachment`; completed
/// assistant turns map to `{role: "assistant", content: …}` when
/// they have any text content; in-progress / cancelled / errored
/// turns are skipped (their content is partial or absent).
/// Build the prior-history message list replayed upstream: every turn before
/// the in-progress assistant turn, with the compacted prefix (`seq <=
/// up_to_seq`) folded out and `history_limit` applied to the verbatim tail.
///
/// Returns just the `[user/assistant …]` tail — the caller prepends a single
/// leading system message (request context + compaction summary) via
/// [`leading_system_message`]. Pure (no I/O) so the fold contract is unit-tested
/// directly.
fn build_history_messages(
    turns: &[session_core::db::TurnWithTools],
    assistant_turn_id: &str,
    compaction: Option<&crate::server::db::chat_compactions::Compaction>,
    history_limit: Option<usize>,
) -> Vec<serde_json::Value> {
    // Prior turns, oldest-first, minus the in-progress assistant turn, minus
    // any turns folded into the summary.
    let prior: Vec<_> = turns
        .iter()
        .filter(|t| t.turn.id != assistant_turn_id)
        .filter(|t| match compaction {
            Some(c) => t.turn.seq > c.up_to_seq,
            None => true,
        })
        .collect();
    // `history_limit` keeps only the most recent N turns (reuse-mode scheduled
    // runs); `None` replays them all.
    let kept = match history_limit {
        Some(n) => &prior[prior.len().saturating_sub(n)..],
        None => &prior[..],
    };
    kept.iter()
        .filter_map(|t| message_for_history(&t.turn))
        .collect()
}

/// Voice-conversation brevity/format directive. Injected only for voice-mode
/// turns (see [`OpenAiDriver::voice_mode`]) so the reply is short spoken prose
/// the TTS can read. Instructs the model to answer in the user's spoken
/// language — the gateway shapes the *format*, not the content.
const VOICE_DIRECTIVE: &str = "You are in a live VOICE conversation. Everything you write is \
read aloud to the user, so it must sound like a person speaking, not a written article.\n\
- Be extremely brief: normally ONE or TWO spoken sentences, under about 40 words. Answer the \
question directly, then stop. Do not pad, recap, or list.\n\
- Never narrate what you are about to do or how you got the answer. Do NOT say things like \
\"I'll search for that\", \"Let me look that up\", \"Sure, here's what I found\", or mention \
your tools, steps, or sources. Any such planning belongs in your private reasoning, never in \
the spoken reply — the user only wants the answer itself.\n\
- If you use a tool or search, do it silently and speak only the conclusion. NEVER read search \
results, pages, or documents back word for word; distil them to one or two sentences in your own \
words.\n\
- Plain spoken prose only: no Markdown, no bullet or numbered lists, no headings, no code \
blocks, no tables, no emoji, and never read out URLs.\n\
- Reply in the same language the user just spoke.\n\
- If a complete answer would genuinely be long or need code or a table, give a one-sentence \
spoken summary, say the details are on screen, and offer to go deeper only if they ask.";

/// Compose the single leading `system` message from the optional voice
/// directive, request context, and compaction summary. Returns `None` when all
/// are absent (so no empty system message is sent).
///
/// All must live in ONE system message: some backends (notably the Qwen3 vLLM
/// chat template) reject a request carrying more than one leading system turn
/// ("System message must be at the beginning"). Merging keeps a single system
/// turn regardless of which parts are present. Pure so it's unit-tested.
fn leading_system_message(
    voice_directive: Option<&str>,
    request_context: Option<String>,
    summary: Option<&str>,
) -> Option<serde_json::Value> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(directive) = voice_directive {
        parts.push(directive.to_string());
    }
    if let Some(ctx) = request_context {
        parts.push(ctx);
    }
    if let Some(summary) = summary {
        parts.push(format!(
            "Summary of the earlier part of this conversation (older messages have been \
             condensed to save context; treat this as established context and continue \
             seamlessly):\n\n{summary}"
        ));
    }
    if parts.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "role": "system",
        "content": parts.join("\n\n---\n\n"),
    }))
}

fn message_for_history(turn: &Turn) -> Option<serde_json::Value> {
    match turn.role {
        TurnRole::User => {
            let raw = turn.user_content.clone().unwrap_or_default();
            let content = crate::server::chat_attachments::strip_markers_for_replay(&raw, &turn.id);
            Some(serde_json::json!({
                "role": "user",
                "content": content,
            }))
        }
        TurnRole::Assistant => {
            if turn.status != TurnStatus::Completed {
                return None;
            }
            let content = turn.content.clone()?;
            if content.is_empty() {
                return None;
            }
            Some(serde_json::json!({
                "role": "assistant",
                "content": content,
            }))
        }
    }
}

/// Emit one usage row for an upstream round on the chat/scheduler path.
/// Fire-and-forget; never affects the turn. Token counts come from the
/// trailing `usage` frame (we set `include_usage`), `None` if absent.
fn emit_usage(
    d: &OpenAiDriver,
    user_email: &str,
    model: &str,
    backend: &str,
    status: u16,
    started: std::time::Instant,
    tokens: (Option<i64>, Option<i64>, Option<i64>),
) {
    let (prompt_tokens, completion_tokens, total_tokens) = tokens;
    d.state.usage.emit(UsageRecord {
        created_at: jiff::Timestamp::now(),
        user_id: d.tool_ctx.user_id.clone(),
        user_email: (!user_email.is_empty()).then(|| user_email.to_string()),
        token_id: None,
        token_name: None,
        source: d.source,
        kind: UsageKind::Chat,
        backend: backend.to_string(),
        model: model.to_string(),
        status,
        duration_ms: started.elapsed().as_millis() as i64,
        prompt_tokens,
        completion_tokens,
        total_tokens,
        input_units: None,
        output_units: None,
        enforce_limits: d
            .state
            .upstreams
            .enforce_limits_for_model(model, crate::server::upstreams::PoolKind::Chat),
    });
}

fn upstream_err<E: std::fmt::Display>(e: E) -> TurnError {
    TurnError::Upstream {
        message: e.to_string(),
    }
}

fn transport_err<E: std::fmt::Display>(e: E) -> TurnError {
    TurnError::Transport {
        message: e.to_string(),
    }
}

/// Flatten an error's full `source()` chain into one line. The top-level
/// `Display` of our `DbError` is terse (`DbError::Query` renders the
/// wrapped `sqlx::Error` only via `#[source]`), so `to_string()` alone
/// drops the real cause. This keeps every link.
fn error_chain(err: &dyn std::error::Error) -> String {
    use std::fmt::Write as _;
    let mut chain = err.to_string();
    let mut src = err.source();
    while let Some(e) = src {
        let s = e.to_string();
        // thiserror's `#[error("… {0}")]` already embeds the source in the
        // parent's `Display`, so a naive walk prints the same sqlx text
        // several times. Skip any frame whose text we've already emitted.
        if !chain.contains(&s) {
            let _ = write!(chain, ": {s}");
        }
        src = e.source();
    }
    chain
}

/// `map_err` adaptor for the turn's local persistence steps. Logs the
/// failing operation + turn id + the full DB error chain at `error`
/// level (so it's always traceable server-side even if the UI truncates)
/// and returns a `Persistence` error whose message names the operation
/// and carries the real cause — so `upstream: query` becomes
/// `storage: append_content: error returned from database: database is locked`.
fn persist_err<'a, E>(op: &'static str, turn_id: &'a str) -> impl FnOnce(E) -> TurnError + 'a
where
    E: std::error::Error + 'a,
{
    move |e| {
        let chain = error_chain(&e);
        tracing::error!(
            operation = op,
            assistant_turn_id = %turn_id,
            error = %chain,
            "turn persistence step failed"
        );
        TurnError::Persistence {
            message: format!("{op}: {chain}"),
        }
    }
}

/// Project a stored `model_defaults` row onto the pure per-effort overrides
/// `apply_effort` consumes. Budgets are SQLite INTEGERs; a negative / oversized
/// value can't be a token count, so it degrades to `None` (built-in default)
/// rather than poisoning the request.
fn reasoning_overrides_from_row(
    row: &crate::server::db::model_defaults::ModelDefaults,
) -> crate::server::reasoning::ReasoningOverrides {
    let budget = |v: Option<i64>| v.and_then(|n| u32::try_from(n).ok());
    crate::server::reasoning::ReasoningOverrides {
        budget_standard: budget(row.thinking_budget_standard),
        budget_deep: budget(row.thinking_budget_deep),
        budget_max: budget(row.thinking_budget_max),
        effort_standard: row.reasoning_effort_standard.clone(),
        effort_deep: row.reasoning_effort_deep.clone(),
        effort_max: row.reasoning_effort_max.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        THINK_TAGS, ToolCallAcc, ensure_unique_tool_call_ids, render_active_skills,
        render_skill_listing, take_safe_content,
    };
    use crate::server::skills::{Skill, SkillRegistry};
    use std::path::PathBuf;

    /// Feed `deltas` through the streaming stripper and flush, returning the
    /// full emitted content (what the user would see).
    fn stream(deltas: &[&str]) -> String {
        let mut buf = String::new();
        let mut out = String::new();
        for d in deltas {
            buf.push_str(d);
            out.push_str(&take_safe_content(&mut buf));
        }
        for tag in THINK_TAGS {
            buf = buf.replace(tag, "");
        }
        out.push_str(&buf);
        out
    }

    #[test]
    fn plain_content_passes_through() {
        assert_eq!(stream(&["Hello, ", "world!"]), "Hello, world!");
    }

    #[test]
    fn strips_whole_think_tags() {
        assert_eq!(stream(&["</think>answer here"]), "answer here");
        assert_eq!(stream(&["<think>x</think>y"]), "xy");
    }

    #[test]
    fn strips_tag_split_across_deltas() {
        // The leaked `</think>` arriving in two chunks must still be removed.
        assert_eq!(stream(&["answer </th", "ink>more"]), "answer more");
    }

    #[test]
    fn preserves_lone_angle_bracket_that_is_not_a_tag() {
        // A `<` that never becomes a tag is delayed, never dropped.
        assert_eq!(stream(&["a < b"]), "a < b");
        assert_eq!(stream(&["value <", " 5 end"]), "value < 5 end");
    }

    fn acc(id: &str, name: &str) -> ToolCallAcc {
        ToolCallAcc {
            id: id.to_string(),
            name: name.to_string(),
            arguments: String::new(),
        }
    }

    #[test]
    fn tool_call_ids_deduped_within_a_round() {
        // Backend emitted two calls with the SAME id in one response.
        let mut seen = std::collections::HashSet::new();
        let mut calls = vec![acc("call_0", "a"), acc("call_0", "b")];
        ensure_unique_tool_call_ids(&mut calls, 0, &mut seen);
        assert_eq!(calls[0].id, "call_0"); // first keeps it
        assert_ne!(calls[1].id, "call_0"); // second rewritten
        assert_ne!(calls[0].id, calls[1].id);
    }

    #[test]
    fn empty_tool_call_ids_get_synthesised() {
        // Backend omitted the id entirely — must never persist "" (would
        // collide with the next empty-id call on the PK).
        let mut seen = std::collections::HashSet::new();
        let mut calls = vec![acc("", "a"), acc("", "b")];
        ensure_unique_tool_call_ids(&mut calls, 1, &mut seen);
        assert!(!calls[0].id.is_empty());
        assert!(!calls[1].id.is_empty());
        assert_ne!(calls[0].id, calls[1].id);
    }

    #[test]
    fn tool_call_ids_deduped_across_rounds() {
        // The real bug: a backend recycles `call_0` every round. Round 0's
        // id is fine; round 1's identical id must be rewritten so the second
        // insert doesn't hit `UNIQUE constraint failed: chat_tool_calls.id`.
        let mut seen = std::collections::HashSet::new();
        let mut round0 = vec![acc("call_0", "a")];
        ensure_unique_tool_call_ids(&mut round0, 0, &mut seen);
        let mut round1 = vec![acc("call_0", "b")];
        ensure_unique_tool_call_ids(&mut round1, 1, &mut seen);
        assert_eq!(round0[0].id, "call_0");
        assert_ne!(round1[0].id, "call_0");
    }

    #[test]
    fn distinct_tool_call_ids_are_left_untouched() {
        let mut seen = std::collections::HashSet::new();
        let mut calls = vec![acc("abc", "a"), acc("xyz", "b")];
        ensure_unique_tool_call_ids(&mut calls, 0, &mut seen);
        assert_eq!(calls[0].id, "abc");
        assert_eq!(calls[1].id, "xyz");
    }

    fn registry(entries: &[(&str, &str)]) -> SkillRegistry {
        SkillRegistry::new(entries.iter().map(|(n, d)| Skill {
            name: (*n).to_string(),
            title: (*n).to_string(),
            description: (*d).to_string(),
            root: PathBuf::from("/nonexistent"),
        }))
    }

    #[test]
    fn listing_includes_only_permitted_skills_with_descriptions() {
        // Two loaded, one permitted: the listing names the permitted one
        // (with its description) and the loader instruction, and never
        // mentions the skill the caller can't use.
        let reg = registry(&[
            ("brand", "Enforce the brand."),
            ("legal", "Apply the contract template."),
        ]);
        let out = render_skill_listing(&reg, &["brand".to_string()]).expect("a listing");
        assert!(out.contains("read_skill(name)"));
        assert!(out.contains("brand: Enforce the brand."));
        assert!(!out.contains("legal"));
    }

    #[test]
    fn no_permitted_skills_means_no_listing() {
        let reg = registry(&[("brand", "Enforce the brand.")]);
        assert!(render_skill_listing(&reg, &[]).is_none());
    }

    #[test]
    fn active_skills_reinject_the_full_body() {
        use crate::server::skills::discover;
        // A real on-disk bundle so `body()` reads actual content — this is
        // the sticky half: a loaded skill's instructions get spliced back in.
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("brand");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(
            bundle.join("SKILL.md"),
            "---\nname: brand\ndescription: d\n---\n\nAlways use purple #8E54E9.\n",
        )
        .unwrap();
        let reg = SkillRegistry::new(discover(dir.path()).unwrap());

        // Loaded → body present; not loaded → nothing.
        let out = render_active_skills(&reg, &["brand".to_string()]).expect("active section");
        assert!(out.contains("### Skill: brand"));
        assert!(out.contains("Always use purple #8E54E9."));
        assert!(render_active_skills(&reg, &[]).is_none());
    }

    mod history_fold {
        use crate::openai_driver::{build_history_messages, leading_system_message};
        use crate::server::db::chat_compactions::Compaction;
        use jiff::Timestamp;
        use session_core::db::{Turn, TurnRole, TurnStatus, TurnWithTools};

        fn turn(seq: i64, role: TurnRole, id: &str, text: &str) -> TurnWithTools {
            let now: Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
            let (user_content, content) = match role {
                TurnRole::User => (Some(text.to_string()), None),
                TurnRole::Assistant => (None, Some(text.to_string())),
            };
            TurnWithTools {
                turn: Turn {
                    id: id.to_string(),
                    session_id: "s1".into(),
                    seq,
                    role,
                    user_content,
                    model: None,
                    content,
                    reasoning: None,
                    reasoning_elapsed_ms: None,
                    status: TurnStatus::Completed,
                    error_message: None,
                    created_at: now,
                    completed_at: Some(now),
                },
                tool_calls: vec![],
            }
        }

        fn convo() -> Vec<TurnWithTools> {
            vec![
                turn(0, TurnRole::User, "t0", "q1"),
                turn(1, TurnRole::Assistant, "t1", "a1"),
                turn(2, TurnRole::User, "t2", "q2"),
                turn(3, TurnRole::Assistant, "t3", "a2"),
                turn(4, TurnRole::User, "t4", "q3"), // the in-progress turn's user prompt
                turn(5, TurnRole::Assistant, "t5", ""), // in-progress assistant (empty)
            ]
        }

        /// Without a compaction row every completed turn replays verbatim and
        /// the in-progress assistant turn is dropped.
        #[test]
        fn no_compaction_replays_all_prior() {
            let turns = convo();
            let msgs = build_history_messages(&turns, "t5", None, None);
            // q1,a1,q2,a2,q3 — the empty in-progress assistant (t5) is skipped.
            assert_eq!(msgs.len(), 5);
            assert_eq!(msgs[0]["content"], "q1");
            assert_eq!(msgs[4]["content"], "q3");
            assert!(msgs.iter().all(|m| m["role"] != "system"));
        }

        /// With a compaction cutoff, the folded prefix is dropped and only the
        /// verbatim tail is returned (the summary rides in the single leading
        /// system message — see the `system_message_*` tests).
        #[test]
        fn compaction_folds_out_prefix() {
            let turns = convo();
            let compaction = Compaction {
                up_to_seq: 3,
                summary: "the gist so far".into(),
                tokens_before: None,
                tokens_after: None,
            };
            let msgs = build_history_messages(&turns, "t5", Some(&compaction), None);
            // Only [q3] — seq 0..3 folded out, seq 4 verbatim, t5 dropped, and no
            // system message in the tail.
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0]["content"], "q3");
            assert!(msgs.iter().all(|m| m["role"] != "system"));
            // None of the folded turns leak through verbatim.
            assert!(
                msgs.iter()
                    .all(|m| m["content"] != "q1" && m["content"] != "a2")
            );
        }

        /// Voice directive + request context + summary collapse into exactly ONE
        /// system message (backends reject multiple leading system turns).
        #[test]
        fn system_message_merges_context_and_summary() {
            let m = leading_system_message(None, Some("CONTEXT".into()), Some("SUMMARY"))
                .expect("some");
            assert_eq!(m["role"], "system");
            let content = m["content"].as_str().unwrap();
            assert!(content.contains("CONTEXT"));
            assert!(content.contains("SUMMARY"));
        }

        /// Each part is optional; absent all three → no system message at all.
        #[test]
        fn system_message_optional_parts() {
            assert!(leading_system_message(None, None, None).is_none());
            let only_ctx = leading_system_message(None, Some("C".into()), None).unwrap();
            assert!(only_ctx["content"].as_str().unwrap().contains('C'));
            let only_sum = leading_system_message(None, None, Some("S")).unwrap();
            assert!(only_sum["content"].as_str().unwrap().contains('S'));
            // Voice directive alone still yields a system message.
            let only_voice = leading_system_message(Some("VOICE"), None, None).unwrap();
            assert!(only_voice["content"].as_str().unwrap().contains("VOICE"));
        }

        /// `history_limit` caps the verbatim tail *after* compaction folding.
        #[test]
        fn history_limit_caps_tail_after_fold() {
            let turns = convo();
            let msgs = build_history_messages(&turns, "t5", None, Some(2));
            // Last 2 prior turns before t5: a2 (seq3), q3 (seq4).
            assert_eq!(msgs.len(), 2);
            assert_eq!(msgs[0]["content"], "a2");
            assert_eq!(msgs[1]["content"], "q3");
        }
    }
}
