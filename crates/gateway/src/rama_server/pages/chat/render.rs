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

    let post_url = format!("/chat/{session_id}/messages");
    let cancel_url = format!("/chat/{session_id}/cancel");
    let share_url = format!("/chat/{session_id}/share");
    let read_only = page.read_only;

    html! {
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
            }))
        }
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
}
