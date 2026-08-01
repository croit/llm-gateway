// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

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
    compacted_up_to_seq: Option<i64>,
    lang: Lang,
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
    // If the session has been compacted, the turns with `seq <= up_to_seq` are
    // no longer sent upstream (a summary stands in for them), but they stay in
    // the transcript. Mark the boundary with a divider so the reader knows
    // where the model's live context begins. Only shown when there's at least
    // one summarised turn above the boundary.
    let divider_at = compacted_up_to_seq.and_then(|cut| {
        let idx = turns_owned.iter().position(|t| t.turn.seq > cut)?;
        (idx > 0).then_some(idx)
    });
    let mut items: Vec<Html> = Vec::with_capacity(turns_owned.len() + 1);
    for (i, t) in turns_owned.iter().enumerate() {
        if Some(i) == divider_at {
            items.push(render_compaction_divider(lang));
        }
        items.push(render_turn(t, actions, lang));
    }
    html! {
        section(
            id: "conversation",
            "data-init": (init_directive)
        ) {
            for item in items.iter() {
                (item.clone())
            }
        }
    }
    .to_html()
}

/// The transcript marker between compacted (summarised) turns above and the
/// verbatim tail the model still sees below. Purely informational — the turns
/// above remain fully readable.
pub(crate) fn render_compaction_divider(lang: Lang) -> Html {
    let label = t(lang, "render-compaction-divider");
    html! {
        div(
            class: "divider text-xs opacity-60 my-2",
            role: "separator",
            "aria-label": (label.clone())
        ) {
            (icons::info(14))
            span { (label) }
        }
    }
    .to_html()
}

/// Dispatch on role. Renders the right bubble shape. `actions` is the
/// retry/edit base path (see [`render_conversation`]).
pub fn render_turn(turn: &TurnWithTools, actions: Option<&str>, lang: Lang) -> Html {
    match turn.turn.role {
        TurnRole::User => render_user_turn(&turn.turn, actions, lang),
        TurnRole::Assistant => render_assistant_turn(turn, actions, lang),
    }
}

/// A muted, always-visible per-message timestamp. Server-renders a UTC
/// `HH:MM` fallback inside `<time datetime=…>`; the client (`scroll.ts`)
/// localizes the visible text to the viewer's own locale/timezone and
/// sets a full date+time `title` on mount. Because streaming re-emits
/// the whole turn (mode `outer` on `#turn-<id>`), the server fallback
/// is written on every tick — `scroll.ts` re-applies the localized
/// text idempotently from the conversation `MutationObserver`, before
/// paint, so there's no flash.
pub(crate) fn render_msg_time(ts: jiff::Timestamp) -> Html {
    let iso = ts.to_string();
    let fallback = ts.strftime("%H:%M").to_string();
    html! {
        time(class: "chat-msg__time", datetime: (iso)) { (fallback) }
    }
    .to_html()
}

pub fn render_user_turn(turn: &Turn, actions: Option<&str>, lang: Lang) -> Html {
    let content = turn.user_content.clone().unwrap_or_default();
    let dom_id = format!("turn-{}", turn.id);
    let segments = crate::attachments::split_markers_for_turn(&content, &turn.id);
    let has_attachments = segments
        .iter()
        .any(|s| matches!(s, crate::attachments::Segment::Attachment(_)));
    let show_actions = actions.is_some();
    // Build the edit affordance first — it borrows `content` before the
    // body macro moves a clone of it into a closure.
    let edit_block = render_user_edit(turn, actions.unwrap_or(""), &content, lang);
    // The message body — either the plain text fast path or the
    // text+attachment segmented path. Kept in `.chat-msg__body` so the
    // edit form can replace it visually via the `.editing` class.
    let body = if !has_attachments {
        let body_text = content.clone();
        html! { div(class: "chat-msg__body") { (body_text) } }.to_html()
    } else {
        // Normalise the segments into body pieces, then let `render_body`
        // group any run of 2+ media into a numbered side-by-side gallery.
        let pieces: Vec<BodyPiece> = segments
            .iter()
            .filter_map(|seg| match seg {
                crate::attachments::Segment::Text(t) if !t.is_empty() => Some(BodyPiece::Block(
                    html! { div(class: "chat-msg__prose") { (t.to_string()) } }.to_html(),
                )),
                crate::attachments::Segment::Text(_) => None,
                crate::attachments::Segment::Attachment(att) => Some(match media_kind(att) {
                    Some(_) => BodyPiece::Media(att.clone()),
                    None => BodyPiece::File(att.clone()),
                }),
            })
            .collect();
        // Per-attachment removal is offered on the same gate as the edit
        // affordance: only when actions are shown (owner, not a shared
        // read-only view). `{base}/{session}/turns/{turn}/attachment` is
        // the per-turn prefix; `render_attachment` appends `/{file}/remove`.
        let remove_prefix =
            actions.map(|base| format!("{base}/{}/turns/{}/attachment", turn.session_id, turn.id));
        let items = render_body(&pieces, remove_prefix.as_deref(), lang);
        html! {
            div(class: "chat-msg__body") {
                for item in items.iter() { (item.clone()) }
            }
        }
        .to_html()
    };
    html! {
        div(id: (dom_id), class: "chat-msg--user") {
            (body)
            (render_msg_time(turn.created_at))
            if show_actions {
                (edit_block)
            }
        }
    }
    .to_html()
}

/// `/{base}/{session}/turns/{turn}/{action}` — the per-message action
/// endpoint URL.
pub(crate) fn action_url(base: &str, turn: &Turn, action: &str) -> String {
    format!("{base}/{}/turns/{}/{action}", turn.session_id, turn.id)
}

/// Datastar submit directive for a retry/edit form: copy the current
/// model dropdown into the form's hidden `model` input, confirm the
/// destructive drop, arm the Stop control, then `@post` (whose SSE
/// response streams the regenerated reply back in).
///
/// `$chatStreaming = true` matters as much here as on the composer's own
/// submit: retry/edit spawn a *real* worker, so without it the whole
/// regenerated turn ran with the composer sitting in its "ready" state —
/// no Stop button (nothing to interrupt a runaway retry with), and the
/// Enter-to-submit guard in `composer.ts` (which reads the
/// `.chat-composer--streaming` class) let the user fire a second message
/// that the server could only reject as "already streaming". The server
/// re-asserts the same signal in its response (see the regeneration
/// handler), so a stale client still ends up armed.
///
/// `confirm` is JSON-encoded (not hand-escaped) before splicing into the
/// JS string literal: JSON string syntax is a strict subset of JS string
/// syntax, so this correctly escapes quotes/backslashes/control chars in
/// one call — safe even once `confirm` carries translated text that may
/// contain apostrophes (French/Spanish) the caller didn't anticipate.
pub(crate) fn action_submit(url: &str, confirm: &str) -> String {
    let confirm_js = serde_json::to_string(confirm).expect("String always serialises");
    format!(
        "window.chatActions.fillModel(el) && confirm({confirm_js}) && \
         ($chatStreaming = true, @post('{url}', {{contentType: 'form'}}))"
    )
}

/// The user-bubble edit affordance: a hover "Edit" button + a hidden
/// inline edit form (revealed by toggling `.editing` on the bubble via
/// `window.chatActions`). Submitting drops everything below this turn
/// and regenerates from the edited text.
pub(crate) fn render_user_edit(turn: &Turn, base: &str, content: &str, lang: Lang) -> Html {
    let id = turn.id.clone();
    let content = content.to_string();
    let edit_url = action_url(base, turn, "edit");
    let confirm_text = t(lang, "render-edit-confirm");
    let submit = action_submit(&edit_url, &confirm_text);
    let start = format!("window.chatActions.editStart('{id}')");
    let cancel = format!("window.chatActions.editCancel('{id}')");
    let edit_label = t(lang, "render-edit-button");
    let save_label = t(lang, "render-edit-save");
    let cancel_label = t(lang, "render-edit-cancel");
    let attach_aria = t(lang, "render-composer-attach-aria");
    let attach_title = t(lang, "render-composer-attach-title");
    html! {
        div(class: "chat-msg__actions") {
            button(
                type: "button",
                class: "chat-msg__action",
                "data-on:click": (start)
            ) {
                (edit_label)
            }
        }
        // `enctype=multipart/form-data` + the hidden file input let this
        // form carry pasted/dropped/picked attachments, exactly like the
        // main composer. `@post(url, {contentType:'form'})` (see
        // `action_submit`) then serialises it as multipart so `chat_edit`
        // can upload them. The paste/drop/pick handlers in
        // `chat/actions.ts` are scoped to this form (not a singleton id),
        // so every message's edit form manages its own attachments.
        form(
            action: (edit_url),
            method: "post",
            enctype: "multipart/form-data",
            class: "chat-msg__edit",
            "data-on:submit__prevent": (submit),
            "data-on:dragover__prevent": "window.chatActions.editDragOver(evt)",
            "data-on:dragleave__prevent": "window.chatActions.editDragLeave(evt)",
            "data-on:drop__prevent": "window.chatActions.editDrop(evt)",
            "data-on:paste": "window.chatActions.editPaste(evt)"
        ) {
            input(type: "hidden", name: "model");
            // Hidden file input — `name="attachment"` so `chat_edit`'s
            // multipart parser picks it up; `multiple` accepts batch picks.
            input(
                name: "attachment",
                type: "file",
                multiple: "multiple",
                hidden: "hidden",
                "data-on:change": "window.chatActions.editFilesPicked(evt)"
            );
            // Chip strip — `chat-composer__chips` for the shared styling
            // (incl. `:empty` hide); `chat-msg__edit-chips` is the hook
            // the edit handlers query it by.
            div(class: "chat-composer__chips chat-msg__edit-chips") {}
            textarea(name: "message", class: "chat-msg__edit-textarea") { (content) }
            div(class: "chat-msg__edit-actions") {
                // Attach button — opens the hidden file input. Left of the
                // Save/Cancel cluster (which stays right-aligned).
                button(
                    type: "button",
                    class: "btn btn-sm btn-circle btn-ghost chat-msg__edit-attach",
                    "data-on:click": "window.chatActions.editPickFiles(el)",
                    "aria-label": (attach_aria),
                    title: (attach_title)
                ) {
                    (icons::paperclip(16))
                }
                span(class: "chat-msg__edit-actions-main") {
                    button(type: "submit", class: "btn btn-sm btn-primary") { (save_label) }
                    button(
                        type: "button",
                        class: "btn btn-sm btn-ghost",
                        "data-on:click": (cancel)
                    ) { (cancel_label) }
                }
            }
        }
    }
    .to_html()
}
