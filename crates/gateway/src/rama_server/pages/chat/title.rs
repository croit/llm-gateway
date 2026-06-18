// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! LLM-driven session title generation.
//!
//! Triggered by `chat_message_send` the moment the first user message
//! lands on an untitled session. Runs as a separate `tokio::spawn`
//! task so it doesn't block the main chat response. The prompt asks
//! the model for a 3-6 word title with no preamble or reasoning;
//! we strip `<think>…</think>` blocks defensively in case the
//! upstream's reasoning parser leaks them through.
//!
//! Once the title lands in `chat_sessions.title`, we push a
//! `TurnUpdate::SidebarChanged` through the live worker's broadcast
//! (if any) so the sidebar row updates in place without waiting for
//! the user's next navigation.

use std::sync::Arc;

use session_core::TurnUpdate;

use crate::rama_server::state::RamaState;
use crate::server::upstreams::PoolKind;
use session_core::db as chat;

/// Hard char cap on the generated title. The sidebar's 18rem column
/// fits roughly 32-36 characters at the 14px session-row font size
/// before ellipsizing, so we land just past that — the CSS still
/// truncates, but the DB row stays close to what's visible.
const MAX_TITLE_LEN: usize = 40;

/// Independent word cap. A 5-word title made of single-character
/// words is fine; a 5-word title where every word is `Functionality`
/// hits the char cap first. Whichever cap fires first wins.
const MAX_TITLE_WORDS: usize = 5;

/// Hard timeout so a sticky upstream can't keep this background task
/// around indefinitely. The chat worker itself doesn't have a
/// timeout (the user can stop it), but title-gen is fire-and-forget
/// with no stop button.
const TIMEOUT_SECS: u64 = 15;

/// System prompt: terse + concrete length bound + examples so
/// reasoning models that follow instructions land in a sensible
/// place. We *also* clamp server-side to be sure — chat models
/// regularly produce 8-word "perfectly accurate" titles.
/// The trailing `/no_think` is Qwen3's reasoning-off marker —
/// honored by Qwen3-family vLLM deployments, treated as harmless
/// junk by everything else.
const SYSTEM_PROMPT: &str = "You name chat conversations.\n\
Output ONLY a short headline (2-4 words, max 30 characters) for the user's message. \
No quotes, no punctuation, no preamble, no reasoning. \
Capitalize like a headline.\n\
Examples:\n\
Ceph vs SeaweedFS\n\
Postgres tuning help\n\
Rust async question\n\
/no_think";

/// Run a title-generation pass against the upstream and persist the
/// result. Best-effort — every failure path logs and returns without
/// touching the DB (so the session stays "Untitled chat" until the
/// next message lands or the user renames it). `model` defaults to
/// the same one the user picked for the conversation.
pub(super) async fn generate_session_title(
    state: Arc<RamaState>,
    user_id: String,
    session_id: String,
    user_msg: String,
    model: String,
) {
    tracing::info!(
        %session_id,
        %model,
        msg_len = user_msg.len(),
        "title generation: starting"
    );
    let fut = call_upstream(&state, &model, &user_msg);
    let raw = match tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_SECS), fut).await {
        Ok(Ok(r)) => r,
        Ok(Err(err)) => {
            tracing::warn!(error = %err, %session_id, %model, "title generation: upstream failed");
            return;
        }
        Err(_) => {
            tracing::warn!(%session_id, %model, "title generation: timed out");
            return;
        }
    };
    tracing::info!(
        %session_id,
        content_len = raw.content.len(),
        reasoning_len = raw.reasoning_content.len(),
        finish_reason = %raw.finish_reason,
        "title generation: response received"
    );
    let title = clean_title(&raw.content);
    if title.is_empty() {
        // Two common upstream shapes land here:
        //   * reasoning-parser pulled the model's entire output into
        //     `reasoning_content` and left `content` empty. The
        //     `chat_template_kwargs.enable_thinking=false` knob in
        //     the request body is meant to prevent that on vLLM
        //     Qwen3 — if you still see this branch, the upstream
        //     either ignored the knob or the chat template doesn't
        //     wire it up.
        //   * `finish_reason="length"` — the model ran out of token
        //     budget mid-reasoning. Bumping `max_tokens` in
        //     `call_upstream` is the answer.
        tracing::warn!(
            %session_id,
            finish_reason = %raw.finish_reason,
            content_len = raw.content.len(),
            reasoning_len = raw.reasoning_content.len(),
            content_prefix = %raw.content.chars().take(200).collect::<String>(),
            reasoning_prefix = %raw.reasoning_content.chars().take(200).collect::<String>(),
            "title generation: empty content after cleanup"
        );
        return;
    }
    tracing::info!(%session_id, title = %title, "title generation: persisting");
    if let Err(err) = chat::set_session_title(&state.db, &session_id, &title).await {
        tracing::warn!(error = %err, %session_id, "title generation: DB write failed");
        return;
    }

    // Push a live update so the sidebar reflects the rename without
    // waiting for the user's next nav. Only meaningful when this
    // session's worker is still being tailed; if it's already
    // finalised (slow title gen) the sidebar will pick up the new
    // title on the user's next page interaction.
    if let Some(worker) = state.chats.get(&user_id)
        && worker.session_id == session_id
    {
        let _ = worker.broadcast.send(TurnUpdate::SidebarChanged);
    }
}

/// Just the fields the caller wants to inspect on the upstream's
/// `choices[0].message` — visible content, reasoning-parser-split
/// content (vLLM extension; empty everywhere else), and the
/// `finish_reason` so an operator can tell a `length` truncation
/// apart from a `stop` with empty output.
struct UpstreamReply {
    content: String,
    reasoning_content: String,
    finish_reason: String,
}

/// Single non-streaming chat completion. Hard-capped output tokens so
/// even an unruly model can't burn time generating an essay. Skips
/// tool injection entirely — title generation should never call any
/// tool.
async fn call_upstream(
    state: &RamaState,
    model: &str,
    user_msg: &str,
) -> Result<UpstreamReply, String> {
    let acquired = state
        .upstreams
        .acquire_for(model, PoolKind::Chat)
        .map_err(|e| e.to_string())?;
    let backend = acquired.backend();
    let url = format!("{}/chat/completions", backend.base_url);
    // Three reasoning-defeating knobs, in order of how reliably they
    // work across upstreams:
    //
    //   1. `chat_template_kwargs: {enable_thinking: false}` is a
    //      vLLM extension. For Qwen3-family models served via vLLM
    //      with the Qwen chat template, this is the *only* way to
    //      make the model skip its `<think>…</think>` block — the
    //      template renders entirely without the reasoning prelude.
    //      Other upstreams (OpenAI, Anthropic-compat, llama.cpp's
    //      openai server) ignore unknown JSON fields per the OpenAI
    //      spec, so passing it is harmless elsewhere.
    //
    //   2. `/no_think` appended to the user message. Qwen3's tokenizer
    //      treats this as a per-turn directive in addition to the
    //      template knob above; other model families read it as
    //      literal text and ignore.
    //
    //   3. The system prompt itself says "no reasoning" — for any
    //      model that follows instructions but doesn't recognise
    //      either of the above mechanisms.
    //
    // `max_tokens: 256` is enough for both a small reasoning preamble
    // (when reasoning slips through anyway) and a 3-6 word title.
    let user_with_directive = format!("{user_msg}\n\n/no_think");
    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": user_with_directive },
        ],
        "stream": false,
        "temperature": 0,
        "max_tokens": 256,
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
    let pluck_str = |ptr: &str| -> String {
        v.pointer(ptr)
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string()
    };
    Ok(UpstreamReply {
        content: pluck_str("/choices/0/message/content"),
        reasoning_content: pluck_str("/choices/0/message/reasoning_content"),
        finish_reason: pluck_str("/choices/0/finish_reason"),
    })
}

/// Squeeze the model's response into a title-shaped string:
/// - strip a leading `<think>…</think>` block if present (some
///   reasoning-parser adapters leak it through despite the prompt)
/// - take the first non-empty line
/// - drop surrounding quotes / asterisks / colons / trailing dots
/// - clamp to `MAX_TITLE_WORDS` words, *then* `MAX_TITLE_LEN` chars,
///   so a chatty model still produces a sidebar-friendly result
/// - if the char clamp lands mid-word, drop the half-word so we
///   don't get `Generation Functionali…`
fn clean_title(raw: &str) -> String {
    let after_think = strip_think_block(raw);
    let first_line = after_think
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let trimmed = first_line.trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '*' | '#' | '-' | '`')
    });
    let trimmed = trimmed.trim_end_matches(['.', ':', '!', '?']).trim();

    // Word cap first.
    let by_words: String = trimmed
        .split_whitespace()
        .take(MAX_TITLE_WORDS)
        .collect::<Vec<_>>()
        .join(" ");

    // Then char cap. If we land mid-word, drop back to the last word
    // boundary so we don't get an ugly half-word + ellipsis later.
    if by_words.chars().count() <= MAX_TITLE_LEN {
        return by_words;
    }
    let clipped: String = by_words.chars().take(MAX_TITLE_LEN).collect();
    match clipped.rfind(' ') {
        Some(idx) if idx > 0 => clipped[..idx].to_string(),
        _ => clipped,
    }
}

/// Strip a single `<think>…</think>` block, case-insensitive. Returns
/// the rest of the string concatenated. Conservative: only acts on
/// a balanced pair; if either tag is missing the input passes through
/// unchanged.
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

    #[test]
    fn clean_title_strips_quotes_and_caps_length() {
        assert_eq!(clean_title(r#""Hello World""#), "Hello World");
        assert_eq!(clean_title("  *Foo Bar*  "), "Foo Bar");
        assert_eq!(clean_title("title: Some Name."), "title: Some Name"); // colon mid-string stays
        let long: String = "x".repeat(200);
        assert_eq!(clean_title(&long).len(), MAX_TITLE_LEN);
    }

    #[test]
    fn clean_title_clips_to_word_cap() {
        // 7 words → first MAX_TITLE_WORDS (5) survive.
        assert_eq!(
            clean_title("Alpha Beta Gamma Delta Epsilon Zeta Eta"),
            "Alpha Beta Gamma Delta Epsilon"
        );
    }

    #[test]
    fn clean_title_char_cap_falls_back_to_word_boundary() {
        // Real-world case from the user: 5 words but 46 chars. The
        // char cap fires mid-word ("Functionality") and we drop back
        // to the previous space rather than emitting "Functio".
        let raw = "Testing Session Title Generation Functionality";
        let out = clean_title(raw);
        assert!(
            out.len() <= MAX_TITLE_LEN,
            "len {} should be <= {}",
            out.len(),
            MAX_TITLE_LEN
        );
        assert!(
            !out.ends_with("Functio"),
            "should not slice mid-word: {out:?}"
        );
        assert_eq!(out, "Testing Session Title Generation");
    }

    #[test]
    fn clean_title_takes_first_non_empty_line() {
        assert_eq!(
            clean_title("\n\nFirst Real Line\n\nrest"),
            "First Real Line"
        );
    }

    #[test]
    fn clean_title_strips_think_block() {
        let raw = "<think>let me think about this</think>\n\nActual Title";
        assert_eq!(clean_title(raw), "Actual Title");
    }

    #[test]
    fn clean_title_case_insensitive_think_tags() {
        let raw = "<THINK>internal</THINK>\nThe Title";
        assert_eq!(clean_title(raw), "The Title");
    }

    #[test]
    fn clean_title_passes_through_when_only_open_tag() {
        // Conservative: we don't strip half-baked thinking blocks
        // because the rest of the response might be misleading.
        let raw = "<think>oops no close\nbut this is the title";
        assert!(clean_title(raw).contains("<think>"));
    }

    #[test]
    fn strip_think_block_no_tags() {
        assert_eq!(strip_think_block("plain text"), "plain text");
    }

    #[test]
    fn strip_think_block_handles_attributes() {
        // We don't handle attributes — `<think foo="bar">` doesn't
        // match. Document the limitation; current upstreams emit
        // bare `<think>` so this is fine.
        let s = r#"<think foo="bar">x</think>rest"#;
        assert!(strip_think_block(s).contains("<think"));
    }
}
