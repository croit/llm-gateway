// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

/// Max chars of args / output we paint into the `<pre>` block.
/// `fetch_url` can return up to 4 MB of text in `output.content`,
/// and the chat page's layout engine chokes on a single
/// monospace `<pre>` that large (the user's report: "expanding
/// the tool call crashes the chat page"). The full payload is
/// still in the DB + still went to the model; the UI just shows
/// a head + a "(truncated for display)" footer so the page stays
/// responsive. 16 KB is generous for human inspection — typical
/// debugging needs the first error / first JSON object, not every
/// byte of a fetched HTML page.
pub(crate) const TOOL_CALL_RENDER_CAP: usize = 16 * 1024;

pub(crate) fn truncate_for_display(raw: String, lang: Lang) -> String {
    if raw.len() <= TOOL_CALL_RENDER_CAP {
        return raw;
    }
    // Take by chars rather than bytes so we don't slice mid-UTF-8
    // sequence. Cap-as-bytes is fine for a head; the next char-
    // boundary find is bounded by 4 bytes max.
    let head_end = raw
        .char_indices()
        .take_while(|(i, _)| *i <= TOOL_CALL_RENDER_CAP)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(TOOL_CALL_RENDER_CAP);
    let mut out = String::with_capacity(head_end + 128);
    out.push_str(&raw[..head_end]);
    let note = t_args(
        lang,
        "render-tool-output-truncated",
        &i18n::args([
            ("bytes", raw.len().to_string().into()),
            ("chars", head_end.to_string().into()),
        ]),
    );
    out.push_str("\n\n…\n(");
    out.push_str(&note);
    out.push(')');
    out.push('\n');
    out
}

/// How many tool-call rows render flat before we fold them into one
/// expandable group. A few read fine inline; a dozen identical "Used
/// rag_search" rows just bury the answer (and push it down behind the
/// composer), so past this count we collapse them behind a single
/// summary the reader can unfold on click.
pub(crate) const TOOL_GROUP_THRESHOLD: usize = 3;

/// Render a turn's tool calls. At or below [`TOOL_GROUP_THRESHOLD`] each
/// call is its own `<details>` row (the original behaviour). Above it,
/// the rows are wrapped in a single collapsed `<details>` group whose
/// summary tallies the calls by name — so a tool-heavy turn stays one
/// compact line until the reader expands it, rather than swamping the
/// viewport. The individual rows (with their stable `tc-<id>` ids) live
/// unchanged inside the group, so streaming morphs and per-row
/// open/close state keep working.
pub fn render_tool_call_list(tools: &[ToolCall], turn_id: &str, lang: Lang) -> Html {
    if tools.len() <= TOOL_GROUP_THRESHOLD {
        return html! {
            for c in tools.iter() {
                (render_tool_call(c, lang))
            }
        }
        .to_html();
    }

    let group_id = format!("turn-{turn_id}-tools-group");
    let any_running = tools.iter().any(|c| c.status == ToolCallStatus::Running);
    let any_errored = tools.iter().any(|c| c.status == ToolCallStatus::Errored);

    // Tally by name, preserving first-seen order so the breakdown reads
    // in call order rather than hash order.
    let mut tally: Vec<(String, usize)> = Vec::new();
    for c in tools {
        if let Some(entry) = tally.iter_mut().find(|(n, _)| *n == c.name) {
            entry.1 += 1;
        } else {
            tally.push((c.name.clone(), 1));
        }
    }
    let breakdown = tally
        .iter()
        .map(|(n, count)| {
            if *count > 1 {
                format!("{n} ×{count}")
            } else {
                n.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let label = if any_running {
        t(lang, "render-tools-running")
    } else if any_errored {
        t(lang, "render-tools-errored")
    } else {
        t(lang, "render-tools-used")
    };
    let summary_text = t_args(
        lang,
        "render-tools-summary",
        &i18n::args([
            ("count", tools.len().to_string().into()),
            ("breakdown", breakdown.into()),
        ]),
    );

    html! {
        // Collapsed by default (like the thinking block). The reader
        // unfolds it on click; `data-preserve-attr="open"` keeps that
        // choice across morph re-renders.
        details(
            id: (group_id),
            class: "tool-calls-group",
            "data-preserve-attr": "open"
        ) {
            summary(class: "tool-call__summary tool-calls-group__summary") {
                span(class: "tool-call__indicator") {
                    if any_running {
                        (icons::spinner(14))
                    } else if any_errored {
                        (icons::alert(14))
                    } else {
                        (icons::check(14))
                    }
                }
                span(class: "tool-call__label") { (label) " " }
                span(class: "tool-call__name") { (summary_text) }
            }
            div(class: "tool-calls-group__body flex flex-col") {
                for c in tools.iter() {
                    (render_tool_call(c, lang))
                }
            }
        }
    }
    .to_html()
}

/// One tool-call row. `<details>` so the user can expand to see
/// input and output. `data-preserve-attr="open"` keeps their
/// toggle state across re-renders.
pub fn render_tool_call(call: &ToolCall, lang: Lang) -> Html {
    // Scope the DOM id by turn: `call.id` is only unique within its turn, but
    // the full conversation renders every turn into one document, so two turns
    // that each used `call_0` would otherwise clash on a duplicate element id.
    let dom_id = format!("tc-{}-{}", call.turn_id, call.id);
    let args_pretty = match serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| call.arguments_json.clone()),
        Err(_) => call.arguments_json.clone(),
    };
    let args_pretty = truncate_for_display(args_pretty, lang);
    let output_pretty = call
        .output_json
        .clone()
        .map(|s| truncate_for_display(s, lang));
    // Long-running tools (currently the ComfyUI workflow family) keep their
    // tool future pending while the background job runs, so the row sits in
    // `Running` for the whole generation — label that "running" rather than
    // the momentary "calling" shown for fast synchronous tools. This prefix
    // is the single, deliberate signal: the persisted tool-call row carries
    // no generic async flag to key on.
    let is_long_running = call.name.starts_with("comfyui_");
    let is_running = call.status == ToolCallStatus::Running;
    let status_label = match call.status {
        ToolCallStatus::Running if is_long_running => t(lang, "render-tools-running"),
        ToolCallStatus::Running => t(lang, "render-tool-status-calling"),
        ToolCallStatus::Completed => t(lang, "render-tool-status-used"),
        ToolCallStatus::Errored => t(lang, "render-tool-status-error"),
    };
    let input_label = t(lang, "render-tool-input-label");
    let output_label = t(lang, "render-tool-output-label");
    let name = call.name.clone();
    html! {
        details(
            id: (dom_id),
            class: "tool-call",
            "data-preserve-attr": "open"
        ) {
            summary(class: "tool-call__summary") {
                span(class: "tool-call__indicator") {
                    if is_running {
                        (icons::spinner(14))
                    } else if call.status == ToolCallStatus::Errored {
                        (icons::alert(14))
                    } else {
                        (icons::check(14))
                    }
                }
                span(class: "tool-call__label") { (status_label) " " }
                span(class: "tool-call__name") { (name) }
            }
            div(class: "tool-call__body") {
                div(class: "tool-call__section") {
                    div(class: "tool-call__section-label") { (input_label) }
                    pre(class: "tool-call__code") { (args_pretty) }
                }
                if let Some(out) = output_pretty.as_ref() {
                    div(class: "tool-call__section") {
                        div(class: "tool-call__section-label") { (output_label) }
                        pre(class: "tool-call__code") { (out.clone()) }
                    }
                }
            }
        }
    }
    .to_html()
}

// ---------------------------------------------------------------------------
// Composer
