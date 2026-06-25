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
    }
}

#[async_trait]
impl SessionDriver for OpenAiDriver {
    async fn run_turn(&self, ctx: SessionContext) -> Result<(), TurnError> {
        run_one_turn(self, ctx).await
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
    let user_mcp = d
        .state
        .mcp
        .layer_for_user(
            &d.tool_ctx.user_id,
            &mcp_role_ids,
            crate::server::tools::mcp::manager::AskContext::Chat,
        )
        .await;
    let tool_source = crate::server::tools::mcp::manager::CompositeToolSource::new(
        d.state.tools.as_ref(),
        &user_mcp,
    );

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
    let reasoning_style = {
        let explicit = crate::server::db::model_defaults::get(&d.state.db, &ctx.model)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.reasoning_style);
        crate::server::reasoning::ReasoningStyle::resolve(explicit.as_deref(), &ctx.model)
    };
    let max_rounds = effort.max_rounds();

    let turns = chat::list_turns(&d.state.db, &ctx.session_id)
        .await
        .map_err(upstream_err)?;
    // Prior turns, oldest-first, minus the in-progress assistant turn.
    let prior: Vec<_> = turns
        .iter()
        .filter(|t| t.turn.id != ctx.assistant_turn_id)
        .collect();
    // `history_limit` keeps only the most recent N turns (reuse-mode
    // scheduled runs); `None` replays them all.
    let kept = match d.history_limit {
        Some(n) => &prior[prior.len().saturating_sub(n)..],
        None => &prior[..],
    };
    let mut messages: Vec<serde_json::Value> = kept
        .iter()
        .filter_map(|t| message_for_history(&t.turn))
        .collect();

    // Prepend an auto-provided request-context system message — the
    // caller's real connection IP, a coarse IP-based location, and their
    // timezone. Lets the model answer "what's my IP / where am I / weather
    // here" directly instead of flailing through fetch_url/search_web/
    // get_user_location, and reflects the *true* source IP (correct in
    // production behind a load balancer, unlike an external IP-echo which
    // would report the gateway's own egress).
    if let Some(context) = build_request_context(d, &user_mcp).await {
        messages.insert(
            0,
            serde_json::json!({ "role": "system", "content": context }),
        );
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
        // parsed for metrics below and otherwise ignored (its `choices` is
        // empty, so the delta loop skips it). Omitted when metrics are off,
        // so a disabled gateway doesn't even alter the upstream request.
        let mut request_body = serde_json::json!({
            "model": ctx.model,
            "messages": messages,
            "stream": true,
        });
        if metrics_on && let Some(obj) = request_body.as_object_mut() {
            obj.insert(
                "stream_options".into(),
                serde_json::json!({"include_usage": true}),
            );
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
        crate::server::reasoning::apply_effort(reasoning_style, effort, &mut request_body);
        let serialized = serde_json::to_vec(&request_body).map_err(upstream_err)?;

        let acquired = d
            .state
            .upstreams
            .acquire_for(&ctx.model, crate::server::upstreams::PoolKind::Chat)
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
                            .map_err(upstream_err)?;
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
                                .map_err(upstream_err)?;
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
                            .map_err(upstream_err)?;
                            frozen_reasoning_elapsed = true;
                        }
                        // Strip stray reasoning tags leaked into content.
                        content_tag_buf.push_str(content);
                        let emit = take_safe_content(&mut content_tag_buf);
                        if !emit.is_empty() {
                            round_content.push_str(&emit);
                            chat::append_content(&d.state.db, &ctx.assistant_turn_id, &emit)
                                .await
                                .map_err(upstream_err)?;
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
                    .map_err(upstream_err)?;
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
        let collected: Vec<ToolCallAcc> = tool_acc.into_values().collect();
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
            .map_err(upstream_err)?;
            let _ = ctx.broadcast.send(TurnUpdate::Tick);

            assistant_tool_calls.push(serde_json::json!({
                "id": acc.id.clone(),
                "type": "function",
                "function": {
                    "name": acc.name.clone(),
                    "arguments": acc.arguments.clone(),
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
            .map_err(upstream_err)?;
            let _ = ctx.broadcast.send(TurnUpdate::Tick);
            // If the tool returned a `tool_content_parts(...)` envelope
            // we splice it into the message as array content (so a
            // vision-capable upstream actually gets `image_url` bytes
            // back). Otherwise fall back to stringified JSON — the
            // pre-existing contract.
            let content = match crate::server::tools::extract_content_parts(&result.body) {
                Some(parts) => serde_json::Value::Array(parts.clone()),
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
    let registry = d.state.skills.as_ref()?.current();
    let allowed = d.state.allowed_skills_for(&d.tool_ctx.roles);
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

#[cfg(test)]
mod tests {
    use super::{THINK_TAGS, render_active_skills, render_skill_listing, take_safe_content};
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
}
