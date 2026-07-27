// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

/// One slice of an assistant bubble — either pre-rendered markdown
/// prose or an attachment the model produced via `upload_attachment`.
/// We pre-render the markdown inside `assistant_segments` (rather
/// than passing the raw text through the bubble loop) so each
/// segment carries its own escaped HTML, the same way the upstream
/// renderer worked before split-marker support.
pub(crate) enum AssistantSegment {
    Prose(String),
    Attachment(crate::attachments::ParsedAttachment),
}

pub(crate) fn assistant_segments(
    content: &str,
    turn_id: &str,
    lang: Lang,
) -> Vec<AssistantSegment> {
    // Copy-button labels for the fenced code blocks in this reply's prose.
    let copy = CopyLabels::for_lang(lang);
    let raw_segs = crate::attachments::split_markers_for_turn(content, turn_id);
    // Fast path: no attachment markers in the assistant content —
    // render the whole thing as one markdown block so the existing
    // streaming/morph behavior is byte-identical to pre-marker code.
    if !raw_segs
        .iter()
        .any(|s| matches!(s, crate::attachments::Segment::Attachment(_)))
    {
        return vec![AssistantSegment::Prose(render_markdown_with_copy(
            content, &copy,
        ))];
    }
    raw_segs
        .into_iter()
        .filter_map(|s| match s {
            crate::attachments::Segment::Text(t) if !t.is_empty() => {
                Some(AssistantSegment::Prose(render_markdown_with_copy(t, &copy)))
            }
            crate::attachments::Segment::Text(_) => None,
            crate::attachments::Segment::Attachment(a) => Some(AssistantSegment::Attachment(a)),
        })
        .collect()
}

pub fn render_assistant_turn(tw: &TurnWithTools, actions: Option<&str>, lang: Lang) -> Html {
    let turn = tw.turn.clone();
    let tools = tw.tool_calls.clone();
    let dom_id = format!("turn-{}", turn.id);
    let reasoning = turn.reasoning.clone().unwrap_or_default();
    let content = turn.content.clone().unwrap_or_default();
    let elapsed_ms = turn.reasoning_elapsed_ms;
    // The reasoning phase is over — and its timer must stop — the moment the
    // driver freezes `reasoning_elapsed_ms` (on the first content delta, or at
    // finalize for a reasoning-only turn), NOT when the whole turn ends. Tying
    // this to turn status would let the live timer keep climbing through the
    // entire answer/tool-call stream and only snap back at completion.
    let reasoning_done = elapsed_ms.is_some();
    let in_progress = turn.status == TurnStatus::InProgress;
    let errored = turn.status == TurnStatus::Errored;
    let error_msg = turn.error_message.clone().unwrap_or_default();
    let show_spinner = in_progress && content.is_empty();
    let thinking_id = format!("turn-{}-thinking", turn.id);
    let tools_id = format!("turn-{}-tools", turn.id);
    let text_id = format!("turn-{}-text", turn.id);
    // Walk the same marker regex over the assistant's content as we
    // do over user content: any `[gw-attachment …]` line the model
    // produced via the `upload_attachment` tool becomes an inline
    // image/chip exactly where it sits chronologically in the prose.
    // No markers? `assistant_segments` returns a single rendered-
    // markdown segment and the body stays one block, preserving the
    // pre-existing fast path.
    let segments = assistant_segments(&content, &turn.id, lang);
    // Normalise into body pieces so a run of 2+ generated media collapses
    // into a numbered side-by-side gallery. Prose stays raw-injected HTML.
    let body_pieces: Vec<BodyPiece> = segments
        .into_iter()
        .map(|seg| match seg {
            AssistantSegment::Prose(html) => BodyPiece::Block(html! { #(html) }.to_html()),
            AssistantSegment::Attachment(att) => match media_kind(&att) {
                Some(_) => BodyPiece::Media(att),
                None => BodyPiece::File(att),
            },
        })
        .collect();
    // Attachments the model produced (generated images, uploaded files)
    // are removable on the same owner gate as the retry affordance.
    let remove_prefix =
        actions.map(|base| format!("{base}/{}/turns/{}/attachment", turn.session_id, turn.id));
    let body_items = render_body(&body_pieces, remove_prefix.as_deref(), lang);
    let has_reasoning = !reasoning.is_empty();

    html! {
        div(id: (dom_id), class: "chat-msg--assistant") {
            // Reasoning block. Conditionally rendered: when reasoning
            // exists we emit the `<details>` shell; otherwise a tiny
            // empty placeholder (so subsequent inner-patches of the
            // bubble can morph it in without touching siblings).
            if has_reasoning {
                (render_thinking_block(&turn.id, &reasoning, elapsed_ms, turn.reasoning_started_at, reasoning_done, lang))
            } else {
                div(id: (thinking_id.clone()), class: "thinking-block-slot") {}
            }
            // Tool calls. Each row has its own stable id (`tc-<id>`)
            // so datastar's morph preserves user open/close state
            // across re-renders.
            div(id: (tools_id), class: "tool-calls flex flex-col") {
                (render_tool_call_list(&tools, &turn.id, lang))
            }
            // Main response text. Each prose segment is its own
            // markdown-rendered block, with attachment chips/images
            // spliced inline at the model's write-position.
            div(id: (text_id), class: "chat-prose") {
                for item in body_items.iter() {
                    (item.clone())
                }
            }
            // "Thinking…" spinner. Visible only when the turn is
            // in-progress AND no content has landed yet — CSS
            // handles the toggle so we don't need to render
            // conditionally on each tick.
            if show_spinner {
                div(class: "thinking flex items-center gap-2 text-base-content/60 text-sm") {
                    (icons::spinner(16))
                    span { (t(lang, "render-thinking-spinner")) }
                }
            }
            if errored {
                div(class: "alert alert-error mt-2") {
                    (icons::alert(16))
                    span { (error_msg) }
                }
            }
            (render_msg_time(turn.created_at))
            // Retry — only on a settled turn (never mid-stream). Drops
            // this reply + everything below and regenerates from the
            // preceding user message with the currently-selected model.
            if actions.is_some() && !in_progress {
                (render_retry_action(&turn, actions.unwrap_or(""), lang))
            }
        }
    }
    .to_html()
}

/// Hover "Retry" affordance under a settled assistant bubble.
pub(crate) fn render_retry_action(turn: &Turn, base: &str, lang: Lang) -> Html {
    let retry_url = action_url(base, turn, "retry");
    let confirm_text = t(lang, "render-retry-confirm");
    let submit = action_submit(&retry_url, &confirm_text);
    let retry_label = t(lang, "render-retry-button");
    html! {
        div(class: "chat-msg__actions") {
            form(
                action: (retry_url),
                method: "post",
                class: "m-0",
                "data-on:submit__prevent": (submit)
            ) {
                input(type: "hidden", name: "model");
                button(type: "submit", class: "chat-msg__action") { (retry_label) }
            }
        }
    }
    .to_html()
}

/// `<details>` shell for a reasoning block. `finalized=true` switches
/// the summary from "Thinking… (Xs)" to "Thought for Xs". Carries
/// `data-preserve-attr="open"` so datastar's morph leaves the user's
/// collapse state alone on each re-render.
pub fn render_thinking_block(
    turn_id: &str,
    reasoning: &str,
    elapsed_ms: Option<i64>,
    reasoning_started_at: Option<jiff::Timestamp>,
    finalized: bool,
    lang: Lang,
) -> Html {
    let body_id = format!("turn-{turn_id}-thinking-body");
    let shell_id = format!("turn-{turn_id}-thinking");
    let summary_id = format!("turn-{turn_id}-thinking-summary");
    let timer_id = format!("turn-{turn_id}-thinking-timer");
    let rendered = render_markdown_with_copy(reasoning, &CopyLabels::for_lang(lang));
    html! {
        // Collapsed by default — reasoning is mostly debugging
        // material, not something the reader needs in the flow.
        // `data-preserve-attr="open"` keeps the user's expand /
        // collapse state across morph re-renders, so if they pop
        // it open mid-stream subsequent ticks don't snap it shut
        // again.
        details(
            id: (shell_id),
            class: "thinking-block",
            "data-preserve-attr": "open"
        ) {
            summary(id: (summary_id), class: "thinking-block__summary") {
                if finalized {
                    // Settled: a static, server-authoritative label.
                    span(class: "thinking-block__label") {
                        (finalized_thinking_label(elapsed_ms, lang))
                    }
                } else {
                    // In progress: the live timer ticks client-side. The
                    // server hands it the elapsed-so-far anchor
                    // (`data-elapsed-ms`, computed from the wall-clock
                    // reasoning start so a reload / late subscriber resumes
                    // correctly) and a localized `data-label-template` with a
                    // `{secs}` placeholder it fills each frame. The element's
                    // light-DOM text is a static fallback for the pre-upgrade
                    // / no-JS case; once upgraded it renders into a shadow
                    // root that datastar's morph never touches, so the count
                    // survives every per-tick bubble re-render.
                    span(class: "thinking-block__indicator") { (icons::spinner(12)) }
                    thinking_timer(
                        id: (timer_id),
                        class: "thinking-block__label",
                        data_elapsed_ms: (live_elapsed_ms(reasoning_started_at, elapsed_ms).to_string()),
                        data_label_template: (in_progress_label_template(lang))
                    ) {
                        (in_progress_thinking_label(reasoning_started_at, elapsed_ms, lang))
                    }
                }
            }
            div(id: (body_id), class: "thinking-prose") {
                #(rendered)
            }
        }
    }
    .to_html()
}

/// One-decimal, locale-stable seconds string (e.g. "12.3"). We format the
/// number ourselves rather than handing Fluent a raw f64 so every locale
/// gets the same fixed shape — Fluent's `NUMBER()` would otherwise apply
/// locale-specific grouping / decimal-separator rules we don't want for a
/// short elapsed-time label.
pub(crate) fn fmt_secs(elapsed_ms: i64) -> String {
    format!("{:.1}", elapsed_ms as f64 / 1000.0)
}

/// Elapsed reasoning time at render instant, in ms. Prefers the wall-clock
/// `reasoning_started_at` anchor (so a mid-stream reload resumes at the
/// right offset); falls back to a frozen `reasoning_elapsed_ms`, then 0.
pub(crate) fn live_elapsed_ms(
    reasoning_started_at: Option<jiff::Timestamp>,
    elapsed_ms: Option<i64>,
) -> i64 {
    reasoning_started_at
        .and_then(|s| {
            (jiff::Timestamp::now() - s)
                .total(jiff::Unit::Millisecond)
                .ok()
        })
        .map(|ms| ms.max(0.0) as i64)
        .or(elapsed_ms)
        .unwrap_or(0)
}

/// Settled "Thought for X.Ys" label.
pub(crate) fn finalized_thinking_label(elapsed_ms: Option<i64>, lang: Lang) -> String {
    t_args(
        lang,
        "render-thinking-finalized",
        &i18n::args([("secs", fmt_secs(elapsed_ms.unwrap_or(0)).into())]),
    )
}

/// Static "Thinking… (X.Ys)" fallback for the in-progress element's light
/// DOM (pre-upgrade / no-JS). The live value is driven by the client.
pub(crate) fn in_progress_thinking_label(
    reasoning_started_at: Option<jiff::Timestamp>,
    elapsed_ms: Option<i64>,
    lang: Lang,
) -> String {
    let secs = fmt_secs(live_elapsed_ms(reasoning_started_at, elapsed_ms));
    t_args(
        lang,
        "render-thinking-in-progress",
        &i18n::args([("secs", secs.into())]),
    )
}

/// The in-progress label with a literal `{secs}` placeholder instead of a
/// number — the client-side `<thinking-timer>` substitutes the live value
/// each frame. Keeps all translations server-owned (Fluent runs with
/// `use_isolating(false)`, so no bidi marks split the placeholder).
pub(crate) fn in_progress_label_template(lang: Lang) -> String {
    t_args(
        lang,
        "render-thinking-in-progress",
        &i18n::args([("secs", "{secs}".into())]),
    )
}

// ---------------------------------------------------------------------------
// Document canvas
