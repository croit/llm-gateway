// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

/// The data a single document-canvas panel renders from. Pure value type
/// (no DB handles) so the gateway can build it from its `documents` store
/// and the same renderer serves the initial page load, the live SSE
/// inject after an edit, and the doc/version-switch GET route.
pub struct DocCanvas<'a> {
    /// Chat session the canvas belongs to — baked into the switcher URLs.
    pub session_id: &'a str,
    /// The document currently shown.
    pub active_id: &'a str,
    pub title: &'a str,
    /// Format string (`markdown` / `text` / `html` / `json` / `toml`).
    pub format: &'a str,
    /// Version on display and the document's latest version.
    pub version: i64,
    pub max_version: i64,
    /// Content of the shown version.
    pub content: &'a str,
    /// `(id, title)` of every document in the session (including the
    /// active one), for the document switcher. A single-element list hides
    /// the switcher.
    pub all_docs: Vec<(String, String)>,
}

/// Render the document-canvas panel as an HTML string. The caller places
/// it inside the always-present `#document-canvas-slot` column (a stable
/// morph target even before the first document); show/hide is driven by
/// the page's `$canvasOpen` datastar signal, not by this markup.
///
/// Markdown is rendered to formatted HTML; every other format is shown as
/// escaped source in a code block — never executed — so an `html` /
/// `json` document can't inject markup into the operator's page.
pub fn render_document_canvas(c: &DocCanvas<'_>, lang: Lang) -> String {
    let is_markdown = c.format.eq_ignore_ascii_case("markdown");
    let body_html = if is_markdown {
        render_markdown(c.content)
    } else {
        // Escaped source view. `(text)` escapes; wrap in <pre><code>.
        html! { pre(class: "document-canvas__source") { code { (c.content) } } }
            .to_html()
            .to_string()
    };
    let version_label = format!("v{}", c.version);
    let show_doc_switcher = c.all_docs.len() > 1;
    let show_versions = c.max_version > 1;
    let sid = c.session_id;
    let active = c.active_id;
    let versions: Vec<i64> = (1..=c.max_version).rev().collect();
    let close_title = t(lang, "render-canvas-close-title");
    let close_aria = t(lang, "render-canvas-close-aria");
    let document_aria = t(lang, "render-canvas-document-aria");
    let version_aria = t(lang, "render-canvas-version-aria");

    html! {
        div(id: "document-canvas", class: "document-canvas") {
            div(class: "document-canvas__header") {
                (icons::pencil(14))
                span(class: "document-canvas__title") { (c.title) }
                span(class: "document-canvas__badge") { (c.format.to_string()) }
                span(class: "document-canvas__badge") { (version_label) }
                // Closes the docked panel (sets the shared datastar signal).
                button(
                    type: "button",
                    class: "document-canvas__close",
                    title: (close_title),
                    "aria-label": (close_aria),
                    "data-on:click": "$canvasOpen = false"
                ) { (icons::x_mark(16)) }
            }
            div(class: "document-canvas__controls") {
                if show_doc_switcher {
                    select(
                        class: "select select-bordered select-xs",
                        "aria-label": (document_aria),
                        "data-on:change": (format!("@get('/chat/{sid}/document/' + evt.target.value)"))
                    ) {
                        for (id, title) in c.all_docs.iter() {
                            if id == active {
                                option(value: (id.clone()), selected: "selected") { (title.clone()) }
                            } else {
                                option(value: (id.clone())) { (title.clone()) }
                            }
                        }
                    }
                }
                if show_versions {
                    select(
                        class: "select select-bordered select-xs",
                        "aria-label": (version_aria),
                        "data-on:change": (format!("@get('/chat/{sid}/document/{active}?version=' + evt.target.value)"))
                    ) {
                        for v in versions.iter() {
                            if *v == c.version {
                                option(value: (v.to_string()), selected: "selected") { (format!("v{v}")) }
                            } else {
                                option(value: (v.to_string())) { (format!("v{v}")) }
                            }
                        }
                    }
                }
            }
            div(id: "document-canvas-body", class: "document-canvas__body document-prose") {
                #(body_html)
            }
        }
    }
    .to_html()
    .to_string()
}
