// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! HTML renderers for the chat-style session UI.
//!
//! Driver-agnostic. The gateway uses these for OpenAI-backed turns;
//! a future consumer could reuse them for its own driver. The
//! POST/tail URLs and a few cosmetic toggles are parameterised
//! through `ComposerOpts` / `render_conversation`'s
//! `in_flight_tail_url`.
//!
//! All renderers are pure functions of their inputs — same shape on
//! the initial server render and on every SSE patch the worker
//! drives, so the morphdom-style diff datastar runs across patches
//! preserves per-element interactive state (collapsed `<details>`,
//! scroll position) automatically.
//!
//! Per-turn DOM ids carry the turn UUID so two concurrent stream
//! attaches (multiple tabs, retry-on-recover) can't cross-write.

use plait::{Html, ToHtml, html};

use crate::db::{ToolCall, ToolCallStatus, Turn, TurnRole, TurnStatus, TurnWithTools};
use crate::icons;

// ---------------------------------------------------------------------------
// Markdown

/// GFM with raw HTML / `javascript:` / `vbscript:` rejected — the LLM
/// can emit `<script>` inside a fenced code block and we still want
/// it to render as escaped text, not execute. Fenced code blocks
/// with a recognised language hint are then post-processed through
/// `lumis` for server-side syntax highlighting (inline-styled spans).
///
/// Markdown image parsing (`![alt](url)`) is disabled. The model never
/// has a legitimate way to produce an image through prose — every image
/// in the chat arrives via the `[gw-attachment …]` marker pipeline
/// (rendered by `render_attachment`). So a markdown image is always
/// either a hallucinated/echoed URL — e.g. `![](image_url)`,
/// `![](preview_url)`, `![](<turn-id>/letter.png)` — that the browser
/// resolves *relative* to the `/chat/<id>` page (`/chat/image_url`,
/// `/chat/<turn-id>/letter.png`) and 404s, and, being re-emitted on
/// every streaming morph, re-fetched until the edge rate-limiter answers
/// 429. Disabling the construct degrades `![alt](url)` to a harmless
/// inline link the browser doesn't auto-load, rather than a live
/// `<img src>`. (Raw `<img>` HTML the model types is already escaped to
/// text by `Options::gfm()`'s `allow_dangerous_html = false`.)
pub fn render_markdown(text: &str) -> String {
    let mut options = markdown::Options::gfm();
    options.parse.constructs.label_start_image = false;
    let html =
        markdown::to_html_with_options(text, &options).unwrap_or_else(|_| markdown::to_html(text));
    highlight_fenced_code_blocks(&html)
}

/// Lumis themes loaded once at process start. We hand them to the
/// multi-themes formatter so every styled token comes out as
/// `color: light-dark(<day>, <night>)`; the browser picks the right
/// half from the document's `color-scheme`, which daisyUI sets per
/// `data-theme`. No second render needed when the user toggles
/// themes.
static LIGHT_THEME: std::sync::LazyLock<lumis::themes::Theme> = std::sync::LazyLock::new(|| {
    lumis::themes::get("tokyonight_day").expect("lumis ships the tokyonight_day theme")
});
static DARK_THEME: std::sync::LazyLock<lumis::themes::Theme> = std::sync::LazyLock::new(|| {
    lumis::themes::get("tokyonight_night").expect("lumis ships the tokyonight_night theme")
});

/// Match `<pre><code class="language-FOO">…</code></pre>` from the
/// markdown crate's GFM output. Pattern is exact-shape; markdown
/// doesn't intersperse other attributes or whitespace inside the
/// open tags. Language captures `[\w+\-.]+` so identifiers like
/// `c++` (encoded as `c++`), `objective-c`, `f#` (encoded), or
/// `csharp` all match. The body capture is lazy so consecutive
/// code blocks don't merge into one giant match.
static FENCED_CODE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"(?s)<pre><code class="language-([\w+\-.]+)">(.*?)</code></pre>"#).unwrap()
});

fn highlight_fenced_code_blocks(html: &str) -> String {
    FENCED_CODE_RE
        .replace_all(html, |caps: &regex::Captures<'_>| {
            let lang_hint = &caps[1];
            let escaped_source = &caps[2];
            let source = html_unescape(escaped_source);
            // Lumis returns its own `<pre class="lumis"…><code…>` —
            // we replace the markdown crate's wrapper entirely so
            // we don't end up with a nested `<pre><pre>`. If lumis
            // doesn't know the language we fall back to the
            // markdown wrapper (plain monospace, no colour).
            match highlight_one(lang_hint, &source) {
                Some(highlighted) => highlighted,
                None => caps[0].to_string(),
            }
        })
        .into_owned()
}

/// Strip the inline `style="…"` lumis writes onto its outer `<pre>`
/// (it carries the theme's foreground + background). We let
/// `.chat-prose pre` (which already paints `--color-base-200` +
/// matching border + radius) provide the surface so highlighted
/// and un-highlighted blocks share one visual treatment. The
/// per-token `<span style>` colours stay — they're what actually
/// makes the highlighting visible.
static PRE_STYLE_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r#"^(<pre[^>]*?) style="[^"]*""#).unwrap());

/// Invoke lumis for one code block. Returns `None` if the language
/// hint doesn't resolve (lumis returns `LanguageParseError`) or the
/// formatter errors out — caller falls back to the un-highlighted
/// HTML so the block still renders, just without colour.
fn highlight_one(lang_hint: &str, source: &str) -> Option<String> {
    use lumis::formatters::Formatter;
    let language: lumis::languages::Language = normalise_lang_hint(lang_hint).parse().ok()?;
    let mut themes = std::collections::HashMap::new();
    themes.insert("light".to_string(), LIGHT_THEME.clone());
    themes.insert("dark".to_string(), DARK_THEME.clone());
    let formatter = lumis::HtmlMultiThemesBuilder::new()
        .language(language)
        .themes(themes)
        .default_theme("light-dark()")
        .build()
        .ok()?;
    let mut output = Vec::new();
    formatter.format(source, &mut output).ok()?;
    let raw = String::from_utf8(output).ok()?;
    Some(PRE_STYLE_RE.replace(&raw, "$1").into_owned())
}

/// Map common markdown-fence shorthands to the canonical names
/// lumis recognises. Markdown writers use `sh`, `py`, `js`, `ts`,
/// `yml` interchangeably with the long form. Unrecognised hints
/// fall through unchanged and either parse against lumis directly
/// or fail open (un-highlighted code block).
fn normalise_lang_hint(hint: &str) -> &str {
    match hint.to_ascii_lowercase().as_str() {
        "sh" | "shell" | "zsh" => "bash",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "yml" => "yaml",
        "rs" => "rust",
        "c++" | "cxx" => "cpp",
        _ => hint,
    }
}

/// Decode the entity set the `markdown` crate emits inside `<code>`
/// blocks. The set is bounded — `&amp;`, `&lt;`, `&gt;`, `&quot;`,
/// `&#39;` — so we don't need a full HTML parser. Order matters:
/// decode `&amp;` last so a nested entity like `&amp;lt;` (which
/// means a literal `&lt;` in the source) round-trips through to
/// `&lt;` instead of decoding into `<` by accident.
fn html_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------------------------
// Conversation

/// All rendered turns + the in-flight tail subscription, wrapped in
/// the scroll container.
///
/// `in_flight_tail_url` is the URL the page should `@get` to attach
/// to a live worker — e.g. `/chat/{id}/tail` for the gateway. Pass
/// `None` when nothing's streaming; `chatScroll.init` still wires
/// the conversation observer (scroll-to-top-on-send + tail-space
/// reserve).
///
/// `actions` is the base path for per-message retry/edit actions
/// (`Some("/chat")` on the gateway). When `None`, no action buttons
/// render.
pub fn render_conversation(
    turns: &[TurnWithTools],
    in_flight_tail_url: Option<&str>,
    actions: Option<&str>,
) -> Html {
    let turns_owned: Vec<TurnWithTools> = turns.to_vec();
    // `data-init` fires every time datastar mounts this element —
    // initial render *and* every nav patch — so a phone that's
    // been backgrounded mid-stream attaches to the worker the
    // moment the page comes back.
    // Optional-chain the call: the surface's app.js must load before datastar
    // (it defines `window.chatScroll`), but guard anyway so a load-order slip
    // degrades to "no auto-scroll" rather than a thrown ExecuteExpression that
    // aborts the whole `data-init` (and with it the `@get` tail attach).
    let init_directive = match in_flight_tail_url {
        Some(url) => format!("window.chatScroll?.init?.(el); @get('{url}')"),
        None => "window.chatScroll?.init?.(el)".to_string(),
    };
    html! {
        section(
            id: "conversation",
            "data-init": (init_directive)
        ) {
            for t in turns_owned.iter() {
                (render_turn(t, actions))
            }
        }
    }
    .to_html()
}

/// Dispatch on role. Renders the right bubble shape. `actions` is the
/// retry/edit base path (see [`render_conversation`]).
pub fn render_turn(turn: &TurnWithTools, actions: Option<&str>) -> Html {
    match turn.turn.role {
        TurnRole::User => render_user_turn(&turn.turn, actions),
        TurnRole::Assistant => render_assistant_turn(turn, actions),
    }
}

pub fn render_user_turn(turn: &Turn, actions: Option<&str>) -> Html {
    let content = turn.user_content.clone().unwrap_or_default();
    let dom_id = format!("turn-{}", turn.id);
    let segments = crate::attachments::split_markers(&content);
    let has_attachments = segments
        .iter()
        .any(|s| matches!(s, crate::attachments::Segment::Attachment(_)));
    let show_actions = actions.is_some();
    // Build the edit affordance first — it borrows `content` before the
    // body macro moves a clone of it into a closure.
    let edit_block = render_user_edit(turn, actions.unwrap_or(""), &content);
    // The message body — either the plain text fast path or the
    // text+attachment segmented path. Kept in `.chat-msg__body` so the
    // edit form can replace it visually via the `.editing` class.
    let body = if !has_attachments {
        let body_text = content.clone();
        html! { div(class: "chat-msg__body") { (body_text) } }.to_html()
    } else {
        html! {
            div(class: "chat-msg__body") {
                for seg in segments.iter() {
                    match seg {
                        crate::attachments::Segment::Text(t) => {
                            if !t.is_empty() {
                                div(class: "chat-msg__prose") { (t.to_string()) }
                            }
                        }
                        crate::attachments::Segment::Attachment(att) => {
                            (render_attachment(att, &turn.id))
                        }
                    }
                }
            }
        }
        .to_html()
    };
    html! {
        div(id: (dom_id), class: "chat-msg--user") {
            (body)
            if show_actions {
                (edit_block)
            }
        }
    }
    .to_html()
}

/// `/{base}/{session}/turns/{turn}/{action}` — the per-message action
/// endpoint URL.
fn action_url(base: &str, turn: &Turn, action: &str) -> String {
    format!("{base}/{}/turns/{}/{action}", turn.session_id, turn.id)
}

/// Datastar submit directive for a retry/edit form: copy the current
/// model dropdown into the form's hidden `model` input, confirm the
/// destructive drop, then `@post` (whose SSE response streams the
/// regenerated reply back in).
fn action_submit(url: &str, confirm: &str) -> String {
    format!(
        "window.chatActions.fillModel(el) && confirm('{confirm}') && \
         @post('{url}', {{contentType: 'form'}})"
    )
}

/// The user-bubble edit affordance: a hover "Edit" button + a hidden
/// inline edit form (revealed by toggling `.editing` on the bubble via
/// `window.chatActions`). Submitting drops everything below this turn
/// and regenerates from the edited text.
fn render_user_edit(turn: &Turn, base: &str, content: &str) -> Html {
    let id = turn.id.clone();
    let content = content.to_string();
    let edit_url = action_url(base, turn, "edit");
    let submit = action_submit(
        &edit_url,
        "Save and regenerate? This deletes all messages below.",
    );
    let start = format!("window.chatActions.editStart('{id}')");
    let cancel = format!("window.chatActions.editCancel('{id}')");
    html! {
        div(class: "chat-msg__actions") {
            button(
                type: "button",
                class: "chat-msg__action",
                "data-on:click": (start)
            ) {
                "✎ Edit"
            }
        }
        form(
            action: (edit_url),
            method: "post",
            class: "chat-msg__edit",
            "data-on:submit__prevent": (submit)
        ) {
            input(type: "hidden", name: "model");
            textarea(name: "message", class: "chat-msg__edit-textarea") { (content) }
            div(class: "chat-msg__edit-actions") {
                button(type: "submit", class: "btn btn-sm btn-primary") { "Save & regenerate" }
                button(
                    type: "button",
                    class: "btn btn-sm btn-ghost",
                    "data-on:click": (cancel)
                ) { "Cancel" }
            }
        }
    }
    .to_html()
}

/// One attachment chip / inline image. Role-agnostic — the same
/// renderer fires for user-uploaded files and assistant-uploaded
/// files (via the `upload_attachment` tool) so the UI stays DRY and
/// the model's attachments look identical to the user's.
///
/// Images are `<img>`-displayed at a thumbnail cap (max ~16 rem each
/// side) linked through to the full-res URL; everything else gets a
/// neutral chip with a mime-aware icon + filename + byte size.
/// Extract the turn id baked into a gateway attachment URL
/// (`/chat/attachment/<turn_id>/<filename>`). Returns `None` for any URL
/// not in that canonical shape, so a non-gateway URL is never misjudged.
fn attachment_turn_id(url: &str) -> Option<&str> {
    url.strip_prefix("/chat/attachment/")
        .and_then(|rest| rest.split('/').next())
        .filter(|id| !id.is_empty())
}

fn render_attachment(att: &crate::attachments::ParsedAttachment, owner_turn_id: &str) -> Html {
    let url = att.url.clone();
    let filename = att.filename.clone();
    let mime = att.mime.clone();
    let size = format_bytes(att.size);
    // Orphaned marker: an attachment URL carries the id of the turn that
    // owns it (the upload writes the marker into that same turn), so a
    // turn_id that doesn't match the turn we're rendering means the bytes
    // are unreachable — the fetch route would 404 "no such turn". This can
    // happen with rows left behind by an older build or an interrupted
    // generation. Render a muted, link-less placeholder rather than a
    // broken <img> or a download link that dead-ends.
    if let Some(att_turn) = attachment_turn_id(&url)
        && att_turn != owner_turn_id
    {
        return html! {
            span(
                class: "chat-msg__attachment-chip chat-msg__attachment-chip--missing",
                title: "This attachment is no longer available"
            ) {
                span(class: "chat-msg__attachment-icon") { (icons::paperclip(14)) }
                span(class: "chat-msg__attachment-name") { (filename) }
                span(class: "chat-msg__attachment-meta") { "unavailable" }
            }
        }
        .to_html();
    }
    if att.is_image() {
        let alt = filename.clone();
        // A preview image (e.g. a typst render's PNG) clicks through to
        // its `link` (the PDF) when set; an ordinary image links to its
        // own full-res bytes. The `<img src>` is always the image url.
        let href = att.link.clone().unwrap_or_else(|| url.clone());
        let title = match &att.link {
            Some(_) => format!("Open {filename} · {mime} · {size}"),
            None => format!("{filename} · {mime} · {size}"),
        };
        return html! {
            a(href: (href), target: "_blank", rel: "noopener", class: "chat-msg__attachment-image") {
                img(src: (url), alt: (alt), title: (title), loading: "lazy");
            }
        }
        .to_html();
    }
    html! {
        a(
            href: (url.clone()),
            target: "_blank",
            rel: "noopener",
            class: "chat-msg__attachment-chip",
            title: (format!("{mime} · {size}"))
        ) {
            span(class: "chat-msg__attachment-icon") { (icons::paperclip(14)) }
            span(class: "chat-msg__attachment-name") { (filename) }
            span(class: "chat-msg__attachment-meta") { (size) }
        }
    }
    .to_html()
}

fn format_bytes(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let kb = n as f64 / 1024.0;
    if kb < 1024.0 {
        return format!("{kb:.1} KB");
    }
    let mb = kb / 1024.0;
    format!("{mb:.1} MB")
}

/// One slice of an assistant bubble — either pre-rendered markdown
/// prose or an attachment the model produced via `upload_attachment`.
/// We pre-render the markdown inside `assistant_segments` (rather
/// than passing the raw text through the bubble loop) so each
/// segment carries its own escaped HTML, the same way the upstream
/// renderer worked before split-marker support.
enum AssistantSegment {
    Prose(String),
    Attachment(crate::attachments::ParsedAttachment),
}

fn assistant_segments(content: &str) -> Vec<AssistantSegment> {
    let raw_segs = crate::attachments::split_markers(content);
    // Fast path: no attachment markers in the assistant content —
    // render the whole thing as one markdown block so the existing
    // streaming/morph behavior is byte-identical to pre-marker code.
    if !raw_segs
        .iter()
        .any(|s| matches!(s, crate::attachments::Segment::Attachment(_)))
    {
        return vec![AssistantSegment::Prose(render_markdown(content))];
    }
    raw_segs
        .into_iter()
        .filter_map(|s| match s {
            crate::attachments::Segment::Text(t) if !t.is_empty() => {
                Some(AssistantSegment::Prose(render_markdown(t)))
            }
            crate::attachments::Segment::Text(_) => None,
            crate::attachments::Segment::Attachment(a) => Some(AssistantSegment::Attachment(a)),
        })
        .collect()
}

pub fn render_assistant_turn(t: &TurnWithTools, actions: Option<&str>) -> Html {
    let turn = t.turn.clone();
    let tools = t.tool_calls.clone();
    let dom_id = format!("turn-{}", turn.id);
    let reasoning = turn.reasoning.clone().unwrap_or_default();
    let content = turn.content.clone().unwrap_or_default();
    let elapsed_ms = turn.reasoning_elapsed_ms;
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
    let segments = assistant_segments(&content);
    let has_reasoning = !reasoning.is_empty();

    html! {
        div(id: (dom_id), class: "chat-msg--assistant") {
            // Reasoning block. Conditionally rendered: when reasoning
            // exists we emit the `<details>` shell; otherwise a tiny
            // empty placeholder (so subsequent inner-patches of the
            // bubble can morph it in without touching siblings).
            if has_reasoning {
                (render_thinking_block(&turn.id, &reasoning, elapsed_ms, !in_progress))
            } else {
                div(id: (thinking_id.clone()), class: "thinking-block-slot") {}
            }
            // Tool calls. Each row has its own stable id (`tc-<id>`)
            // so datastar's morph preserves user open/close state
            // across re-renders.
            div(id: (tools_id), class: "tool-calls flex flex-col") {
                (render_tool_call_list(&tools, &turn.id))
            }
            // Main response text. Each prose segment is its own
            // markdown-rendered block, with attachment chips/images
            // spliced inline at the model's write-position.
            div(id: (text_id), class: "chat-prose") {
                for seg in segments.iter() {
                    match seg {
                        AssistantSegment::Prose(html) => {
                            #(html.clone())
                        }
                        AssistantSegment::Attachment(att) => {
                            (render_attachment(att, &turn.id))
                        }
                    }
                }
            }
            // "Thinking…" spinner. Visible only when the turn is
            // in-progress AND no content has landed yet — CSS
            // handles the toggle so we don't need to render
            // conditionally on each tick.
            if show_spinner {
                div(class: "thinking flex items-center gap-2 text-base-content/60 text-sm") {
                    (icons::spinner(16))
                    span { "Thinking…" }
                }
            }
            if errored {
                div(class: "alert alert-error mt-2") {
                    (icons::alert(16))
                    span { (error_msg) }
                }
            }
            // Retry — only on a settled turn (never mid-stream). Drops
            // this reply + everything below and regenerates from the
            // preceding user message with the currently-selected model.
            if actions.is_some() && !in_progress {
                (render_retry_action(&turn, actions.unwrap_or("")))
            }
        }
    }
    .to_html()
}

/// Hover "Retry" affordance under a settled assistant bubble.
fn render_retry_action(turn: &Turn, base: &str) -> Html {
    let retry_url = action_url(base, turn, "retry");
    let submit = action_submit(
        &retry_url,
        "Regenerate this reply? This deletes it and everything below.",
    );
    html! {
        div(class: "chat-msg__actions") {
            form(
                action: (retry_url),
                method: "post",
                class: "m-0",
                "data-on:submit__prevent": (submit)
            ) {
                input(type: "hidden", name: "model");
                button(type: "submit", class: "chat-msg__action") { "↻ Retry" }
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
    finalized: bool,
) -> Html {
    let body_id = format!("turn-{turn_id}-thinking-body");
    let shell_id = format!("turn-{turn_id}-thinking");
    let summary_id = format!("turn-{turn_id}-thinking-summary");
    let elapsed_secs = elapsed_ms.map(|ms| ms as f64 / 1000.0).unwrap_or(0.0);
    let summary_label = if finalized {
        format!("Thought for {elapsed_secs:.1}s")
    } else {
        format!("Thinking… ({elapsed_secs:.1}s)")
    };
    let rendered = render_markdown(reasoning);
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
                if !finalized {
                    span(class: "thinking-block__indicator") { (icons::spinner(12)) }
                }
                span(class: "thinking-block__label") { (summary_label) }
            }
            div(id: (body_id), class: "thinking-prose") {
                #(rendered)
            }
        }
    }
    .to_html()
}

// ---------------------------------------------------------------------------
// Document canvas

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
pub fn render_document_canvas(c: &DocCanvas<'_>) -> String {
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
                    title: "Close",
                    "aria-label": "Close document canvas",
                    "data-on:click": "$canvasOpen = false"
                ) { (icons::x_mark(16)) }
            }
            div(class: "document-canvas__controls") {
                if show_doc_switcher {
                    select(
                        class: "select select-bordered select-xs",
                        "aria-label": "Document",
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
                        "aria-label": "Version",
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
const TOOL_CALL_RENDER_CAP: usize = 16 * 1024;

fn truncate_for_display(raw: String) -> String {
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
    out.push_str(&format!(
        "\n\n…\n(truncated for display — full {} bytes still available to the model + persisted in the DB; \
         displaying first {} chars)\n",
        raw.len(),
        head_end,
    ));
    out
}

/// How many tool-call rows render flat before we fold them into one
/// expandable group. A few read fine inline; a dozen identical "Used
/// rag_search" rows just bury the answer (and push it down behind the
/// composer), so past this count we collapse them behind a single
/// summary the reader can unfold on click.
const TOOL_GROUP_THRESHOLD: usize = 3;

/// Render a turn's tool calls. At or below [`TOOL_GROUP_THRESHOLD`] each
/// call is its own `<details>` row (the original behaviour). Above it,
/// the rows are wrapped in a single collapsed `<details>` group whose
/// summary tallies the calls by name — so a tool-heavy turn stays one
/// compact line until the reader expands it, rather than swamping the
/// viewport. The individual rows (with their stable `tc-<id>` ids) live
/// unchanged inside the group, so streaming morphs and per-row
/// open/close state keep working.
pub fn render_tool_call_list(tools: &[ToolCall], turn_id: &str) -> Html {
    if tools.len() <= TOOL_GROUP_THRESHOLD {
        return html! {
            for c in tools.iter() {
                (render_tool_call(c))
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
        "Running tools"
    } else if any_errored {
        "Tool calls"
    } else {
        "Used tools"
    };
    let summary_text = format!("{} calls · {breakdown}", tools.len());

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
                    (render_tool_call(c))
                }
            }
        }
    }
    .to_html()
}

/// One tool-call row. `<details>` so the user can expand to see
/// input and output. `data-preserve-attr="open"` keeps their
/// toggle state across re-renders.
pub fn render_tool_call(call: &ToolCall) -> Html {
    let dom_id = format!("tc-{}", call.id);
    let args_pretty = match serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| call.arguments_json.clone()),
        Err(_) => call.arguments_json.clone(),
    };
    let args_pretty = truncate_for_display(args_pretty);
    let output_pretty = call.output_json.clone().map(truncate_for_display);
    let is_running = call.status == ToolCallStatus::Running;
    let status_label = match call.status {
        ToolCallStatus::Running => "Calling",
        ToolCallStatus::Completed => "Used",
        ToolCallStatus::Errored => "Tool error",
    };
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
                    div(class: "tool-call__section-label") { "Input" }
                    pre(class: "tool-call__code") { (args_pretty) }
                }
                if let Some(out) = output_pretty.as_ref() {
                    div(class: "tool-call__section") {
                        div(class: "tool-call__section-label") { "Output" }
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

/// Knobs the composer renderer needs that aren't universal across
/// drivers.
pub struct ComposerOpts<'a> {
    /// Where the form submits. `/chat/{id}/messages` for the gateway.
    pub post_url: &'a str,
    /// Where the stop button posts. `/chat/{id}/cancel` for the
    /// gateway.
    pub cancel_url: &'a str,
    /// Textarea placeholder.
    pub placeholder: &'a str,
    /// Voice-input mic button. The gateway shows this when the user
    /// has a transcription model available.
    pub has_voice: bool,
    /// Initial value of the `$chatStreaming` signal. `true` when a turn is
    /// already in flight at render time, so the Stop control shows on a
    /// fresh load / reload — not just after a submit set the signal in JS.
    /// Without this, reloading mid-turn leaves no way to stop a runaway.
    pub streaming: bool,
    /// Optional toolbar row rendered inside the composer, above the input —
    /// the host app's per-message controls (the gateway puts its "+" tools /
    /// integrations / skills menu here). `None` renders no row. Must contain
    /// no `<form>` (the composer itself is a form; nested forms are invalid) —
    /// use button-driven actions instead.
    pub toolbar: Option<Html>,
}

pub fn render_composer(opts: ComposerOpts<'_>) -> Html {
    let ComposerOpts {
        post_url,
        cancel_url,
        placeholder,
        has_voice,
        streaming,
        toolbar,
    } = opts;
    let submit_directive = format!(
        "window.chatComposer.onSubmit(evt) && ($chatStreaming = true, \
         @post('{post_url}', {{contentType: 'form'}}))"
    );
    let cancel_directive = format!("@post('{cancel_url}'); $chatStreaming = false");
    let placeholder = placeholder.to_string();
    // Seed `$chatStreaming` from the server's knowledge of whether a turn is
    // live, so Stop is present on load/reload (not only after a JS submit).
    let initial_signals = format!("{{chatStreaming: {streaming}}}");
    // Pre-render the optional toolbar (empty fragment when absent) so it can be
    // interpolated by value inside the macro's `Fn` closure.
    let toolbar_html = toolbar.unwrap_or_else(|| html! { "" }.to_html());
    html! {
        form(
            id: "chat-form",
            "data-signals": (initial_signals),
            "data-class": "{'chat-composer--streaming': $chatStreaming}",
            "data-on:submit__prevent": (submit_directive),
            "data-on:dragover__prevent": "window.chatComposer.onDragOver(evt)",
            "data-on:dragleave__prevent": "window.chatComposer.onDragLeave(evt)",
            "data-on:drop__prevent": "window.chatComposer.onDrop(evt)",
            "data-on:paste": "window.chatComposer.onPaste(evt)",
            method: "post",
            enctype: "multipart/form-data",
            class: "chat-composer"
        ) {
            // Hidden file input — `name="attachment"` so the
            // backend's multipart parser picks it up; `multiple`
            // accepts batch picks. The composer.ts paste/drop
            // handlers replace `.files` via DataTransfer so all
            // attachment sources flow through this one element.
            input(
                id: "chat-attachments-input",
                name: "attachment",
                type: "file",
                multiple: "multiple",
                hidden: "hidden",
                "data-on:change": "window.chatComposer.onFilesPicked(evt)"
            );
            // Host-app toolbar row (gateway: the "+" tools/integrations/skills
            // menu + active chips). Rendered above the field; contains no form.
            // Empty fragment when none was supplied.
            (toolbar_html.clone())
            // Chip strip — populated by composer.ts as files land.
            // Empty container; CSS hides it while no children.
            div(
                id: "chat-attachments-chips",
                class: "chat-composer__chips"
            ) {}
            div(class: "chat-composer__field") {
                textarea(
                    id: "message",
                    name: "message",
                    rows: "1",
                    placeholder: (placeholder),
                    // Focus the composer on a full page load so the user
                    // can start typing immediately. The Datastar nav path
                    // (+ New chat / switching chats) re-focuses via the
                    // nav script, since `autofocus` only fires on initial
                    // parse.
                    autofocus: "autofocus",
                    "data-on:keydown": "window.chatComposer.onKeydown(evt)",
                    class: "chat-composer__textarea"
                ) {}
                div(class: "chat-composer__action") {
                    // Attach button — opens the hidden file input.
                    button(
                        type: "button",
                        "data-on:click": "window.chatComposer.openFilePicker()",
                        "aria-label": "Attach files",
                        title: "Attach files (also drop / paste)",
                        class: "btn btn-sm btn-circle btn-ghost chat-composer__attach"
                    ) {
                        (icons::paperclip(16))
                    }
                    if has_voice {
                        div(class: "voice-control") {
                            div(class: "voice-level", "data-mic-meter": "1") {
                                span {}
                                span {}
                                span {}
                                span {}
                            }
                            button(
                                type: "button",
                                "data-on:click": "window.chatMic.toggle(el)",
                                "aria-label": "Record voice message",
                                title: "Record",
                                class: "btn btn-sm btn-circle btn-ghost data-[recording=1]:btn-error"
                            ) {
                                span(class: "mic-idle") { (icons::mic(16)) }
                                span(class: "mic-recording") { (icons::stop(16)) }
                                span(class: "mic-transcribing") { (icons::spinner(16)) }
                            }
                        }
                    }
                    button(
                        type: "submit",
                        class: "btn btn-sm btn-circle btn-primary chat-composer__send",
                        "aria-label": "Send",
                        title: "Send"
                    ) {
                        (icons::send(16))
                    }
                    button(
                        type: "button",
                        "data-on:click": (cancel_directive),
                        class: "btn btn-sm btn-circle btn-error chat-composer__stop",
                        "aria-label": "Stop",
                        title: "Stop"
                    ) {
                        (icons::stop(16))
                    }
                }
            }
        }
    }
    .to_html()
}

// ---------------------------------------------------------------------------
// Busy-button helpers.
//
// Datastar's `data-indicator="<signal>"` directive flips `$<signal>`
// to `true` while a request issued from that element is in flight,
// back to `false` when it settles. Pair that with two spans whose
// visibility is driven by `data-show` and you get a "label →
// spinner → label" swap with no JS on the page side.
//
// The two functions below are the DRY-est expression of that
// pattern across the codebase:
//
//   * `render_busy_post_form` — a single-button form. Wraps the
//     button in a `<form>` with the `@post` directive + indicator
//     wiring. Use this for any one-shot action button (delete row,
//     stop, probe, …). Each call site MUST pass a `busy_signal`
//     unique to that button on the page so concurrent clicks on
//     siblings don't share state. Hashing the action URL is a good
//     default but we leave that to the caller so signal names stay
//     stable across re-renders.
//   * `render_busy_submit` — just the submit button. Use inside
//     bigger forms (multi-field create dialogs) whose `<form>` you
//     already authored with `data-indicator` on it.
//
// Both render `<span data-show="$<signal>" style="display:none">…
// spinner …</span>` for the busy state. Datastar's `data-show`
// flips `style.display` between `none` and an empty string at
// runtime, so the initial render shows the label and never the
// spinner. No FOUC.
//
// Why two spans + `data-show` rather than `data-class` on the
// button: keeps the helper a pure renderer with no CSS-side
// dependency to add or remember elsewhere.

/// Strip everything that isn't `[A-Za-z0-9_]` from `raw` so the
/// result is safe to use as a datastar signal-name suffix (those
/// live in a JS-identifier namespace). Hyphens etc. become
/// underscores; empty input becomes a single underscore so the
/// suffix is never empty.
pub fn sanitize_signal_name(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Action-form spec for `render_busy_post_form`.
pub struct BusyPostForm<'a> {
    /// URL the form POSTs to. Used both as the form's `action`
    /// attribute (for no-JS fallbacks) and inside the `@post(...)`
    /// datastar directive.
    pub action: &'a str,
    /// Label rendered when idle. Plain text — escaped by plait.
    pub label: &'a str,
    /// Label rendered while the request is in flight. "Stopping…",
    /// "Deleting…", "Probing…" — verb in present-continuous.
    pub busy_label: &'a str,
    /// Button-side classes — `btn btn-sm btn-error`, etc. Don't
    /// include `m-0`; the form gets that automatically so it
    /// stacks cleanly next to siblings.
    pub button_class: &'a str,
    /// Signal name backing the data-indicator. Must be unique per
    /// in-flight button on the page (e.g. `"busy_probe_sbx_abc"`).
    /// Allowed chars are JS identifier ones — alnum + underscore.
    pub busy_signal: &'a str,
    /// Optional native `confirm("…")` guard prepended to the
    /// directive. Useful for destructive actions ("Delete this
    /// item?"). Prevents the @post from firing on cancel.
    pub confirm: Option<&'a str>,
    /// Optional `title=` for hover tooltip.
    pub title: Option<&'a str>,
}

/// Render an action button wrapped in a single-button form with
/// loading-state visuals + a data-indicator. See module docs.
pub fn render_busy_post_form(opts: BusyPostForm<'_>) -> Html {
    let post_call = format!("@post('{}', {{contentType: 'form'}})", opts.action);
    let directive = match opts.confirm {
        Some(prompt) => {
            // Single-quote-escape the prompt so it can ride inside
            // a single-quoted JS string literal.
            let safe = prompt.replace('\\', "\\\\").replace('\'', "\\'");
            format!("confirm('{safe}') && {post_call}")
        }
        None => post_call,
    };
    let signal_ref = format!("${}", opts.busy_signal);
    let show_idle = format!("!{signal_ref}");
    let show_busy = signal_ref.clone();
    let disabled_attr = signal_ref.clone();
    let action_owned = opts.action.to_string();
    let label_owned = opts.label.to_string();
    let busy_label_owned = opts.busy_label.to_string();
    let class_owned = opts.button_class.to_string();
    let busy_signal_owned = opts.busy_signal.to_string();
    let title_owned = opts.title.map(str::to_string);
    html! {
        form(
            action: (action_owned),
            method: "post",
            class: "m-0",
            "data-indicator": (busy_signal_owned),
            "data-on:submit__prevent": (directive)
        ) {
            button(
                type: "submit",
                class: (class_owned),
                "data-attr-disabled": (disabled_attr),
                title: (title_owned.clone().unwrap_or_default())
            ) {
                span(
                    "data-show": (show_idle),
                    class: "contents"
                ) { (label_owned) }
                span(
                    "data-show": (show_busy),
                    class: "inline-flex items-center gap-2",
                    style: "display:none"
                ) {
                    (icons::spinner(14))
                    span { (busy_label_owned) }
                }
            }
        }
    }
    .to_html()
}

/// Idle/busy submit-button spec for `render_busy_submit`.
pub struct BusySubmit<'a> {
    pub label: &'a str,
    pub busy_label: &'a str,
    pub button_class: &'a str,
    pub busy_signal: &'a str,
}

/// Render a submit button with the same idle/busy swap as
/// `render_busy_post_form`, intended for inclusion inside a larger
/// form whose `data-indicator` already references `busy_signal`.
pub fn render_busy_submit(opts: BusySubmit<'_>) -> Html {
    let signal_ref = format!("${}", opts.busy_signal);
    let show_idle = format!("!{signal_ref}");
    let show_busy = signal_ref.clone();
    let disabled_attr = signal_ref.clone();
    let label_owned = opts.label.to_string();
    let busy_label_owned = opts.busy_label.to_string();
    let class_owned = opts.button_class.to_string();
    html! {
        button(
            type: "submit",
            class: (class_owned),
            "data-attr-disabled": (disabled_attr)
        ) {
            span(
                "data-show": (show_idle),
                class: "contents"
            ) { (label_owned) }
            span(
                "data-show": (show_busy),
                class: "inline-flex items-center gap-2",
                style: "display:none"
            ) {
                (icons::spinner(14))
                span { (busy_label_owned) }
            }
        }
    }
    .to_html()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer(streaming: bool) -> String {
        render_composer(ComposerOpts {
            post_url: "/chat/s1/messages",
            cancel_url: "/chat/s1/cancel",
            placeholder: "msg",
            has_voice: false,
            streaming,
            toolbar: None,
        })
        .to_string()
    }

    #[test]
    fn composer_arms_stop_when_a_turn_is_in_flight() {
        // Server seeds $chatStreaming=true so the Stop control shows on
        // load/reload — the fix for "I reloaded and there's no stop button".
        assert!(
            composer(true).contains("chatStreaming: true"),
            "an in-flight turn must seed the streaming signal true"
        );
    }

    #[test]
    fn composer_is_idle_by_default() {
        assert!(
            composer(false).contains("chatStreaming: false"),
            "an idle composer must not show the streaming/stop state"
        );
    }

    fn tool_call(id: &str, name: &str, status: ToolCallStatus) -> ToolCall {
        ToolCall {
            id: id.into(),
            turn_id: "t1".into(),
            seq: 0,
            name: name.into(),
            arguments_json: "{}".into(),
            output_json: Some("{}".into()),
            status,
            created_at: jiff::Timestamp::now(),
            completed_at: None,
        }
    }

    #[test]
    fn few_tool_calls_render_flat_without_a_group() {
        let calls: Vec<ToolCall> = (0..TOOL_GROUP_THRESHOLD)
            .map(|i| tool_call(&format!("c{i}"), "rag_search", ToolCallStatus::Completed))
            .collect();
        let html = render_tool_call_list(&calls, "t1").to_string();
        assert!(
            !html.contains("tool-calls-group"),
            "at/below the threshold the rows stay flat: {html}"
        );
        // Each individual row is still present.
        assert_eq!(
            html.matches("tool-call__name").count(),
            TOOL_GROUP_THRESHOLD
        );
    }

    #[test]
    fn many_tool_calls_collapse_into_one_group_with_a_tally() {
        let calls: Vec<ToolCall> = (0..13)
            .map(|i| tool_call(&format!("c{i}"), "rag_search", ToolCallStatus::Completed))
            .collect();
        let html = render_tool_call_list(&calls, "t1").to_string();
        assert!(
            html.contains("tool-calls-group"),
            "expected a group wrapper"
        );
        // Summary tallies them by name so the reader sees the count
        // without unfolding.
        assert!(
            html.contains("13 calls"),
            "summary should show the count: {html}"
        );
        assert!(
            html.contains("rag_search ×13"),
            "summary should tally by name: {html}"
        );
        // The individual rows still live inside (unfold on click).
        assert!(html.contains("tc-c0") && html.contains("tc-c12"));
        // Stable group id so morph preserves the open/close toggle.
        assert!(html.contains("turn-t1-tools-group"));
    }

    #[test]
    fn group_summary_reflects_mixed_names_and_running_state() {
        let mut calls = vec![
            tool_call("a", "rag_search", ToolCallStatus::Completed),
            tool_call("b", "rag_search", ToolCallStatus::Completed),
            tool_call("c", "fetch_url", ToolCallStatus::Completed),
            tool_call("d", "rag_search", ToolCallStatus::Running),
        ];
        calls[3].status = ToolCallStatus::Running;
        let html = render_tool_call_list(&calls, "t9").to_string();
        assert!(html.contains("rag_search ×3"), "tally per name: {html}");
        assert!(html.contains("fetch_url"), "all names listed: {html}");
        assert!(
            html.contains("Running tools"),
            "any running call flips the group label to running: {html}"
        );
    }

    #[test]
    fn truncate_for_display_passes_through_small_payloads() {
        let small = "x".repeat(128);
        assert_eq!(truncate_for_display(small.clone()), small);
    }

    #[test]
    fn truncate_for_display_caps_oversized_payloads_with_footer() {
        let huge = "x".repeat(TOOL_CALL_RENDER_CAP * 4);
        let out = truncate_for_display(huge.clone());
        assert!(
            out.len() < huge.len() / 2,
            "expected significant truncation"
        );
        assert!(
            out.contains("truncated for display"),
            "expected footer note, got: {}",
            &out[out.len().saturating_sub(200)..]
        );
        assert!(
            out.contains(&huge.len().to_string()),
            "footer should report original byte count"
        );
    }

    #[test]
    fn truncate_for_display_doesnt_split_utf8() {
        // Build a payload that crosses the cap with multi-byte
        // chars so a naive byte-slice would corrupt the last char.
        let prefix = "x".repeat(TOOL_CALL_RENDER_CAP - 1);
        let payload = format!("{prefix}\u{1F600}\u{1F600}");
        let out = truncate_for_display(payload);
        // If we sliced mid-codepoint, this would panic.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn render_markdown_highlights_fenced_rust() {
        let md = "```rust\nfn main() { println!(\"hi\"); }\n```";
        let out = render_markdown(md);
        // lumis emits inline-styled spans for highlighted tokens.
        assert!(
            out.contains("<span style="),
            "expected lumis spans in output, got:\n{out}"
        );
        // Multi-theme mode wraps every colour in `light-dark(<day>,
        // <night>)` so the browser can switch theme without a
        // re-render.
        assert!(
            out.contains("light-dark("),
            "expected `light-dark()` styles for theme switching: {out}"
        );
        // The post-pass replaces the markdown wrapper with lumis's
        // own `<pre class="lumis lumis-themes …"><code …>`.
        assert!(
            out.contains(r#"class="lumis lumis-themes"#),
            "missing lumis multi-themes <pre> wrapper: {out}"
        );
        assert!(
            out.contains(">println<"),
            "expected `println` as its own token: {out}"
        );
    }

    #[test]
    fn render_markdown_strips_lumis_pre_inline_style() {
        let md = "```rust\nfn main() {}\n```";
        let out = render_markdown(md);
        let pre_open = out
            .split_once("</pre>")
            .map(|(head, _)| head)
            .unwrap_or(&out);
        let first_pre = pre_open
            .split_once('>')
            .map(|(open, _)| open)
            .unwrap_or(pre_open);
        assert!(
            !first_pre.contains(" style="),
            "lumis <pre> kept its inline style attr (theme bg leaks): {first_pre}"
        );
        assert!(out.contains("<span style="));
    }

    #[test]
    fn render_markdown_passes_through_unknown_language() {
        let md = "```neverheardofit\nblah blah\n```";
        let out = render_markdown(md);
        assert!(!out.contains("<span style="));
        assert!(out.contains("<code class=\"language-neverheardofit\">"));
        assert!(out.contains("blah blah"));
    }

    #[test]
    fn render_markdown_leaves_plain_text_alone() {
        let out = render_markdown("just some **bold** text");
        assert!(out.contains("<strong>bold</strong>"));
        assert!(!out.contains("<pre>"));
    }

    #[test]
    fn render_markdown_never_emits_img_tags() {
        // The model echoes/hallucinates image links pointing at relative
        // or placeholder URLs; rendered as live <img src> they resolve
        // against the /chat/<id> page and 404/429-flood. Every one must
        // degrade to text/link, never a fetched <img>.
        for md in [
            "![preview](preview_url)",
            "![](image_url)",
            "![letter](5c858cd7-12b3-439e-9b31-c2cef4b65116/letter.png)",
            "see ![the chart](./png_url) here",
        ] {
            let out = render_markdown(md);
            assert!(
                !out.contains("<img"),
                "markdown image leaked a live <img> for {md:?}: {out}"
            );
        }
        // Real attachments don't come through markdown — they're spliced
        // as [gw-attachment …] markers and rendered by render_attachment,
        // so disabling the construct can't break a legitimate image.
        assert!(render_markdown("plain **text**").contains("<strong>text</strong>"));
    }

    #[test]
    fn render_markdown_normalises_lang_aliases() {
        let md = "```py\nprint('hi')\n```";
        let out = render_markdown(md);
        assert!(
            out.contains("<span style="),
            "py alias should have routed to python: {out}"
        );
    }

    #[test]
    fn assistant_segments_fast_path_returns_one_prose_block() {
        let segs = assistant_segments("plain text with **bold**");
        assert_eq!(segs.len(), 1);
        assert!(
            matches!(&segs[0], AssistantSegment::Prose(s) if s.contains("<strong>bold</strong>"))
        );
    }

    #[test]
    fn assistant_segments_splices_uploaded_attachment() {
        let marker = crate::attachments::marker_line(
            "chart.png",
            "image/png",
            "https://example.invalid/x.png",
            42,
        );
        let body = format!(
            "Here is the chart you asked for:\n\n{marker}\n\nLet me know if you want adjustments."
        );
        let segs = assistant_segments(&body);
        // Three segments: prose, attachment, prose. Each prose chunk
        // gets its own markdown pass so links/bold/etc. still work
        // around the attachment.
        assert_eq!(segs.len(), 3);
        assert!(matches!(&segs[0], AssistantSegment::Prose(s) if s.contains("Here is the chart")));
        match &segs[1] {
            AssistantSegment::Attachment(a) => {
                assert_eq!(a.filename, "chart.png");
                assert!(a.is_image());
            }
            _ => panic!("expected attachment in middle slot"),
        }
        assert!(matches!(&segs[2], AssistantSegment::Prose(s) if s.contains("adjustments")));
    }

    #[test]
    fn attachment_turn_id_parses_gateway_urls_only() {
        assert_eq!(
            attachment_turn_id("/chat/attachment/abc-123/letter.pdf"),
            Some("abc-123")
        );
        assert_eq!(attachment_turn_id("https://example.invalid/x.png"), None);
        assert_eq!(attachment_turn_id("/chat/attachment//letter.pdf"), None);
    }

    #[test]
    fn render_attachment_degrades_when_marker_turn_orphaned() {
        let pdf = crate::attachments::ParsedAttachment {
            filename: "letter.pdf".into(),
            mime: "application/pdf".into(),
            url: "/chat/attachment/turn-A/letter.pdf".into(),
            size: 19600,
            link: None,
        };
        // Owner matches → normal chip with a working download link.
        let ok = render_attachment(&pdf, "turn-A").to_string();
        assert!(
            ok.contains("/chat/attachment/turn-A/letter.pdf"),
            "expected the real download link: {ok}"
        );
        assert!(
            !ok.contains("unavailable"),
            "should not be a placeholder: {ok}"
        );
        // Owner differs (orphaned marker) → muted placeholder, no dead link.
        let orphan = render_attachment(&pdf, "turn-B").to_string();
        assert!(
            orphan.contains("unavailable"),
            "expected the unavailable placeholder: {orphan}"
        );
        assert!(
            !orphan.contains("href"),
            "an orphaned attachment must not render a dead link: {orphan}"
        );
        assert!(
            orphan.contains("letter.pdf"),
            "filename should still be shown: {orphan}"
        );
        // An orphaned image must not emit a broken <img>.
        let png = crate::attachments::ParsedAttachment {
            filename: "preview.png".into(),
            mime: "image/png".into(),
            url: "/chat/attachment/turn-A/preview.png".into(),
            size: 1000,
            link: None,
        };
        let orphan_img = render_attachment(&png, "turn-B").to_string();
        assert!(
            !orphan_img.contains("<img"),
            "an orphaned image must not emit a broken <img>: {orphan_img}"
        );
    }

    #[test]
    fn html_unescape_decodes_markdown_entity_set() {
        assert_eq!(
            html_unescape("if x &lt; 5 &amp;&amp; y &gt; 0"),
            "if x < 5 && y > 0"
        );
        assert_eq!(html_unescape("&quot;hello&quot;"), "\"hello\"");
        assert_eq!(html_unescape("don&#39;t"), "don't");
    }
}
