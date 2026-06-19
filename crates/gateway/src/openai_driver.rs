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
use crate::server::tools::{ToolContext, runner};

/// Reasoning tags some vLLM reasoning-parser configs leak into the *content*
/// channel even though reasoning is delivered separately via
/// `reasoning_content`. We strip them so a stray `</think>` never shows up in
/// the rendered answer.
const THINK_TAGS: [&str; 2] = ["<think>", "</think>"];

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
// forced to produce a final answer instead of ending the turn empty.
use crate::server::tools::runner::MAX_TOOL_ROUNDS as MAX_ROUNDS;

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
    let turns = chat::list_turns(&d.state.db, &ctx.session_id)
        .await
        .map_err(upstream_err)?;
    let mut messages: Vec<serde_json::Value> = turns
        .iter()
        .filter(|t| t.turn.id != ctx.assistant_turn_id)
        .filter_map(|t| message_for_history(&t.turn))
        .collect();

    // Prepend an auto-provided request-context system message — the
    // caller's real connection IP, a coarse IP-based location, and their
    // timezone. Lets the model answer "what's my IP / where am I / weather
    // here" directly instead of flailing through fetch_url/search_web/
    // get_user_location, and reflects the *true* source IP (correct in
    // production behind a load balancer, unlike an external IP-echo which
    // would report the gateway's own egress).
    if let Some(context) = build_request_context(d).await {
        messages.insert(
            0,
            serde_json::json!({ "role": "system", "content": context }),
        );
    }

    let mut started_reasoning: Option<std::time::Instant> = None;
    let mut frozen_reasoning_elapsed = false;

    for round in 0..MAX_ROUNDS {
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
        let final_round = round + 1 == MAX_ROUNDS;

        // Build the request. `stream: true` so we can forward
        // content deltas; tools injected if the user has any
        // granted.
        let mut request_body = serde_json::json!({
            "model": ctx.model,
            "messages": messages,
            "stream": true,
        });
        // Re-resolve the per-conversation tool overlay each round so a
        // mid-turn `enable_tools` call surfaces the newly-enabled schemas
        // on the next round. Cheap (sub-ms SQLite hit) and the only way
        // to make the model-driven enablement loop work.
        let allowed_tools = d
            .state
            .allowed_tools_for_session(&d.tool_ctx.roles, &d.tool_ctx.user_id, &ctx.session_id)
            .await;
        if final_round {
            tracing::info!(
                max_rounds = MAX_ROUNDS,
                "tool-round budget reached; requesting final answer with tools withheld"
            );
        } else {
            runner::inject_tools(&mut request_body, &d.state.tools, &allowed_tools)
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
        let serialized = serde_json::to_vec(&request_body).map_err(upstream_err)?;

        let acquired = d
            .state
            .upstreams
            .acquire_for(&ctx.model, crate::server::upstreams::PoolKind::Chat)
            .map_err(upstream_err)?;
        let backend = acquired.backend();
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
        let upstream = http_req.send().await.map_err(transport_err)?;
        if !upstream.status().is_success() {
            let status = upstream.status();
            let bytes = upstream.bytes().await.unwrap_or_default();
            drop(acquired);
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
        let mut upstream_stream = upstream.bytes_stream();

        'chunks: while let Some(chunk) = upstream_stream.next().await {
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
                        let _ = ctx.broadcast.send(TurnUpdate::Tick);
                        if reasoning_guard.push(reasoning) {
                            // Drop the upstream stream (closes the
                            // connection) and finalize the turn as errored
                            // with a clear message. The partial reasoning
                            // already streamed stays visible.
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
        let mut assistant_tool_calls: Vec<serde_json::Value> = Vec::new();
        let mut call_refs: Vec<runner::ToolCallRef> = Vec::new();
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
            if d.state.tools.contains(&acc.name) {
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
                    let key = crate::server::tools::catalog::entry_key_for(&acc.name);
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
                tracing::debug!(
                    wire_name = %acc.name,
                    "chat-stream got tool_call for a tool we don't own; ignoring"
                );
            }
        }
        if call_refs.is_empty() {
            return Ok(());
        }

        let results = runner::execute_tool_calls(&d.state.tools, &d.tool_ctx, &call_refs).await;
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": serde_json::Value::Null,
            "tool_calls": assistant_tool_calls,
        }));
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
async fn build_request_context(d: &OpenAiDriver) -> Option<String> {
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

    if ip.is_none() && geo.is_none() && timezone.is_none() && name.is_none() && email.is_none() {
        return None;
    }

    let mut out = String::from(
        "Automatically provided context about the signed-in user making this request. \
         When they ask you to act on their behalf — e.g. as the sender/signature of a \
         letter or document — use their name and email below; do not invent a name or \
         ask for it. Also use this to personalise replies and to answer questions about \
         their IP address, approximate location, or local time directly — do not fetch \
         external services or search the web for these.\n\
         \n\
         Your `tools` list is intentionally minimal: only `enable_tools` is on by \
         default; every other capability (memory, web fetch, document rendering, \
         network diagnostics, MCP integrations, attachments, …) starts off and must be \
         turned on. Call `enable_tools(keys)` FIRST whenever the user's request needs a \
         capability that isn't already in your tools list — its description lists every \
         available key. Enablement is sticky for this conversation, so you only pay the \
         turn-on cost once per capability.\n",
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
    Some(out)
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
    use super::{THINK_TAGS, take_safe_content};

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
}
