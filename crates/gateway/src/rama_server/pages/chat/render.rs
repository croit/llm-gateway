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
    pub models: &'a [String],
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
    let models_vec: Vec<String> = page.models.to_vec();
    let models_empty = models_vec.is_empty();
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
            if !read_only {
            // `ml-auto`: with the title hidden on mobile this stays pinned
            // right (away from the top-left floating drawer button) instead of
            // collapsing left under `justify-content: space-between`.
            div(class: "flex items-center gap-2 ml-auto") {
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
                            class: "select select-bordered select-sm chat-model-select"
                        ) {
                            for m in models_vec.iter() {
                                option(value: (m.clone())) { (m.clone()) }
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
        let s = session();
        render_chat_page(ChatPage {
            active: &s,
            turns: &[],
            in_flight_turn_id: in_flight,
            models: &[],
            transcription_models: &[],
            error_msg: None,
            read_only: false,
            shared: false,
        })
        .to_string()
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
}
