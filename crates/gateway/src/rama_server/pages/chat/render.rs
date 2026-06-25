// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Gateway-specific chat-page chrome.
//!
//! The transcript bubbles, composer, and markdown/highlighting all
//! live in `session_core::render` — both this bin and the upcoming
//! orchestrator paint the same shapes from there. What stays here is
//! the gateway-only wrapper: the header strip with the model + voice
//! pickers, the optional error alert, and the `ChatPage` parameter
//! struct that the chat-page handler assembles for one render.

use plait::{Html, ToHtml, html};
use session_core::db::{Session, TurnWithTools};
use session_core::icons;
use session_core::render;

/// One selectable chat model + its data-handling flags. `gdpr`/`nda` are
/// `true` (clear) for the common case; a `false` drives the dropdown-label
/// suffix and the per-conversation warning banner. Built from
/// `UpstreamRegistry::models_with_compliance_for_kind`.
pub(super) struct ChatModelOption {
    pub id: String,
    pub gdpr: bool,
    pub nda: bool,
}

/// One toggleable capability in the composer's "+" menu — a built-in tool
/// group, a connected MCP integration, or an operator-installed skill the
/// caller's roles permit. Built fresh per request from RBAC + the
/// per-conversation overlay.
pub(super) struct CapabilityRow {
    /// The overlay key: the catalog toggle key (`web`, `typst`, `mcp__gitlab`,
    /// … — one key governs a whole group) for `CapKind::Tool`, or the skill
    /// name for `CapKind::Skill`.
    pub key: String,
    pub kind: CapKind,
    /// Human-readable label shown in the menu and on the active chip.
    pub label: String,
    /// Plain-language "what this does / when it helps", shown under the label.
    pub description: String,
    /// Section heading the row sorts under — a `Category::label()` for tools
    /// ("Web & Network", …) or "Skills".
    pub group: &'static str,
    /// Sort/group order. Built-in categories use `Category::order()`; skills
    /// trail everything via [`SKILL_ORDER`].
    pub order: u8,
    /// Tri-state for this conversation: On (force-exposed), Auto (model may
    /// enable), Off (blocked). Skills only ever use On / Auto.
    pub state: ToolState,
    /// Connector icon hint (the catalog `icon`, usually an emoji) for
    /// integrations without a built-in brand logo. `None` otherwise.
    pub icon: Option<String>,
}

impl CapabilityRow {
    /// Whether this capability is force-enabled (shows as an active chip).
    pub fn is_on(&self) -> bool {
        self.state == ToolState::On
    }
}

/// Skills sort after every built-in category (`Category::order()` tops out at
/// 6) so the "Skills" group is always last in the menu.
pub(super) const SKILL_ORDER: u8 = 250;

/// Shared horizontal padding for every menu row (master, category heading,
/// tool row). Keeping it identical — combined with one scroll box that
/// reserves a stable scrollbar gutter — is what lines the trailing segmented
/// pills up at the exact same distance from the panel border.
const ROW_PAD_X: &str = "0.5rem";

/// The per-conversation enablement tier of one capability. Built-in tools use
/// all three; skills use only `On` (loaded) / `Auto` (not loaded).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ToolState {
    /// Force-exposed to the model every turn (`chat_session_tools.enabled=1`).
    On,
    /// Lazy — no row; the model may turn it on via `enable_tools`.
    Auto,
    /// Blocked — `chat_session_tools.enabled=0`; hidden and un-enableable.
    Off,
}

impl ToolState {
    /// Wire value posted by a segment button and parsed by the handler.
    pub fn as_str(self) -> &'static str {
        match self {
            ToolState::On => "on",
            ToolState::Auto => "auto",
            ToolState::Off => "off",
        }
    }

    /// Map a `chat_session_tools` row (present → its `enabled` flag; absent →
    /// `None`) to a tier.
    pub fn from_row(row: Option<bool>) -> Self {
        match row {
            Some(true) => ToolState::On,
            Some(false) => ToolState::Off,
            None => ToolState::Auto,
        }
    }
}

/// Which overlay a capability toggle writes to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CapKind {
    /// `chat_session_tools` (built-in tool groups + MCP connectors).
    Tool,
    /// `chat_session_skills` (operator-installed skills).
    Skill,
}

impl CapKind {
    /// Wire value posted by the toggle form and parsed by the handler.
    pub fn as_str(self) -> &'static str {
        match self {
            CapKind::Tool => "tool",
            CapKind::Skill => "skill",
        }
    }
}

/// Inputs the chat-page handler passes through to one render call.
/// Owns no state — the handler builds it fresh per request from
/// `RamaState` + DB.
pub(super) struct ChatPage<'a> {
    pub active: &'a Session,
    pub turns: &'a [TurnWithTools],
    /// Most-recent assistant turn, *if it's still streaming*. The
    /// renderer marks the bubble in-progress and the conversation
    /// section emits the auto-tail `data-init`. None when there's
    /// no live worker for this session.
    pub in_flight_turn_id: Option<&'a str>,
    pub models: &'a [ChatModelOption],
    pub transcription_models: &'a [String],
    pub error_msg: Option<&'a str>,
    /// Viewer is not the owner (the session is shared): render read-only —
    /// no composer, no pickers, a "shared, read-only" banner instead.
    pub read_only: bool,
    /// Current share state, for the owner's share toggle label.
    pub shared: bool,
    /// The conversation's effort level ("Denkaufwand"), for the header picker.
    pub effort: crate::server::reasoning::Effort,
    /// Toggleable capabilities for the "+" menu (owner only; empty for a
    /// read-only viewer).
    pub capabilities: &'a [CapabilityRow],
    /// Pre-rendered document-canvas panel (the active document for this
    /// session), or `None` when the conversation has no documents yet. The
    /// always-present `#document-canvas-slot` wraps it so a later
    /// `create_document` has a live morph target even on a doc-less load.
    pub document_canvas_html: Option<&'a str>,
}

/// Chat page body — header (model pickers) + conversation +
/// composer. The conversation list lives in the global sidebar
/// (see `render_app_sidebar` in pages/mod.rs); we don't render
/// it here.
pub(super) fn render_chat_page(page: ChatPage<'_>) -> Html {
    let models_empty = page.models.is_empty();
    // (value, label) per option — label carries the compliance suffix, value
    // stays the raw id so the form still posts the real model name.
    let model_options: Vec<(String, String)> = page
        .models
        .iter()
        .map(|m| (m.id.clone(), model_label(m)))
        .collect();
    // Compliance UI is owner-only (a read-only shared viewer sends nothing)
    // and only meaningful when there's a real model picker. The signal store
    // is emitted whenever the picker is shown (so the picker's `data-on:change`
    // always writes a declared signal); each banner is emitted only when some
    // model actually trips that flag, so an all-clear deployment carries no
    // banner markup at all.
    let show_compliance = !page.read_only && !models_empty;
    let any_gdpr_flagged = page.models.iter().any(|m| !m.gdpr);
    let any_nda_flagged = page.models.iter().any(|m| !m.nda);
    let compliance_signals = compliance_signals(page.models);
    let voice_models: Vec<String> = page.transcription_models.to_vec();
    let has_voice = !voice_models.is_empty();
    let session_id = page.active.id.clone();
    let turns_owned: Vec<TurnWithTools> = page.turns.to_vec();
    let in_flight_tail_url = page
        .in_flight_turn_id
        .map(|_| format!("/chat/{session_id}/tail"));
    let error_owned = page.error_msg.map(|s| s.to_string());
    let document_canvas_html = page.document_canvas_html.map(|s| s.to_string());
    // Canvas docks as a right-hand column. `hasCanvas` reveals the header
    // toggle once a document exists (set live on the first create); `canvasOpen`
    // shows/hides the column. `canvasOpen` seeds false and a `data-init` opens
    // it on mount only on a wide viewport — so a doc-bearing chat opens docked
    // on desktop but never auto-covers the chat on mobile (button-only there).
    let has_canvas = document_canvas_html.is_some();
    let canvas_signals = format!("{{\"hasCanvas\": {has_canvas}, \"canvasOpen\": false}}");

    let post_url = format!("/chat/{session_id}/messages");
    let cancel_url = format!("/chat/{session_id}/cancel");
    let share_url = format!("/chat/{session_id}/share");
    let read_only = page.read_only;

    html! {
      // Two-column shell: the chat (header/conversation/composer) on the left,
      // the document canvas docked on the right. A draggable splitter sits
      // between them (desktop); on mobile the canvas is a button-toggled
      // overlay. The signal store drives both the header toggle and the column.
      div(
          class: "chat-shell",
          "data-signals": (canvas_signals),
          // Open docked on mount, desktop only. Live edits re-open via the
          // `gwcanvasopen` window event (also desktop-gated, in the tool).
          "data-init": "$canvasOpen = $hasCanvas && window.innerWidth >= 768",
          "data-on:gwcanvasopen__window": "$canvasOpen = true",
          // Full width ONLY while the canvas is docked; otherwise the chat
          // falls back to the centered reading column (see main.css).
          "data-class": "{'canvas-open': $canvasOpen}"
      ) {
        div(class: "chat-col") {
        // Header row: title (left) + model/voice pickers and the share toggle
        // (right) — all ONE row. The header is always rendered so the share
        // toggle is reachable on mobile; on phones the title + pickers hide
        // (the sidebar + floating drawer button cover them), leaving just the
        // share control on the right.
        div(class: "chat-header flex") {
            div(class: "chat-header__title hidden sm:block") {
                h1(class: "text-lg font-semibold truncate") {
                    (session_label(page.active))
                }
            }
            // Right group: model/voice pickers (desktop-only — the `<select>`
            // chips chew ~70px of vertical space and most mobile sessions keep
            // the default model) + the always-visible share toggle. Owner-only
            // — a read-only viewer changes nothing here.
            // `ml-auto`: with the title hidden on mobile this stays pinned
            // right (away from the top-left floating drawer button) instead of
            // collapsing left under `justify-content: space-between`.
            div(class: "flex items-center gap-2 ml-auto") {
            // Export is available to anyone who can read the chat (owner or a
            // shared viewer), so it lives outside the owner-only block below.
            (render_export_control(&session_id))
            // Document-canvas toggle. Hidden until a document exists (the live
            // create flips `$hasCanvas`); click shows/hides the docked panel.
            button(
                type: "button",
                class: "btn btn-ghost btn-sm gap-1",
                title: "Show / hide the document canvas",
                "data-show": "$hasCanvas",
                "data-on:click": "$canvasOpen = !$canvasOpen"
            ) {
                (icons::pencil(16))
                span(class: "hidden sm:inline") { "Document" }
            }
            // A read-only viewer (recipient of a shared chat) gets the inverse
            // of the owner controls: a "fork into my chats" button instead of
            // the model pickers + share toggle.
            if read_only {
                (render_fork_control(&session_id))
            }
            if !read_only {
            div(class: "chat-header__pickers hidden sm:flex") {
                if models_empty {
                    input(
                        id: "model",
                        name: "model",
                        form: "chat-form",
                        type: "text",
                        required: "required",
                        placeholder: "model (e.g. gpt-4o-mini)",
                        class: "input input-bordered input-sm w-56"
                    );
                } else {
                    div(class: "flex items-center gap-1.5 text-sm") {
                        (icons::sliders(14))
                        select(
                            id: "model",
                            name: "model",
                            form: "chat-form",
                            "aria-label": "Chat model",
                            // Track the picked model so the compliance banner
                            // below reacts when the user switches models.
                            "data-on:change": "$selectedModel = evt.target.value",
                            class: "select select-bordered select-sm chat-model-select"
                        ) {
                            for (value, label) in model_options.iter() {
                                option(value: (value.clone())) { (label.clone()) }
                            }
                        }
                    }
                }
                if has_voice {
                    div(class: "flex items-center gap-1.5 text-sm") {
                        (icons::mic(14))
                        select(
                            id: "voice-model",
                            "data-mic-model": "1",
                            "aria-label": "Voice model",
                            class: "select select-bordered select-sm chat-model-select"
                        ) {
                            for m in voice_models.iter() {
                                option(value: (m.clone())) { (m.clone()) }
                            }
                        }
                    }
                }
            }
            (render_share_control(&share_url, page.shared))
            }
            }
        }

        // Per-conversation compliance banners. Rendered up front (before any
        // prompt) and reactive: `data-show` keys off `$selectedModel`, which
        // the model picker updates on change, so the right warning appears the
        // moment a flagged model is selected. The signal store is emitted
        // whenever the picker is shown (so the picker's `data-on:change` always
        // writes a declared signal); each banner is emitted only when some
        // model trips that flag — an all-clear deployment carries no banner.
        if show_compliance {
            // Carries the signal store, and on mount syncs `selectedModel`
            // from the picker's *actual* value. The seed in `data-signals` is
            // only the server's guess (first option); a browser-restored
            // selection (new conversation / reload) or a flagged default can
            // differ without ever firing a `change`, which is exactly when the
            // banner would otherwise stay hidden. Reading the live DOM value at
            // `data-init` covers that; `change` keeps it live afterwards.
            div(
                "data-signals": (compliance_signals),
                "data-init": "$selectedModel = document.getElementById('model')?.value ?? $selectedModel",
                style: "display:none"
            ) {}
            if any_gdpr_flagged {
                div(
                    class: "alert alert-error mb-2",
                    role: "alert",
                    "data-show": "$gdprFlagged.includes($selectedModel)",
                    style: "display:none"
                ) {
                    (icons::alert(20))
                    span {
                        "You are sending data to a non-GDPR-compliant model. \
                         Do not enter personal information (names, emails, \
                         addresses, customer or employee data)."
                    }
                }
            }
            if any_nda_flagged {
                div(
                    class: "alert alert-error mb-2",
                    role: "alert",
                    "data-show": "$ndaFlagged.includes($selectedModel)",
                    style: "display:none"
                ) {
                    (icons::alert(20))
                    span {
                        "This model is not covered by a confidentiality agreement. \
                         Do not send NDA-protected or proprietary material."
                    }
                }
            }
        }

        if let Some(msg) = error_owned.as_ref() {
            div(class: "alert alert-error mb-4") {
                (icons::alert(20))
                span { (msg.clone()) }
            }
        }

        (render::render_conversation(&turns_owned, in_flight_tail_url.as_deref(), Some("/chat")))
        // Owner gets the composer; a read-only viewer of a shared chat gets a
        // banner instead (mutations are owner-only on the server regardless).
        if read_only {
            div(class: "alert mt-4") {
                (icons::alert(20))
                span { "Shared chat — read-only. Only the creator can reply." }
            }
        } else {
            (render::render_composer(render::ComposerOpts {
                post_url: &post_url,
                cancel_url: &cancel_url,
                placeholder: "Message the model…",
                has_voice,
                // A turn already in flight seeds the Stop control server-side,
                // so a reload mid-stream still offers a way to stop it.
                streaming: page.in_flight_turn_id.is_some(),
                // The "+" tools/integrations/skills menu AND the "Denken"
                // (effort) picker live inside the composer (above the input),
                // so both ride with the sticky composer and sit where the user
                // is typing — not stranded in the page header.
                toolbar: Some(render_composer_toolbar(
                    &session_id,
                    page.capabilities,
                    page.effort,
                )),
            }))
        }
        } // .chat-col
        // Draggable splitter between chat and canvas. Desktop only (CSS hides
        // it on narrow screens, where the canvas is a full overlay). The drag
        // handler in app.js resizes the canvas column and remembers the width.
        div(
            id: "canvas-splitter",
            class: "canvas-splitter",
            "data-show": "$canvasOpen",
            "aria-hidden": "true"
        ) {}
        // Right-docked canvas column. Always in the DOM (even empty) so it is a
        // stable morph target for the first `create_document`; shown when the
        // panel is open.
        aside(
            id: "document-canvas-slot",
            class: "canvas-col",
            "data-show": "$canvasOpen"
        ) {
            if let Some(html) = document_canvas_html.as_ref() {
                #(html.clone())
            }
        }
      } // .chat-shell
    }
    .to_html()
}

/// Dropdown label for one model: the raw id, plus a parenthetical suffix
/// naming each restriction so the user sees it before opening any banner.
fn model_label(m: &ChatModelOption) -> String {
    match (m.gdpr, m.nda) {
        (true, true) => m.id.clone(),
        (false, true) => format!("{} (non-GDPR)", m.id),
        (true, false) => format!("{} (confidential-restricted)", m.id),
        (false, false) => format!("{} (non-GDPR, confidential-restricted)", m.id),
    }
}

/// Builds the datastar signal store the compliance banners read:
///   - `selectedModel` seeded to the default (first) option, so the banner is
///     correct on load before any `change` event;
///   - `gdprFlagged` / `ndaFlagged`: the model ids that trip each warning.
///
/// A JS object literal with JSON arrays (valid datastar `data-signals`).
fn compliance_signals(models: &[ChatModelOption]) -> String {
    let gdpr_flagged: Vec<&str> = models
        .iter()
        .filter(|m| !m.gdpr)
        .map(|m| m.id.as_str())
        .collect();
    let nda_flagged: Vec<&str> = models
        .iter()
        .filter(|m| !m.nda)
        .map(|m| m.id.as_str())
        .collect();
    let default_model = models.first().map(|m| m.id.as_str()).unwrap_or("");
    format!(
        "{{selectedModel: {}, gdprFlagged: {}, ndaFlagged: {}}}",
        serde_json::to_string(default_model).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(&gdpr_flagged).unwrap_or_else(|_| "[]".into()),
        serde_json::to_string(&nda_flagged).unwrap_or_else(|_| "[]".into()),
    )
}

fn session_label(session: &Session) -> String {
    session
        .title
        .clone()
        .unwrap_or_else(|| "New conversation".to_string())
}

/// The conversation's effort ("Denkaufwand") picker: a one-field form whose
/// `<select>` auto-posts on change. One knob drives both the upstream
/// reasoning budget and the tool-round cap (see `server::reasoning`).
/// The composer toolbar row: the "+" capability menu and the "Denken"
/// (effort) picker, side by side right above the input — so both controls sit
/// where the user is typing instead of being stranded in the page header.
fn render_composer_toolbar(
    session_id: &str,
    caps: &[CapabilityRow],
    effort: crate::server::reasoning::Effort,
) -> Html {
    html! {
        // Full width so the effort picker can sit hard-right: "+ Tools" (+ active
        // chips) stay left, the thinking picker floats right. Horizontal padding
        // matches the textarea's text inset (1.125rem) so the pills clear the
        // composer's rounded corners and line up with the typed text.
        div(style: "display:flex; flex-wrap:wrap; align-items:center; gap:0.5rem; \
                    width:100%; padding:0.5rem 1.125rem 0") {
            (render_capabilities(session_id, caps))
            div(style: "margin-left:auto") {
                (render_effort_select(session_id, effort))
            }
        }
    }
    .to_html()
}

/// The "Denken" (effort / thinking) picker. A labelled `<select>` — NOT wrapped
/// in a `<form>` (it lives inside the composer's form; nested forms are
/// invalid), so it posts the chosen level in the query string on change. The
/// visible "Denken:" label + sparkle make it findable next to the "+" button.
fn render_effort_select(session_id: &str, effort: crate::server::reasoning::Effort) -> Html {
    use crate::server::reasoning::Effort;
    let action = format!("/chat/{session_id}/effort");
    let levels = [Effort::Fast, Effort::Standard, Effort::Deep, Effort::Max];
    let opts: Vec<Html> = levels
        .iter()
        .map(|e| {
            if *e == effort {
                html! { option(value: (e.as_str()), selected: "selected") { (e.label()) } }
                    .to_html()
            } else {
                html! { option(value: (e.as_str())) { (e.label()) } }.to_html()
            }
        })
        .collect();
    html! {
        span(
            class: "text-sm",
            style: "display:inline-flex; align-items:center; gap:0.35rem",
            title: "Thinking effort: higher = more reasoning and more tool rounds, but slower"
        ) {
            (icons::sparkles(16))
            span(class: "opacity-70 hidden sm:inline") { "Thinking:" }
            // `name` deliberately omitted so the surrounding composer form
            // doesn't serialise this select on send; the value rides in the
            // `@post` query instead.
            select(
                "aria-label": "Thinking effort",
                "data-on:change": (format!("@post('{action}?effort=' + evt.target.value)")),
                class: "select select-sm",
                // `padding-right` clears daisyUI's dropdown arrow (~20px in)
                // so the label can't run under it; pill radius matches the
                // rounded "+ Tools" button.
                style: "border:1px solid color-mix(in oklch, currentColor 15%, transparent); \
                        border-radius:9999px; padding-right:2rem; min-width:6rem"
            ) {
                for o in opts.iter() {
                    (o.clone())
                }
            }
        }
    }
    .to_html()
}

/// The composer's "+" menu + active-capability chips. Rendered *inside* the
/// composer (via `ComposerOpts::toolbar`) so it sits with the input rather than
/// floating below the sticky composer. The toggles are plain buttons that
/// `@post` to `/chat/{id}/capabilities?kind=…&key=…` (the composer is itself a
/// `<form>`, so nested forms would be invalid — the key rides in the query
/// string instead). The whole block lives under `#capabilities`, which the
/// toggle handler re-patches; the `capMenu` open-state signal sits on the outer
/// `#cap-wrap` so it survives a patch (the menu stays open while pinning
/// several things). The popup opens upward (`bottom:100%`) via inline style —
/// daisyUI's `dropdown-top` / Tailwind's `bottom-full` aren't in the purged
/// build, so positioning is hand-rolled with styles that always ship.
pub(super) fn render_capabilities(session_id: &str, caps: &[CapabilityRow]) -> Html {
    // `#cap-wrap` carries the menu's signals and is never re-patched, so they
    // survive the targeted patches the toggle handler emits:
    //   * `capMenu`  — popup open/closed
    //   * `capQuery` — the search filter
    //   * `openCats` — which category sections are expanded (a map of
    //                  group-slug → bool; absent ⇒ collapsed)
    // On a toggle the handler patches only the small `#cap-seg-*` controls,
    // the `#cap-grp-*` / `#cap-all` aggregate pills, and `#cap-chips` — never
    // the section containers — so collapse + search state can't be clobbered
    // by the morph (datastar's `data-show` doesn't re-evaluate on a morph, so
    // we keep the signal-bearing structure off the patch path entirely).
    let base = format!("/chat/{session_id}/capabilities");
    html! {
        div(
            id: "cap-wrap",
            "data-signals": "{capMenu: false, capQuery: '', openCats: {}}",
            // Click anywhere outside this wrapper (button + popup) closes the
            // menu. Scoped to `#cap-wrap` so clicking the toggle button itself
            // — which lives inside — isn't treated as an outside click.
            "data-on:click__outside": "$capMenu = false"
        ) {
            (render_capabilities_inner(&base, caps))
        }
    }
    .to_html()
}

/// The `#capabilities` subtree: the "+" button, the popup tree, and the active
/// chips. Rendered once at page load and left in place — the handler patches
/// only its volatile leaves.
fn render_capabilities_inner(base: &str, caps: &[CapabilityRow]) -> Html {
    let has_caps = !caps.is_empty();
    // Full catalog ⇒ a search box is always worthwhile.
    let show_search = caps.len() > 6;
    // Opaque card, opening upward above the sticky composer (z above its 20).
    // Seeded closed; `data-show:$capMenu` governs it thereafter (never
    // re-patched, so the binding keeps working).
    let panel_style = "display:none; position:absolute; left:0; bottom:100%; \
         margin-bottom:8px; width:27rem; z-index:30; overflow:hidden; padding:0.25rem";
    html! {
        div(
            id: "capabilities",
            style: "display:flex; flex-wrap:wrap; align-items:center; gap:0.4rem"
        ) {
            div(style: "position:relative") {
                button(
                    type: "button",
                    "data-on:click": "$capMenu = !$capMenu",
                    class: "btn btn-ghost btn-sm gap-1",
                    style: "border-radius:9999px",
                    title: "Tools, integrations & skills for this conversation"
                ) {
                    (icons::plus(16))
                    span { "Tools" }
                }
                if has_caps {
                    div(
                        class: "rounded-box border border-base-300 bg-base-100 shadow",
                        "data-show": "$capMenu",
                        style: (panel_style)
                    ) {
                        if show_search {
                            input(
                                type: "text",
                                placeholder: "Search tools…",
                                "data-on:input": "$capQuery = evt.target.value.toLowerCase()",
                                class: "input input-sm",
                                style: "width:100%; margin-bottom:0.25rem; \
                                        border:1px solid color-mix(in oklch, currentColor 15%, transparent)"
                            );
                        }
                        // One scroll box holds the master row *and* the
                        // groups so they share an identical right inset:
                        // `scrollbar-gutter:stable` reserves the scrollbar
                        // gutter whether or not it shows, and every row uses
                        // the same `ROW_PAD_X` padding, so all the segmented
                        // pills line up at the exact same distance from the
                        // border. The master row sticks to the top so it stays
                        // visible while the list scrolls.
                        div(style: "max-height:52vh; overflow-y:auto; scrollbar-gutter:stable") {
                            div(
                                "data-show": "$capQuery === ''",
                                class: "bg-base-100",
                                style: (format!(
                                    "position:sticky; top:0; z-index:1; display:flex; \
                                     align-items:center; gap:0.5rem; padding:0.35rem {ROW_PAD_X}; \
                                     margin-bottom:0.15rem; \
                                     border-bottom:1px solid color-mix(in oklch, currentColor 10%, transparent)"
                                ))
                            ) {
                                span(style: "flex:1; font-size:0.78rem; font-weight:700; \
                                             text-transform:uppercase; letter-spacing:0.05em; opacity:0.7") {
                                    "All tools"
                                }
                                (render_master_pill(base, caps))
                            }
                            (render_cap_groups(base, caps))
                        }
                    }
                } else {
                    div(
                        class: "rounded-box border border-base-300 bg-base-100 shadow text-sm",
                        "data-show": "$capMenu",
                        style: (format!("{panel_style}; padding:0.75rem"))
                    ) {
                        "No tools are available to your account yet. Connect an integration under "
                        a(href: "/integrations", style: "text-decoration:underline") { "Integrations" }
                        "."
                    }
                }
            }
            (render_cap_chips(base, caps))
        }
    }
    .to_html()
}

/// The `(selector, html)` patches the toggle handler emits after a change:
/// every per-row segmented control, every group-aggregate pill, the master
/// pill, and the chips. Deliberately *only* the volatile leaves — never the
/// section containers, the search box, or `#cap-wrap` — so the morph can't
/// reset the open/collapse or search signals. Patching an unchanged control is
/// a harmless no-op morph, so the same set covers single-row, group, and "All"
/// toggles uniformly.
pub(super) fn render_capability_patches(
    session_id: &str,
    caps: &[CapabilityRow],
) -> Vec<(String, String)> {
    let base = format!("/chat/{session_id}/capabilities");
    let mut patches: Vec<(String, String)> = Vec::new();
    // Contiguous group runs (caps arrive pre-sorted by order).
    let mut groups: Vec<(&'static str, Vec<&CapabilityRow>)> = Vec::new();
    for c in caps {
        match groups.last_mut() {
            Some((g, rows)) if *g == c.group => rows.push(c),
            _ => groups.push((c.group, vec![c])),
        }
    }
    for (group, rows) in groups.iter() {
        patches.push((
            format!("#cap-grp-{}", group_slug(group)),
            render_group_pill(&base, group, rows).to_string(),
        ));
        for c in rows.iter() {
            patches.push((
                format!("#{}", cap_seg_id(c)),
                render_cap_segment(&base, c).to_string(),
            ));
        }
    }
    patches.push((
        "#cap-all".to_string(),
        render_master_pill(&base, caps).to_string(),
    ));
    patches.push((
        "#cap-chips".to_string(),
        render_cap_chips(&base, caps).to_string(),
    ));
    patches
}

/// The grouped section tree: one collapsible `<section>` per group, in
/// [`CapabilityRow::order`] order. Each heading toggles `$openCats[slug]`; the
/// body shows when expanded *or* while a search is active (so matches surface
/// regardless of collapse). Section containers are never re-patched, so their
/// `data-show` bindings stay live.
fn render_cap_groups(base: &str, caps: &[CapabilityRow]) -> Html {
    // Stable, order-respecting grouping (caps arrive pre-sorted by order).
    let mut groups: Vec<(&'static str, Vec<&CapabilityRow>)> = Vec::new();
    for c in caps {
        match groups.last_mut() {
            Some((g, rows)) if *g == c.group => rows.push(c),
            _ => groups.push((c.group, vec![c])),
        }
    }
    html! {
        for (group, rows) in groups.iter() {
            (render_cap_section(base, group, rows))
        }
    }
    .to_html()
}

/// One collapsible category section: a clickable heading (chevron + label +
/// count) with the group-aggregate pill beside it, over a body of rows.
fn render_cap_section(base: &str, group: &'static str, rows: &[&CapabilityRow]) -> Html {
    let slug = group_slug(group);
    let open_expr = format!("$openCats['{slug}']");
    // Body visible when this section is expanded, or whenever a search is
    // active (then headings hide and matching rows filter themselves in).
    let body_show = format!("{open_expr} || $capQuery !== ''");
    let count = rows.len();
    let count_label = if count == 1 {
        "1 tool".to_string()
    } else {
        format!("{count} tools")
    };
    html! {
        section(style: "margin-bottom:0.1rem") {
            // Heading row: a collapse button on the left, the aggregate pill on
            // the right (a sibling, so its clicks don't trip the collapse).
            div(
                "data-show": "$capQuery === ''",
                style: (format!("display:flex; align-items:center; gap:0.4rem; padding:0.1rem {ROW_PAD_X} 0.1rem 0.2rem"))
            ) {
                button(
                    type: "button",
                    "data-on:click": (format!("{open_expr} = !{open_expr}")),
                    class: "flex-1 flex items-center gap-1.5 px-2 py-1.5 rounded-lg hover:bg-base-200",
                    style: "text-align:left; min-width:0"
                ) {
                    span(
                        style: "display:inline-flex; transition:transform .12s; transform:rotate(-90deg)",
                        // Points right when collapsed, rotates down when open.
                        "data-attr-style": (format!(
                            "'display:inline-flex; transition:transform .12s; transform:rotate(' + ({open_expr} ? '0' : '-90') + 'deg)'"
                        ))
                    ) { (icons::chevron_down(14)) }
                    span(style: "flex:1; font-size:0.7rem; font-weight:700; letter-spacing:0.05em; \
                                 text-transform:uppercase; opacity:0.7; min-width:0") {
                        (group.to_string())
                    }
                    span(style: "font-size:0.65rem; opacity:0.45") { (count_label) }
                }
                (render_group_pill(base, group, rows))
            }
            div("data-show": (body_show)) {
                for c in rows.iter() {
                    (render_cap_row(base, c))
                }
            }
        }
    }
    .to_html()
}

/// One capability row: a header line (icon + label + the tri-state segmented
/// control on the right), with the plain-language description as a full-width
/// block *below* — so it flows under the control too and uses fewer lines,
/// instead of being boxed into a narrow left column. The row wrapper carries
/// the search-filter `data-show` and is never re-patched, so the filter
/// survives a toggle; only the inner `#cap-seg-*` control is patched.
fn render_cap_row(base: &str, c: &CapabilityRow) -> Html {
    let label_lower =
        serde_json::to_string(&c.label.to_lowercase()).unwrap_or_else(|_| "\"\"".to_string());
    let desc_lower =
        serde_json::to_string(&c.description.to_lowercase()).unwrap_or_else(|_| "\"\"".to_string());
    // Match the label or the description, so searching "domain" finds DNS/WHOIS.
    let show = format!(
        "$capQuery === '' || {label_lower}.includes($capQuery) || {desc_lower}.includes($capQuery)"
    );
    let has_desc = !c.description.is_empty();
    html! {
        div(
            id: (cap_row_id(c)),
            "data-show": (show),
            style: (format!("padding:0.4rem {ROW_PAD_X}; border-radius:0.5rem"))
        ) {
            div(style: "display:flex; align-items:center; gap:0.5rem") {
                span(style: "display:inline-flex; width:1.1rem; justify-content:center; opacity:0.85; flex:none") {
                    (cap_icon(c))
                }
                span(style: "flex:1; min-width:0; font-size:0.82rem; font-weight:500") {
                    (c.label.clone())
                }
                (render_cap_segment(base, c))
            }
            if has_desc {
                // Full-width: spans under the icon *and* the control.
                div(style: "font-size:0.72rem; opacity:0.6; margin-top:0.15rem; line-height:1.35") {
                    (c.description.clone())
                }
            }
        }
    }
    .to_html()
}

/// Active-capability chips below the "+" button: the force-**On** tools and
/// loaded skills. Clicking a chip returns it to Auto (un-pin). `#cap-chips` is
/// patched whenever the On set changes.
fn render_cap_chips(base: &str, caps: &[CapabilityRow]) -> Html {
    html! {
        div(id: "cap-chips", style: "display:contents") {
            for c in caps.iter().filter(|c| c.is_on()) {
                button(
                    type: "button",
                    // Un-pin → Auto (model may still enable on demand).
                    "data-on:click": (format!(
                        "@post('{base}?kind={}&key={}&state=auto')", c.kind.as_str(), c.key
                    )),
                    class: "badge badge-outline gap-1",
                    style: "cursor:pointer",
                    title: "Unpin (back to automatic)"
                ) {
                    span(style: "display:inline-flex") { (cap_icon(c)) }
                    span { (c.label.clone()) }
                    span(class: "opacity-60") { "×" }
                }
            }
        }
    }
    .to_html()
}

/// The per-row tri-state segmented control (`#cap-seg-<kind>-<key>`). Tools get
/// all three segments; skills get only Auto / On (a skill can't be "blocked").
fn render_cap_segment(base: &str, c: &CapabilityRow) -> Html {
    let include_off = c.kind == CapKind::Tool;
    segmented(
        &cap_seg_id(c),
        base,
        c.kind.as_str(),
        &c.key,
        Some(c.state),
        include_off,
    )
}

/// The group-aggregate pill (`#cap-grp-<slug>`): a segmented control that sets
/// every row in the group at once. Its active segment reflects the aggregate —
/// none highlighted when the rows disagree (mixed).
fn render_group_pill(base: &str, group: &'static str, rows: &[&CapabilityRow]) -> Html {
    let include_off = group != "Skills";
    segmented(
        &format!("cap-grp-{}", group_slug(group)),
        base,
        "group",
        &group_slug(group),
        aggregate_state(rows),
        include_off,
    )
}

/// The master pill (`#cap-all`): sets every capability at once. Three segments
/// (applying Off to a skill is treated as Auto by the handler).
fn render_master_pill(base: &str, caps: &[CapabilityRow]) -> Html {
    let rows: Vec<&CapabilityRow> = caps.iter().collect();
    segmented("cap-all", base, "all", "*", aggregate_state(&rows), true)
}

/// Render a segmented Off/Auto/On (or Auto/On) control. `current` is the active
/// tier, or `None` for a mixed aggregate (nothing highlighted). Each segment
/// `@post`s `?kind=&key=&state=` — the same endpoint for rows, groups, and All.
fn segmented(
    id: &str,
    base: &str,
    kind: &str,
    key: &str,
    current: Option<ToolState>,
    include_off: bool,
) -> Html {
    let mut segs: Vec<ToolState> = Vec::new();
    if include_off {
        segs.push(ToolState::Off);
    }
    segs.push(ToolState::Auto);
    segs.push(ToolState::On);
    html! {
        span(
            id: (id),
            style: "display:inline-flex; flex:none; border-radius:9999px; overflow:hidden; \
                    border:1px solid color-mix(in oklch, currentColor 18%, transparent)"
        ) {
            for st in segs.iter() {
                (seg_button(base, kind, key, *st, current == Some(*st)))
            }
        }
    }
    .to_html()
}

/// One segment button within a [`segmented`] control.
fn seg_button(base: &str, kind: &str, key: &str, st: ToolState, active: bool) -> Html {
    let (glyph, tip, active_color) = match st {
        ToolState::Off => (
            "⊘",
            "Off — blocked; hidden from the assistant",
            "var(--color-error, #dc2626)",
        ),
        ToolState::Auto => (
            "◐",
            "Auto — the assistant turns it on when a request needs it",
            "currentColor",
        ),
        ToolState::On => (
            "✓",
            "On — always available to the assistant",
            "var(--color-success, #16a34a)",
        ),
    };
    let base_style = "padding:0.05rem 0.45rem; font-size:0.8rem; line-height:1.5; \
                      background:transparent; border:0; cursor:pointer";
    let style = if active {
        format!(
            "{base_style}; background:color-mix(in oklch, currentColor 16%, transparent); \
             font-weight:700; color:{active_color}"
        )
    } else {
        format!("{base_style}; opacity:0.5")
    };
    html! {
        button(
            type: "button",
            title: (tip),
            "aria-label": (tip),
            "data-on:click": (format!("@post('{base}?kind={kind}&key={key}&state={}')", st.as_str())),
            style: (style)
        ) { (glyph) }
    }
    .to_html()
}

/// The aggregate tier of a group: `Some(state)` when every row agrees, else
/// `None` (mixed). An empty group reads as Auto.
fn aggregate_state(rows: &[&CapabilityRow]) -> Option<ToolState> {
    let mut it = rows.iter();
    let first = match it.next() {
        Some(c) => c.state,
        None => return Some(ToolState::Auto),
    };
    if it.all(|c| c.state == first) {
        Some(first)
    } else {
        None
    }
}

/// DOM id for a row's segmented control. Patched in place on a toggle.
fn cap_seg_id(c: &CapabilityRow) -> String {
    format!("cap-seg-{}-{}", c.kind.as_str(), c.key)
}

/// DOM id for a row wrapper (structural — never re-patched).
fn cap_row_id(c: &CapabilityRow) -> String {
    format!("cap-row-{}-{}", c.kind.as_str(), c.key)
}

/// URL-safe slug for a group label (`"Web & Network"` → `"web-network"`), used
/// both as the `$openCats` key and the group pill's wire `key`.
pub(super) fn group_slug(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_dash = false;
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Icon for a capability row: the connector's brand logo (or its catalog emoji,
/// else a generic plug) for integrations; a sparkle for skills.
fn cap_icon(c: &CapabilityRow) -> Html {
    match c.kind {
        CapKind::Skill => icons::sparkles(16),
        CapKind::Tool => {
            let connector = c.key.strip_prefix("mcp__").unwrap_or(c.key.as_str());
            // Brand logo by connector key, then by the catalog `icon` hint
            // (seeded connectors store a brand key like "gitlab" there, so the
            // self-managed variant still gets the GitLab mark). Only fall back
            // to rendering `icon` as text when it's an actual emoji
            // (non-ASCII) — never a brand-key string — else a generic plug.
            icons::connector_logo(connector, 16)
                .or_else(|| c.icon.as_deref().and_then(|i| icons::connector_logo(i, 16)))
                .unwrap_or_else(|| match c.icon.as_deref() {
                    Some(emoji) if !emoji.is_empty() && !emoji.is_ascii() => {
                        html! { span(style: "font-size:1rem; line-height:1") { (emoji.to_string()) } }
                            .to_html()
                    }
                    _ => icons::plug(16),
                })
        }
    }
}

/// The owner's share toggle. datastar-driven: the click `@post`s to flip the
/// `shared` flag, and the handler answers by re-patching *this* control
/// (`#share-toggle`) with the flipped label **and** firing the authoritative
/// toast off the resulting state — so a stale view can never tell the user
/// "shared, everyone can read" while it actually un-shares. The only
/// client-side bit is the clipboard copy: it needs a user gesture, so it can't
/// move to the SSE response. We copy when *enabling* (the freshly-rendered
/// state says not-yet-shared); copying is harmless if that state was stale, and
/// the server toast governs what the user is actually told. Plain (non-JS) POST
/// still works as a fallback — the handler redirects.
pub(super) fn render_share_control(share_url: &str, shared: bool) -> Html {
    // Plain `@post` (default JSON content type). The toggle is a standalone
    // `<button>`, not wrapped in a `<form>`, and the handler reads nothing
    // from the body — it keys off the path alone. Asking datastar for
    // `contentType:'form'` would make it `closest('form')` and throw
    // `FetchFormNotFound`, so the share click never fires.
    let toggle = format!("@post('{share_url}')");
    let on_click = if shared {
        toggle
    } else {
        // Best-effort copy of the current chat URL, then persist the share.
        format!(
            "navigator.clipboard && navigator.clipboard.writeText(window.location.href); {toggle}"
        )
    };
    let label = if shared { "Shared ✓" } else { "Share" };
    html! {
        button(
            id: "share-toggle",
            type: "button",
            "data-on:click": (on_click),
            class: "btn btn-ghost btn-sm whitespace-nowrap",
            title: "Shared chats are readable by any signed-in user who has the link"
        ) {
            (label)
        }
    }
    .to_html()
}

/// "Continue in my chats": shown to a read-only viewer of a shared
/// conversation. A standalone `@post` button (same pattern as the share
/// toggle — no enclosing `<form>`, the handler keys off the path) that
/// copies the conversation into the viewer's account and navigates into
/// the editable copy. Owner-only mutation it is *not*: the endpoint
/// re-checks that the caller isn't the owner.
pub(super) fn render_fork_control(session_id: &str) -> Html {
    let fork_url = format!("/chat/{session_id}/fork");
    html! {
        button(
            id: "fork-button",
            type: "button",
            "data-on:click": (format!("@post('{fork_url}')")),
            class: "btn btn-primary btn-sm whitespace-nowrap",
            title: "Copy this conversation into your own chats so you can keep chatting"
        ) {
            (icons::copy(16))
            span(class: "hidden sm:inline") { "Continue in my chats" }
        }
    }
    .to_html()
}

/// Export menu: a `<details>`-based daisyUI dropdown with one plain
/// download link per format. Pure HTML — no datastar directives — so the
/// browser performs an ordinary GET and the handler's
/// `Content-Disposition: attachment` triggers a download. The links must
/// stay free of `data-on:*` so the SPA-nav path doesn't swallow them and
/// try to morph a binary/markdown body into the page.
pub(super) fn render_export_control(session_id: &str) -> Html {
    let md_url = format!("/chat/{session_id}/export.md");
    let pdf_url = format!("/chat/{session_id}/export.pdf");
    html! {
        details(class: "dropdown dropdown-end") {
            summary(
                class: "btn btn-ghost btn-sm whitespace-nowrap",
                title: "Download this conversation"
            ) {
                (icons::download(16))
                span(class: "hidden sm:inline") { "Export" }
            }
            ul(class: "dropdown-content menu bg-base-100 rounded-box z-10 mt-1 w-48 p-2 shadow") {
                li {
                    a(href: (pdf_url), download: "download") { "PDF document" }
                }
                li {
                    a(href: (md_url), download: "download") { "Markdown (.md)" }
                }
            }
        }
    }
    .to_html()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;
    use session_core::db::Session;

    fn session() -> Session {
        let now = Timestamp::now();
        Session {
            id: "s1".into(),
            user_id: "u1".into(),
            title: None,
            created_at: now,
            updated_at: now,
            shared: false,
            pinned: false,
        }
    }

    fn page_body(in_flight: Option<&str>) -> String {
        page_body_ro(in_flight, false)
    }

    fn page_body_ro(in_flight: Option<&str>, read_only: bool) -> String {
        let s = session();
        render_chat_page(ChatPage {
            active: &s,
            turns: &[],
            in_flight_turn_id: in_flight,
            models: &[],
            transcription_models: &[],
            error_msg: None,
            read_only,
            shared: false,
            effort: crate::server::reasoning::Effort::Standard,
            capabilities: &[],
            document_canvas_html: None,
        })
        .to_string()
    }

    fn page_body_with_models(models: &[ChatModelOption], read_only: bool) -> String {
        let s = session();
        render_chat_page(ChatPage {
            active: &s,
            turns: &[],
            in_flight_turn_id: None,
            models,
            transcription_models: &[],
            error_msg: None,
            read_only,
            shared: false,
            effort: crate::server::reasoning::Effort::Standard,
            capabilities: &[],
            document_canvas_html: None,
        })
        .to_string()
    }

    fn opt(id: &str, gdpr: bool, nda: bool) -> ChatModelOption {
        ChatModelOption {
            id: id.into(),
            gdpr,
            nda,
        }
    }

    #[test]
    fn model_label_suffixes_each_restriction() {
        assert_eq!(model_label(&opt("qwen", true, true)), "qwen");
        assert_eq!(
            model_label(&opt("glm-4.6", false, true)),
            "glm-4.6 (non-GDPR)"
        );
        assert_eq!(
            model_label(&opt("x", true, false)),
            "x (confidential-restricted)"
        );
        assert_eq!(
            model_label(&opt("glm-5.2", false, false)),
            "glm-5.2 (non-GDPR, confidential-restricted)"
        );
    }

    #[test]
    fn compliant_models_emit_no_warning_banner_or_flags() {
        // All-clear models: the option carries no suffix, and the flag arrays
        // are empty so the banner can never show.
        let body = page_body_with_models(&[opt("qwen", true, true)], false);
        assert!(
            !body.contains("non-GDPR"),
            "no suffix for clear model: {body}"
        );
        assert!(
            body.contains("gdprFlagged: []") && body.contains("ndaFlagged: []"),
            "clear models must yield empty flag arrays: {body}"
        );
        assert!(
            !body.contains("non-GDPR-compliant model"),
            "no GDPR banner copy expected: {body}"
        );
    }

    #[test]
    fn flagged_model_wires_dropdown_suffix_signals_and_banner() {
        // The end-to-end wiring for a non-GDPR model: dropdown suffix, the
        // model in the gdprFlagged signal array, the default selected model
        // seeded, and the reactive banner present (hidden until selected).
        let body = page_body_with_models(&[opt("glm-4.6", false, true)], false);
        assert!(
            body.contains("glm-4.6 (non-GDPR)"),
            "dropdown option must show the suffix: {body}"
        );
        // Attribute values are HTML-escaped (`"` → `&quot;`); the browser
        // un-escapes them before datastar parses the object literal.
        assert!(
            body.contains(r#"gdprFlagged: [&quot;glm-4.6&quot;]"#),
            "model must be in the gdprFlagged signal array: {body}"
        );
        assert!(
            body.contains(r#"selectedModel: &quot;glm-4.6&quot;"#),
            "default model must seed selectedModel: {body}"
        );
        assert!(
            body.contains("$gdprFlagged.includes($selectedModel)"),
            "GDPR banner must react to the selected model: {body}"
        );
        assert!(
            body.contains("non-GDPR-compliant model. Do not enter personal"),
            "GDPR banner copy must be present: {body}"
        );
        // nda is clear here → it must not land in the nda array.
        assert!(
            body.contains("ndaFlagged: []"),
            "nda-clear model must not be flagged for NDA: {body}"
        );
        // The picker must update the signal on change.
        assert!(
            body.contains("$selectedModel = evt.target.value"),
            "model select must update selectedModel on change: {body}"
        );
        // …and the banner must reflect the *initial* selection too — a
        // browser-restored / default flagged model fires no change event, so
        // the signal has to sync from the live picker value on mount or the
        // banner silently stays hidden (regression guard).
        // `'` is HTML-escaped to `&#39;` in the attribute value.
        assert!(
            body.contains(r#"$selectedModel = document.getElementById(&#39;model&#39;)?.value"#),
            "compliance signals must sync from the picker on data-init: {body}"
        );
    }

    #[test]
    fn read_only_viewer_sees_no_compliance_ui() {
        // A read-only shared viewer sends nothing, so no picker and no banner.
        let body = page_body_with_models(&[opt("glm-4.6", false, false)], true);
        assert!(
            !body.contains("gdprFlagged"),
            "read-only view must not wire compliance signals: {body}"
        );
        assert!(
            !body.contains("non-GDPR-compliant model"),
            "read-only view must not show the banner: {body}"
        );
    }

    #[test]
    fn share_control_posts_without_form_serialization() {
        // The share button is a standalone <button>, never inside a <form>.
        // `@post(..., {contentType:'form'})` makes datastar look for an
        // enclosing form and throw FetchFormNotFound, so the toggle silently
        // never fires. The directive must post plainly.
        let s = render_share_control("/chat/s1/share", false).to_string();
        // The attribute value is HTML-escaped (`'` → `&#39;`); datastar
        // un-escapes it back to `@post('/chat/s1/share')` at parse time.
        assert!(
            s.contains("@post(&#39;/chat/s1/share&#39;)"),
            "share toggle must POST plainly; got {s}"
        );
        assert!(
            !s.contains("contentType"),
            "no enclosing form exists — must not request form serialization; got {s}"
        );
    }

    #[test]
    fn in_flight_turn_arms_the_stop_control_on_load() {
        // The bug: reloading mid-turn left $chatStreaming=false, so no Stop
        // button. With a turn in flight the page must seed it true.
        assert!(
            page_body(Some("t1")).contains("chatStreaming: true"),
            "an in-flight turn must arm the Stop control server-side"
        );
    }

    #[test]
    fn idle_page_renders_composer_not_streaming() {
        assert!(
            page_body(None).contains("chatStreaming: false"),
            "an idle page must not seed the streaming/stop state"
        );
    }

    #[test]
    fn export_links_present_for_owner_and_shared_viewer() {
        // The export menu links straight at the download endpoints and must
        // be reachable by both the owner and a read-only viewer of a shared
        // chat — so it lives outside the owner-only header block.
        for read_only in [false, true] {
            let body = page_body_ro(None, read_only);
            assert!(
                body.contains("/chat/s1/export.pdf"),
                "PDF export link missing (read_only={read_only}): {body}"
            );
            assert!(
                body.contains("/chat/s1/export.md"),
                "Markdown export link missing (read_only={read_only}): {body}"
            );
        }
    }

    #[test]
    fn fork_button_only_for_read_only_viewer_and_posts_plainly() {
        // The recipient of a shared chat (read_only) gets a fork button wired
        // to POST /chat/{id}/fork; the owner never sees it.
        let owner = page_body_ro(None, false);
        assert!(
            !owner.contains("/chat/s1/fork"),
            "owner must not see the fork button: {owner}"
        );

        let viewer = page_body_ro(None, true);
        assert!(
            viewer.contains("id=\"fork-button\""),
            "read-only viewer must see the fork button: {viewer}"
        );
        // Same plain-@post contract as the share toggle (no enclosing form).
        assert!(
            viewer.contains("@post(&#39;/chat/s1/fork&#39;)"),
            "fork button must POST plainly to the fork endpoint: {viewer}"
        );
        // A read-only viewer has no composer, so forking is the only way to
        // continue — the message endpoint must stay absent.
        assert!(
            !viewer.contains("/chat/s1/messages"),
            "read-only view must not expose the message endpoint: {viewer}"
        );
    }

    #[test]
    fn export_links_are_plain_downloads_not_spa_nav() {
        // A `data-on:*` directive here would let the SPA-nav path intercept
        // the click and try to morph a binary PDF into the page. The links
        // must stay plain anchors with `download`.
        let menu = render_export_control("s1").to_string();
        assert!(menu.contains("download"), "expected download attr: {menu}");
        assert!(
            !menu.contains("data-on"),
            "export links must not carry datastar directives: {menu}"
        );
    }

    #[test]
    fn effort_picker_wires_to_effort_endpoint_with_current_selected() {
        // The composer's "Denkaufwand" picker must post to /chat/{id}/effort
        // and pre-select the conversation's current level.
        let body = page_body(None);
        assert!(
            body.contains("/chat/s1/effort"),
            "effort picker must post to the effort endpoint: {body}"
        );
        for label in ["Fast", "Standard", "Deep", "Max"] {
            assert!(
                body.contains(label),
                "missing effort level `{label}`: {body}"
            );
        }
        // page_body seeds Effort::Standard → that option is the selected one.
        assert!(
            body.contains("value=\"standard\" selected=\"selected\""),
            "the current effort must be pre-selected: {body}"
        );
    }

    fn cap(
        key: &str,
        kind: CapKind,
        label: &str,
        group: &'static str,
        order: u8,
        state: ToolState,
    ) -> CapabilityRow {
        CapabilityRow {
            key: key.into(),
            kind,
            label: label.into(),
            description: format!("what {label} does"),
            group,
            order,
            state,
            icon: None,
        }
    }

    fn sample_caps() -> Vec<CapabilityRow> {
        vec![
            cap(
                "dns_lookup",
                CapKind::Tool,
                "DNS lookup",
                "Web & Network",
                0,
                ToolState::On,
            ),
            cap(
                "fetch_url",
                CapKind::Tool,
                "Fetch a web page",
                "Web & Network",
                0,
                ToolState::Auto,
            ),
            cap(
                "mcp__gitlab",
                CapKind::Tool,
                "GitLab",
                "Integrations",
                5,
                ToolState::Off,
            ),
            cap(
                "brand",
                CapKind::Skill,
                "Brand",
                "Skills",
                SKILL_ORDER,
                ToolState::Auto,
            ),
        ]
    }

    #[test]
    fn capabilities_menu_wires_tristate_and_reflects_state() {
        let caps = sample_caps();
        let html = render_capabilities("s1", &caps).to_string();

        // Wrapper carries the persistent signals (open / search / collapse).
        assert!(
            html.contains("id=\"cap-wrap\""),
            "wrapper id missing: {html}"
        );
        assert!(
            html.contains("id=\"capabilities\""),
            "mount id missing: {html}"
        );
        assert!(
            html.contains("capMenu") && html.contains("openCats"),
            "signals missing: {html}"
        );

        // No nested <form> — the composer is the page's only form.
        assert!(
            !html.contains("<form"),
            "capability controls must not be nested forms: {html}"
        );

        // Category section headings present.
        assert!(
            html.contains("Web &amp; Network"),
            "category heading missing: {html}"
        );
        assert!(html.contains("Skills"), "skills heading missing: {html}");

        // Per-row segmented controls carry their stable ids and all three
        // states wire to the same endpoint with kind=tool&key&state.
        assert!(
            html.contains("id=\"cap-seg-tool-dns_lookup\""),
            "row seg id missing: {html}"
        );
        assert!(
            html.contains("kind=tool&amp;key=dns_lookup&amp;state=on"),
            "On segment must wire kind+key+state: {html}"
        );
        assert!(
            html.contains("kind=tool&amp;key=dns_lookup&amp;state=off"),
            "Off segment must wire state=off: {html}"
        );
        assert!(
            html.contains("kind=tool&amp;key=dns_lookup&amp;state=auto"),
            "Auto segment must wire state=auto: {html}"
        );

        // Group + master aggregate pills exist and wire kind=group / kind=all.
        assert!(
            html.contains("id=\"cap-grp-web-network\""),
            "group pill id missing: {html}"
        );
        assert!(
            html.contains("id=\"cap-all\""),
            "master pill missing: {html}"
        );
        assert!(
            html.contains("kind=group&amp;key=web-network&amp;state=on"),
            "group pill must wire kind=group with slug: {html}"
        );
        assert!(
            html.contains("kind=all&amp;key=*&amp;state=on"),
            "master pill must wire kind=all: {html}"
        );

        // Skills get a 2-state control: no Off segment for the skill row.
        assert!(
            !html.contains("kind=skill&amp;key=brand&amp;state=off"),
            "skills must not expose an Off segment: {html}"
        );
        assert!(
            html.contains("kind=skill&amp;key=brand&amp;state=on"),
            "skill On segment must wire: {html}"
        );

        // The On tool surfaces as a removable chip that un-pins back to Auto.
        assert!(
            html.contains("id=\"cap-chips\""),
            "chips region missing: {html}"
        );
        assert!(
            html.contains("kind=tool&amp;key=dns_lookup&amp;state=auto"),
            "On chip must un-pin to Auto: {html}"
        );

        // Closed by default on page load.
        assert!(
            html.contains("display:none"),
            "menu must start closed: {html}"
        );
    }

    #[test]
    fn capability_patches_target_only_volatile_leaves() {
        let caps = sample_caps();
        let patches = render_capability_patches("s1", &caps);
        let selectors: Vec<&str> = patches.iter().map(|(s, _)| s.as_str()).collect();

        // Every row's seg control, every group pill, the master, and chips.
        assert!(selectors.contains(&"#cap-seg-tool-dns_lookup"));
        assert!(selectors.contains(&"#cap-seg-skill-brand"));
        assert!(selectors.contains(&"#cap-grp-web-network"));
        assert!(selectors.contains(&"#cap-all"));
        assert!(selectors.contains(&"#cap-chips"));

        // Crucially: never the signal-bearing structure, so the morph can't
        // reset open/collapse or search.
        assert!(
            !selectors
                .iter()
                .any(|s| *s == "#cap-wrap" || *s == "#capabilities"),
            "patches must not touch the signal-bearing containers: {selectors:?}"
        );
    }

    #[test]
    fn group_slug_is_url_safe() {
        assert_eq!(group_slug("Web & Network"), "web-network");
        assert_eq!(
            group_slug("Attachments & Documents"),
            "attachments-documents"
        );
        assert_eq!(group_slug("Skills"), "skills");
    }

    #[test]
    fn aggregate_state_detects_mixed() {
        let on = cap("a", CapKind::Tool, "A", "G", 0, ToolState::On);
        let off = cap("b", CapKind::Tool, "B", "G", 0, ToolState::Off);
        assert_eq!(aggregate_state(&[&on, &on]), Some(ToolState::On));
        assert_eq!(aggregate_state(&[&on, &off]), None);
        assert_eq!(aggregate_state(&[]), Some(ToolState::Auto));
    }

    #[test]
    fn read_only_view_exposes_no_capabilities_or_effort_controls() {
        // A shared read-only viewer has no composer, so neither the effort
        // picker nor the "+" menu (both owner-only mutations) may appear.
        let viewer = page_body_ro(None, true);
        assert!(
            !viewer.contains("/chat/s1/effort"),
            "read-only view must not expose the effort endpoint: {viewer}"
        );
        assert!(
            !viewer.contains("/chat/s1/capabilities"),
            "read-only view must not expose the capabilities endpoint: {viewer}"
        );
    }
}
