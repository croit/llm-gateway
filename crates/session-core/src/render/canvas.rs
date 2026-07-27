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
    /// Whether the version on display was written by the user rather than
    /// the assistant — shown as a badge so their own correction is
    /// distinguishable from the model's next pass over it.
    pub hand_edited: bool,
    /// Version numbers this document has, newest first, each flagged when the
    /// *user* wrote it. Empty for a single-version document (the switcher is
    /// hidden then anyway).
    ///
    /// Why per-version and not just the current one: scrubbing back through a
    /// history where every entry reads `v4 v3 v2 v1` tells you nothing about
    /// which one you fixed by hand — and that is exactly the revision you go
    /// looking for after the model has written over it twice.
    pub versions: Vec<(i64, bool)>,
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
    let copy_label = CopyLabels::for_lang(lang);
    let body_html = if is_markdown {
        render_markdown_with_copy(c.content, &copy_label)
    } else {
        // Escaped source view. `(text)` escapes; wrap in <pre><code>. A
        // non-markdown document *is* one big code block, so it gets the
        // same one-click copy as a fenced block in prose.
        add_code_copy_buttons(
            &html! { pre(class: "document-canvas__source") { code { (c.content) } } }
                .to_html()
                .to_string(),
            &copy_label,
        )
    };
    let version_label = format!("v{}", c.version);
    let show_doc_switcher = c.all_docs.len() > 1;
    let show_versions = c.max_version > 1;
    let sid = c.session_id;
    let active = c.active_id;
    // Newest first. Fall back to a bare descending range when the caller
    // didn't load the per-version authors (nothing but the label is lost).
    let versions: Vec<(i64, bool)> = if c.versions.is_empty() {
        (1..=c.max_version).rev().map(|v| (v, false)).collect()
    } else {
        c.versions.clone()
    };
    let close_title = t(lang, "render-canvas-close-title");
    let close_aria = t(lang, "render-canvas-close-aria");
    let document_aria = t(lang, "render-canvas-document-aria");
    let version_aria = t(lang, "render-canvas-version-aria");
    let hand_edited_badge = t(lang, "render-canvas-hand-edited");
    // Only the newest version is editable. Editing while an older one is on
    // screen would either fork the history or silently save something the
    // user isn't looking at; restoring first (`v3` → restore → edit) keeps
    // one linear history, which is the canvas's whole promise.
    //
    // *Who* may edit is not decided here: this HTML is broadcast to every live
    // viewer of a shared conversation (and rendered from tool calls that have
    // no viewer at all), so the affordance is gated on the page shell's
    // `$canEditDocs` signal, which only the owner's own render seeds true.
    let can_edit = c.version == c.max_version;
    let edit_form = if can_edit {
        render_canvas_editor(sid, active, c.content, lang)
    } else {
        String::new()
    };
    let edit_label = t(lang, "render-canvas-edit-button");
    let by_you = t(lang, "render-canvas-version-by-you");

    html! {
        // `docEditing` is declared here, on the panel root, so every SSE patch
        // that replaces the panel (a save, a doc/version switch, a tool edit)
        // re-declares it as `false` — the panel always comes back in reading
        // mode rather than stranding a stale textarea over new content.
        div(
            id: "document-canvas",
            class: "document-canvas",
            "data-signals": "{docEditing: false}"
        ) {
            div(class: "document-canvas__header") {
                (icons::pencil(14))
                span(class: "document-canvas__title") { (c.title) }
                span(class: "document-canvas__badge") { (c.format.to_string()) }
                span(class: "document-canvas__badge") { (version_label) }
                if c.hand_edited {
                    span(class: "document-canvas__badge document-canvas__badge--you") {
                        (hand_edited_badge)
                    }
                }
                if can_edit {
                    button(
                        type: "button",
                        class: "document-canvas__edit",
                        "data-show": "$canEditDocs && !$docEditing",
                        "data-on:click": "$docEditing = true"
                    ) { (edit_label) }
                }
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
                        for (v, by_user) in versions.iter() {
                            // `v3 · you` for the revisions the reader wrote —
                            // the one thing that makes a history scrubbable.
                            (version_option(*v, *by_user, *v == c.version, &by_you))
                        }
                    }
                }
            }
            div(
                id: "document-canvas-body",
                class: "document-canvas__body document-prose",
                "data-show": "!$docEditing"
            ) {
                #(body_html)
            }
            if can_edit {
                #(edit_form)
            }
        }
    }
    .to_html()
    .to_string()
}

/// One `<option>` in the version switcher, marked when the user wrote that
/// revision.
///
/// A standalone helper returning a `&str`-built `Html`, for two reasons: the
/// `selected` attribute is presence-based (plait's `attr: (bool)` would render
/// `selected="false"`, which browsers honour as selected), and keeping the
/// `html!` out of the loop body avoids the macro moving the loop's locals into
/// per-attribute closures.
fn version_option(version: i64, by_user: bool, selected: bool, by_you: &str) -> Html {
    let label = if by_user {
        format!("v{version} · {by_you}")
    } else {
        format!("v{version}")
    };
    let value = version.to_string();
    if selected {
        html! { option(value: (value), selected: "selected") { (label) } }.to_html()
    } else {
        html! { option(value: (value)) { (label) } }.to_html()
    }
}

/// The hand-edit form: the document's raw source in a textarea, saved as a
/// new version.
///
/// Raw source for *every* format, markdown included — the panel renders
/// markdown to HTML for reading, but a human correcting a document needs the
/// text they can actually edit, and round-tripping HTML back to markdown
/// would rewrite passages nobody touched.
///
/// `@post(..., {contentType: 'form'})` is the same submit path the message-edit
/// form uses; the handler answers with an SSE patch that re-renders this panel
/// (new version, reading mode), so there is no client-side state to reconcile.
fn render_canvas_editor(session_id: &str, doc_id: &str, content: &str, lang: Lang) -> String {
    let url = format!("/chat/{session_id}/document/{doc_id}/edit");
    let save_label = t(lang, "render-canvas-save");
    let cancel_label = t(lang, "render-canvas-cancel");
    let hint = t(lang, "render-canvas-edit-hint");
    let content = content.to_string();
    html! {
        form(
            action: (url.clone()),
            method: "post",
            class: "document-canvas__editor",
            "data-show": "$canEditDocs && $docEditing",
            "data-on:submit__prevent": (format!("@post('{url}', {{contentType: 'form'}})"))
        ) {
            textarea(
                name: "content",
                class: "document-canvas__textarea",
                spellcheck: "false"
            ) { (content) }
            div(class: "document-canvas__editor-actions") {
                span(class: "document-canvas__editor-hint") { (hint) }
                button(type: "submit", class: "btn btn-sm btn-primary") { (save_label) }
                button(
                    type: "button",
                    class: "btn btn-sm btn-ghost",
                    "data-on:click": "$docEditing = false"
                ) { (cancel_label) }
            }
        }
    }
    .to_html()
    .to_string()
}
