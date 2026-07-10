// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Automatic conversation compaction.
//!
//! A chat session replays its whole turn history to the model on every turn
//! (see [`crate::openai_driver::run_one_turn`]), so the upstream prompt grows
//! without bound and eventually crowds the model's context window. Compaction
//! folds the oldest prefix of a conversation into one LLM-generated summary and
//! replays that summary in place of the folded turns, keeping the most recent
//! turns verbatim (the "hybrid" scheme).
//!
//! The trigger is automatic and after-the-fact: once an assistant turn
//! finalises, the driver spawns [`maybe_autocompact`], which compares the
//! turn's measured context size ([`session_core::db::latest_context_tokens`])
//! against a fraction of the model's context window. If it's over, it
//! summarises in the background — off the turn's critical path, exactly like
//! the title-generation task — so the *next* turn replays a smaller prompt.
//!
//! Re-compaction folds the previous summary plus the newly-aged turns into a
//! fresh summary and bumps the cutoff, so a long-running conversation stays
//! bounded across many compactions.
//!
//! The summariser is a one-shot, non-streaming, capped, best-effort model call
//! modelled on [`crate::rama_server::pages::chat::title`]. The folded turns are
//! never deleted — they stay in `chat_turns` and remain visible in the
//! transcript; they are simply not sent upstream.

use session_core::db::{self as chat, TurnRole, TurnStatus, TurnWithTools};

use crate::rama_server::state::RamaState;
use crate::server::config::CompactionConfig;
use crate::server::db::{chat_compactions, model_defaults};
use crate::server::upstreams::PoolKind;

/// Hard timeout on the summariser call — a sticky upstream can't keep the
/// background task alive indefinitely.
const TIMEOUT_SECS: u64 = 60;

/// Per-tool-call truncation caps in the summariser input. Tool arguments and
/// outputs can be large (a fetched page, a document); we hand the summariser a
/// bounded slice — enough to know what happened, not the whole payload it's
/// meant to compress away.
const MAX_TOOL_ARGS_CHARS: usize = 200;
const MAX_TOOL_OUTPUT_CHARS: usize = 600;

const SUMMARY_SYSTEM_PROMPT: &str = "You compress the earlier part of a chat conversation into a \
dense summary that will REPLACE those messages so the assistant can keep going without them.\n\
Preserve: concrete facts, decisions made, the user's goals and constraints, file/function/entity \
names, numbers, code or commands that matter, tool results the conversation relied on, and any \
open questions or unfinished tasks.\n\
Drop: pleasantries, redundancy, and step-by-step narration that no longer matters.\n\
Write in compact prose or bullet points. No preamble, no meta-commentary, no \"here is the \
summary\" — output only the summary itself.\n\
/no_think";

/// Check the session's current context size against the compaction threshold
/// and, if over, summarise the oldest turns in the background. Best-effort:
/// every failure path logs and returns without touching the conversation.
///
/// Called (spawned) by the driver after an assistant turn finalises. `model`
/// is the **resolved real model** (the driver maps any alias first) — used both
/// to resolve the context window and to route the summariser call.
pub async fn maybe_autocompact(state: &RamaState, session_id: &str, model: &str) {
    let cfg = &state.config.chat.compaction;
    if !cfg.enabled {
        return;
    }
    if cfg.trigger_ratio <= 0.0 {
        return;
    }

    let window = model_context_window(state, model)
        .await
        .unwrap_or(cfg.default_context_window);
    if window <= 0 {
        return;
    }
    let threshold = (window as f64 * cfg.trigger_ratio) as i64;

    let current = match chat::latest_context_tokens(&state.db, session_id).await {
        Ok(Some(n)) => n,
        Ok(None) => return, // no measurement yet — nothing to decide on
        Err(err) => {
            tracing::warn!(error = %err, %session_id, "compaction: reading context size failed");
            return;
        }
    };
    if current < threshold {
        return;
    }

    tracing::info!(
        %session_id, %model, current, threshold, window,
        "compaction: context over threshold, summarising"
    );
    match run_compaction(state, session_id, model, Some(current)).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(%session_id, "compaction: nothing to fold (guarded)");
        }
        Err(err) => {
            tracing::warn!(error = %err, %session_id, "compaction: failed");
        }
    }
}

/// Resolve the model's context window from `model_defaults`. `None` when the
/// model has no row or no `context_window` set — the caller falls back to the
/// global default. Keyed on the resolved real model id (the caller maps any
/// alias first), matching how reasoning config and cost accounting key on it —
/// an alias carries no settings of its own.
async fn model_context_window(state: &RamaState, model: &str) -> Option<i64> {
    model_defaults::get(&state.db, model)
        .await
        .ok()
        .flatten()
        .and_then(|row| row.context_window)
        .filter(|w| *w > 0)
}

/// Load the session, plan the fold, call the summariser, and persist the
/// compaction row. Returns `Ok(true)` if a summary was written, `Ok(false)` if
/// the plan decided there was nothing (new) to fold.
async fn run_compaction(
    state: &RamaState,
    session_id: &str,
    model: &str,
    tokens_before: Option<i64>,
) -> Result<bool, String> {
    let turns = chat::list_turns(&state.db, session_id)
        .await
        .map_err(|e| e.to_string())?;
    let existing = chat_compactions::get(&state.db, session_id)
        .await
        .map_err(|e| e.to_string())?;
    let cfg = &state.config.chat.compaction;

    let Some(plan) = plan_compaction(&turns, existing.as_ref().map(|c| c.up_to_seq), cfg) else {
        return Ok(false);
    };

    let raw = tokio::time::timeout(
        std::time::Duration::from_secs(TIMEOUT_SECS),
        call_summarizer(state, model, &plan.input_text, cfg.summary_max_tokens),
    )
    .await
    .map_err(|_| "summariser timed out".to_string())??;

    let summary = clean_summary(&raw);
    if summary.is_empty() {
        return Err("summariser returned empty content".to_string());
    }
    // Rough token estimate for bookkeeping only (~4 chars/token); never
    // load-bearing.
    let tokens_after = Some((summary.chars().count() / 4) as i64);
    chat_compactions::upsert(
        &state.db,
        session_id,
        plan.new_up_to_seq,
        &summary,
        tokens_before,
        tokens_after,
    )
    .await
    .map_err(|e| e.to_string())?;
    tracing::info!(
        %session_id,
        up_to_seq = plan.new_up_to_seq,
        summary_len = summary.len(),
        "compaction: summary persisted"
    );
    Ok(true)
}

/// The decision + the text to summarise. Pure output of [`plan_compaction`].
#[derive(Debug, PartialEq, Eq)]
struct CompactionPlan {
    /// New cutoff: the highest turn `seq` the fresh summary will cover.
    new_up_to_seq: i64,
    /// The summariser input — the previous summary (if any) followed by the
    /// newly-aged turns rendered as plain text.
    input_text: String,
}

/// Decide whether (and what) to compact. Pure so it can be unit-tested without
/// a model call.
///
/// - Keeps the last `keep_recent_turns` eligible turns verbatim.
/// - Only folds turns that have aged past `old_up_to_seq` (the previous
///   summary already covers the rest).
/// - Returns `None` when there aren't enough newly-aged turns to be worth a
///   re-summarise (`min_turns_to_compact`), or when nothing new has aged.
fn plan_compaction(
    turns: &[TurnWithTools],
    old_up_to_seq: Option<i64>,
    cfg: &CompactionConfig,
) -> Option<CompactionPlan> {
    // Eligible = the turns that actually go upstream on replay: every user
    // turn, plus completed assistant turns with visible content. In-progress /
    // errored / empty assistant turns are skipped (they never replay), so the
    // cutoff lines up with what the summary is standing in for.
    let eligible: Vec<&TurnWithTools> = turns
        .iter()
        .filter(|t| match t.turn.role {
            TurnRole::User => true,
            TurnRole::Assistant => {
                t.turn.status == TurnStatus::Completed
                    && t.turn.content.as_deref().is_some_and(|c| !c.is_empty())
            }
        })
        .collect();

    if eligible.len() <= cfg.keep_recent_turns {
        return None;
    }
    let cutoff_index = eligible.len() - cfg.keep_recent_turns;
    let folded = &eligible[..cutoff_index];
    let new_up_to_seq = folded.last()?.turn.seq;

    let old_up_to = old_up_to_seq.unwrap_or(-1);
    if new_up_to_seq <= old_up_to {
        return None; // nothing new has aged past the previous cutoff
    }
    let newly_folded: Vec<&&TurnWithTools> =
        folded.iter().filter(|t| t.turn.seq > old_up_to).collect();
    if newly_folded.len() < cfg.min_turns_to_compact {
        return None; // anti-thrash: not enough new material to re-summarise
    }

    let mut input_text = String::new();
    if old_up_to_seq.is_some() {
        input_text.push_str(
            "Additional conversation messages to fold into the summary above follow. \
             Merge them into a single updated summary.\n\n",
        );
    }
    for t in newly_folded {
        append_turn(&mut input_text, t);
    }

    Some(CompactionPlan {
        new_up_to_seq,
        input_text,
    })
}

/// Render one turn into the summariser input: the user's prompt, or the
/// assistant's content plus a one-line trace of each tool call (name + bounded
/// args + bounded output). Tool traces matter because they're never replayed
/// as normal history, yet are often the load-bearing context.
fn append_turn(out: &mut String, t: &TurnWithTools) {
    match t.turn.role {
        TurnRole::User => {
            let raw = t.turn.user_content.clone().unwrap_or_default();
            let content =
                crate::server::chat_attachments::strip_markers_for_replay(&raw, &t.turn.id);
            if !content.trim().is_empty() {
                out.push_str("User: ");
                out.push_str(content.trim());
                out.push_str("\n\n");
            }
        }
        TurnRole::Assistant => {
            if let Some(content) = t.turn.content.as_deref().filter(|c| !c.is_empty()) {
                out.push_str("Assistant: ");
                out.push_str(content.trim());
                out.push('\n');
            }
            for tc in &t.tool_calls {
                out.push_str("  [tool ");
                out.push_str(&tc.name);
                out.push('(');
                out.push_str(&truncate_chars(
                    tc.arguments_json.trim(),
                    MAX_TOOL_ARGS_CHARS,
                ));
                out.push(')');
                if let Some(output) = tc.output_json.as_deref() {
                    out.push_str(" -> ");
                    out.push_str(&truncate_chars(output.trim(), MAX_TOOL_OUTPUT_CHARS));
                }
                out.push_str("]\n");
            }
            out.push('\n');
        }
    }
}

/// Char-bounded truncation with an ellipsis marker. Char-based (not byte-based)
/// so it never splits a UTF-8 sequence.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// One non-streaming chat completion that produces the summary. Modelled on the
/// title-generation call: temperature 0, capped output, reasoning defeated
/// three ways (vLLM `enable_thinking=false`, `/no_think`, and the prompt), no
/// tools.
async fn call_summarizer(
    state: &RamaState,
    model: &str,
    input: &str,
    max_tokens: i64,
) -> Result<String, String> {
    let acquired = state
        .upstreams
        .route(model, PoolKind::Chat)
        .map_err(|e| e.to_string())?;
    let real_model = acquired.resolved_model().to_string();
    let backend = acquired.backend();
    let url = format!("{}/chat/completions", backend.base_url);
    let user_with_directive = format!("{input}\n\n/no_think");
    let body = serde_json::json!({
        "model": real_model,
        "messages": [
            { "role": "system", "content": SUMMARY_SYSTEM_PROMPT },
            { "role": "user", "content": user_with_directive },
        ],
        "stream": false,
        "temperature": 0,
        "max_tokens": max_tokens,
        "chat_template_kwargs": { "enable_thinking": false },
    });
    let serialized = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let mut req = state
        .http
        .post(&url)
        .header("content-type", "application/json")
        .body(serialized);
    if let Some(key) = backend.api_key.as_deref() {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    drop(acquired);
    if !status.is_success() {
        return Err(format!(
            "upstream {status}: {}",
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(120)
                .collect::<String>()
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(v.pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string())
}

/// Trim the summariser output into a clean body: strip a leaked
/// `<think>…</think>` block (some reasoning-parser adapters leak it despite the
/// knobs) and surrounding whitespace.
fn clean_summary(raw: &str) -> String {
    strip_think_block(raw).trim().to_string()
}

/// Strip a single `<think>…</think>` block, case-insensitive. Conservative:
/// only acts on a balanced pair. (Mirrors the title-gen helper.)
fn strip_think_block(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let Some(start) = lower.find("<think>") else {
        return s.to_string();
    };
    let after_start = start + "<think>".len();
    let Some(rel_end) = lower[after_start..].find("</think>") else {
        return s.to_string();
    };
    let end = after_start + rel_end + "</think>".len();
    let mut out = String::with_capacity(s.len() - (end - start));
    out.push_str(&s[..start]);
    out.push_str(&s[end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;
    use session_core::db::{Turn, TurnRole, TurnStatus};

    fn cfg() -> CompactionConfig {
        CompactionConfig {
            enabled: true,
            default_context_window: 1000,
            trigger_ratio: 0.7,
            keep_recent_turns: 2,
            min_turns_to_compact: 2,
            summary_max_tokens: 512,
        }
    }

    fn turn(seq: i64, role: TurnRole, text: &str) -> TurnWithTools {
        let now: Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        let (user_content, content, status) = match role {
            TurnRole::User => (Some(text.to_string()), None, TurnStatus::Completed),
            TurnRole::Assistant => (None, Some(text.to_string()), TurnStatus::Completed),
        };
        TurnWithTools {
            turn: Turn {
                id: format!("t{seq}"),
                session_id: "s1".into(),
                seq,
                role,
                user_content,
                model: None,
                content,
                reasoning: None,
                reasoning_elapsed_ms: None,
                status,
                error_message: None,
                created_at: now,
                completed_at: Some(now),
            },
            tool_calls: vec![],
        }
    }

    /// A short conversation (<= keep_recent_turns worth) is never compacted.
    #[test]
    fn no_plan_when_short() {
        let turns = vec![
            turn(0, TurnRole::User, "hi"),
            turn(1, TurnRole::Assistant, "hello"),
        ];
        assert!(plan_compaction(&turns, None, &cfg()).is_none());
    }

    /// First compaction folds everything but the last `keep_recent_turns`.
    #[test]
    fn first_compaction_folds_prefix_keeps_tail() {
        let turns = vec![
            turn(0, TurnRole::User, "q1"),
            turn(1, TurnRole::Assistant, "a1"),
            turn(2, TurnRole::User, "q2"),
            turn(3, TurnRole::Assistant, "a2"),
            turn(4, TurnRole::User, "q3"),
            turn(5, TurnRole::Assistant, "a3"),
        ];
        // keep_recent_turns = 2 → fold seq 0..3, keep seq 4,5.
        let plan = plan_compaction(&turns, None, &cfg()).expect("should plan");
        assert_eq!(plan.new_up_to_seq, 3);
        assert!(plan.input_text.contains("q1"));
        assert!(plan.input_text.contains("a2"));
        assert!(!plan.input_text.contains("q3"), "tail must stay verbatim");
    }

    /// Anti-thrash: nothing new has aged past the previous cutoff → no plan.
    #[test]
    fn no_replan_when_nothing_new_aged() {
        let turns = vec![
            turn(0, TurnRole::User, "q1"),
            turn(1, TurnRole::Assistant, "a1"),
            turn(2, TurnRole::User, "q2"),
            turn(3, TurnRole::Assistant, "a2"),
            turn(4, TurnRole::User, "q3"),
            turn(5, TurnRole::Assistant, "a3"),
        ];
        // Already compacted up to seq 3; cutoff would still be 3 → None.
        assert!(plan_compaction(&turns, Some(3), &cfg()).is_none());
    }

    /// Re-compaction only folds the turns beyond the previous cutoff.
    #[test]
    fn recompaction_folds_only_new_turns() {
        let turns = vec![
            turn(0, TurnRole::User, "q1"),
            turn(1, TurnRole::Assistant, "a1"),
            turn(2, TurnRole::User, "q2"),
            turn(3, TurnRole::Assistant, "a2"),
            turn(4, TurnRole::User, "q3"),
            turn(5, TurnRole::Assistant, "a3"),
            turn(6, TurnRole::User, "q4"),
            turn(7, TurnRole::Assistant, "a4"),
        ];
        // Previously compacted to seq 1; keep_recent 2 → new cutoff seq 5.
        let plan = plan_compaction(&turns, Some(1), &cfg()).expect("should replan");
        assert_eq!(plan.new_up_to_seq, 5);
        assert!(
            !plan.input_text.contains("q1"),
            "already-summarised turn excluded"
        );
        assert!(plan.input_text.contains("q2"));
        assert!(plan.input_text.contains("a3"));
        assert!(!plan.input_text.contains("q4"), "tail stays verbatim");
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 3), "hel…");
        // Multi-byte chars aren't split.
        assert_eq!(truncate_chars("héllo wörld", 4), "héll…");
    }

    #[test]
    fn clean_summary_strips_think() {
        assert_eq!(
            clean_summary("<think>pondering</think>\n\nThe summary."),
            "The summary."
        );
    }
}
