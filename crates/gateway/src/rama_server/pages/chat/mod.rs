// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Multi-conversation chat page.
//!
//! Routes:
//!
//! | Method | Path                       | What |
//! |--------|----------------------------|------|
//! | GET    | /chat                      | redirect to latest session (or create one) |
//! | GET    | /chat/{id}                 | render a specific session |
//! | POST   | /chat/sessions             | create a fresh session + nav to it |
//! | POST   | /chat/{id}/messages        | submit a user message; spawns worker; SSE-tails the live broadcast |
//! | GET    | /chat/{id}/tail            | subscribe to whatever worker is running for this user + session |
//! | POST   | /chat/{id}/cancel          | flip the worker's cancel flag |
//! | POST   | /chat/{id}/delete          | remove the session + nav to the next one |
//!
//! Worker lifecycle: `POST /chat/{id}/messages` creates the user turn,
//! creates the assistant turn (status `in_progress`), then spawns
//! `worker::run_chat_turn`. The worker writes content / reasoning /
//! tool-call deltas straight to SQLite and broadcasts a `Tick` after
//! every DB write. All HTTP subscribers (the messages POST itself + any
//! tail GET) re-read the row from the DB on each tick and emit the
//! same `mode outer` patch keyed to `#turn-<uuid>`. DB is the source of
//! truth; nothing the subscriber emits depends on in-memory state.

use std::sync::Arc;

use rama::http::service::web::extract::{Path, State};
use rama::http::{Request, Response};

use super::{
    NavItem, SidebarChat, SidebarSession, html_authed_page, internal_error_html, is_admin,
    nav_or_html_page, require_session_or_redirect,
};
use session_core::chat::{
    SidebarEmitter, SseTx, cancel_turn as chat_cancel_turn, empty_sse_response,
    spawn_session_stream_response, sse_error_response,
};
use session_core::chrome::{
    Theme, is_datastar_request, read_body_to_bytes, see_other, sse_patch, sse_response,
    sse_signals, sse_toast,
};
use session_core::{RegisterOutcome, TurnUpdate};

use session_core::db as chat;
use session_core::export;

use crate::rama_server::state::RamaState;
use crate::server::chat_attachments;
use crate::server::db::users::User;

mod render;
mod title;

// ---------------------------------------------------------------------------
// GET /chat — redirect to latest (or new) session.

pub async fn chat_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let datastar = is_datastar_request(req.headers());
    let target = match resolve_landing_session(&state, &user).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if datastar {
        render_chat_response(
            state.clone(),
            &user,
            target,
            datastar,
            session.impersonator_id.is_some(),
        )
        .await
    } else {
        see_other(&format!("/chat/{}", target.id))
    }
}

async fn resolve_landing_session(
    state: &RamaState,
    user: &User,
) -> Result<chat::Session, Response> {
    match chat::latest_session(&state.db, &user.id).await {
        Ok(Some(s)) => Ok(s),
        Ok(None) => chat::create_session(&state.db, &user.id)
            .await
            .map_err(|err| internal_error_html(&user.email, &err.to_string())),
        Err(err) => Err(internal_error_html(&user.email, &err.to_string())),
    }
}

// ---------------------------------------------------------------------------
// GET /chat/{id} — render a specific session.

pub async fn chat_session_view(
    Path(session_id): Path<String>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let datastar = is_datastar_request(req.headers());
    // Readable = owned OR shared. A non-owner viewing a shared chat gets a
    // read-only render (see `render_chat_response`); mutations stay owner-only.
    let target = match chat::get_session_readable(&state.db, &user.id, &session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return see_other("/chat"),
        Err(err) => return internal_error_html(&user.email, &err.to_string()),
    };
    render_chat_response(
        state.clone(),
        &user,
        target,
        datastar,
        session.impersonator_id.is_some(),
    )
    .await
}

async fn render_chat_response(
    state: Arc<RamaState>,
    user: &User,
    active: chat::Session,
    datastar: bool,
    impersonating: bool,
) -> Response {
    let theme = Theme::from_headers(&rama::http::HeaderMap::new());
    let sessions = match chat::list_sessions(&state.db, &user.id).await {
        Ok(s) => s,
        Err(err) => return internal_error_html(&user.email, &err.to_string()),
    };
    // Read-only when the viewer isn't the owner (only reachable for a shared
    // session — get_session_readable gated the load). The server enforces
    // owner-only mutations independently; this shapes the UI *and* gates the
    // owner-only side effects below.
    let read_only = active.user_id != user.id;

    // Sweep stale `in_progress` rows before we list — OWNER ONLY. A turn is
    // only live if a worker is actively driving it; anything else is an orphan
    // (legacy "create row before reserving worker" leak, or a crash artefact)
    // and rendering it would show a forever-spinning bubble. The exempt id is
    // the live worker's turn, looked up in the *viewer's* registry slot — which
    // is exactly why this must never run for a non-owner: their slot holds no
    // worker for this session, so the sweep would flip the owner's genuinely
    // *live* turn to `errored` mid-stream. A non-owner read must not mutate the
    // owner's session at all; the owner's own next view clears any real orphan.
    if !read_only {
        let exempt_turn_id: Option<String> = state
            .chats
            .get(&user.id)
            .filter(|w| w.session_id == active.id)
            .map(|w| w.turn_id.clone());
        let _ = chat::mark_orphaned_in_progress_as_errored(
            &state.db,
            &active.id,
            exempt_turn_id.as_deref(),
        )
        .await;
    }
    let turns = match chat::list_turns(&state.db, &active.id).await {
        Ok(t) => t,
        Err(err) => return internal_error_html(&user.email, &err.to_string()),
    };
    // Live tail is owner-only too (workers are keyed by the owner's id), so
    // don't arm the auto-tail for a read-only viewer: they get a static
    // snapshot, and an in-progress turn just shows its partial state until they
    // reload — rather than firing a tail that immediately reports "not
    // streaming" and leaves a spinner hanging.
    let in_flight_turn_id = if read_only {
        None
    } else {
        chat::in_flight_turn(&state.db, &active.id)
            .await
            .ok()
            .flatten()
            .map(|t| t.id)
    };
    let models = list_chat_models(&state).await;
    let transcription_models = list_transcription_models(&state).await;
    let body = render::render_chat_page(render::ChatPage {
        active: &active,
        turns: &turns,
        in_flight_turn_id: in_flight_turn_id.as_deref(),
        models: &models,
        transcription_models: &transcription_models,
        error_msg: None,
        read_only,
        shared: active.shared,
    });
    let chat_sidebar = SidebarChat {
        sessions: sessions
            .into_iter()
            .map(|s| SidebarSession {
                id: s.id,
                title: s.title,
            })
            .collect(),
        active_session_id: Some(active.id.clone()),
    };
    let title = active.title.clone().unwrap_or_else(|| "Chat".to_string());
    let url = format!("/chat/{}", active.id);
    if datastar {
        nav_or_html_page(
            true,
            theme,
            NavItem::Chat,
            &format!("{title} — LLM Gateway"),
            &user.email,
            is_admin(&state, user),
            impersonating,
            body,
            &url,
            &chat_sidebar,
        )
    } else {
        html_authed_page(
            theme,
            Some(NavItem::Chat),
            &format!("{title} — LLM Gateway"),
            &user.email,
            is_admin(&state, user),
            impersonating,
            body,
            &chat_sidebar,
        )
    }
}

// ---------------------------------------------------------------------------
// POST /chat/sessions — new session + nav to it.

pub async fn chat_session_create(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let datastar = is_datastar_request(req.headers());
    let new_session = match chat::create_session(&state.db, &user.id).await {
        Ok(s) => s,
        Err(err) => return internal_error_html(&user.email, &err.to_string()),
    };
    if datastar {
        render_chat_response(
            state.clone(),
            &user,
            new_session,
            true,
            session.impersonator_id.is_some(),
        )
        .await
    } else {
        see_other(&format!("/chat/{}", new_session.id))
    }
}

// ---------------------------------------------------------------------------
// POST /chat/{id}/delete — drop the session + nav to the next one.

pub async fn chat_session_delete(
    Path(session_id): Path<String>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let datastar = is_datastar_request(req.headers());
    let deleted = match chat::delete_session(&state.db, &user.id, &session_id).await {
        Ok(v) => v,
        Err(err) => return internal_error_html(&user.email, &err.to_string()),
    };
    if !deleted {
        return sse_response(&[sse_toast(&super::Flash {
            kind: super::FlashKind::Info,
            message: "Conversation was already gone.".into(),
        })]);
    }
    let next = match resolve_landing_session(&state, &user).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    render_chat_response(
        state.clone(),
        &user,
        next,
        datastar,
        session.impersonator_id.is_some(),
    )
    .await
}

// ---------------------------------------------------------------------------
// POST /chat/{id}/share — owner toggles the read-only share flag. Shared
// sessions are readable by any signed-in user who knows the (UUID) link.

pub async fn chat_share_toggle(
    Path(session_id): Path<String>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let datastar = is_datastar_request(req.headers());
    // Owner-only on both reads and writes: get_session is owner-scoped, and
    // set_shared's UPDATE is `WHERE id = ? AND user_id = ?`. A non-owner POST
    // finds no session and is redirected away with no effect.
    let current = match chat::get_session(&state.db, &user.id, &session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return see_other("/chat"),
        Err(err) => return internal_error_html(&user.email, &err.to_string()),
    };
    let now_shared = !current.shared;
    if let Err(err) = chat::set_shared(&state.db, &user.id, &session_id, now_shared).await {
        return internal_error_html(&user.email, &err.to_string());
    }
    if !datastar {
        // No-JS fallback: full-page redirect (the client copy + the toast only
        // happen on the datastar path).
        return see_other(&format!("/chat/{session_id}"));
    }
    // datastar @post: re-patch just the toggle (label flips in place) and fire
    // the *authoritative* toast off the new flag. Driving the message from the
    // server — not the client's possibly-stale view — means a click that ends
    // up un-sharing can never claim "everyone can read this". A full re-render
    // is unnecessary: toggling `shared` changes nothing else on the page.
    let share_url = format!("/chat/{session_id}/share");
    let control = render::render_share_control(&share_url, now_shared).to_string();
    let toast = if now_shared {
        sse_toast(&super::Flash {
            kind: super::FlashKind::Success,
            message: "Link copied — any signed-in user with the link can read along.".into(),
        })
    } else {
        sse_toast(&super::Flash {
            kind: super::FlashKind::Info,
            message: "Sharing stopped — the link no longer works.".into(),
        })
    };
    sse_response(&[
        sse_patch(Some("#share-toggle"), Some("outer"), &control),
        toast,
    ])
}

// ---------------------------------------------------------------------------
// POST /chat/{id}/fork — copy a shared conversation into the viewer's
// account so the recipient can keep chatting (and re-share their copy).

pub async fn chat_fork(
    Path(session_id): Path<String>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let datastar = is_datastar_request(req.headers());

    // Recipient-only: the source must be readable (owner or shared) AND not
    // already owned by the viewer. Forking your own chat is a no-op — the
    // button is only rendered for read-only viewers, but guard the endpoint
    // too so a hand-crafted POST can't clone-spam an owner's own session.
    let src = match chat::get_session_readable(&state.db, &user.id, &session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return see_other("/chat"),
        Err(err) => return internal_error_html(&user.email, &err.to_string()),
    };
    if src.user_id == user.id {
        if !datastar {
            return see_other(&format!("/chat/{session_id}"));
        }
        return sse_response(&[sse_toast(&super::Flash {
            kind: super::FlashKind::Info,
            message: "This conversation is already in your chats.".into(),
        })]);
    }

    let (new_session, copies) = match chat::fork_session(&state.db, &src, &user.id).await {
        Ok(v) => v,
        Err(err) => return internal_error_html(&user.email, &err.to_string()),
    };

    // Best-effort: copy the attachment bytes to the new turn-scoped keys.
    // A copy failure leaves a marker pointing at an empty key (a broken
    // thumbnail) but the conversation text — the main value — still lands,
    // so we warn rather than roll the whole fork back.
    if let Some(cfg) = state.config.chat.s3.as_ref() {
        for c in &copies {
            if let Err(err) =
                chat_attachments::copy_object(cfg, &c.from_turn_id, &c.to_turn_id, &c.filename)
                    .await
            {
                tracing::warn!(
                    from = %c.from_turn_id, file = %c.filename,
                    "fork: failed to copy attachment object: {err}"
                );
            }
        }
    } else if !copies.is_empty() {
        tracing::warn!(
            count = copies.len(),
            "fork: chat attachments not configured; copied conversation references unreachable files"
        );
    }

    // Land the viewer in their fresh copy — it's owned by them now, so it
    // renders editable. Datastar morphs in place + updates the sidebar/URL;
    // a plain POST gets a redirect.
    let impersonating = session.impersonator_id.is_some();
    if datastar {
        render_chat_response(state.clone(), &user, new_session, true, impersonating).await
    } else {
        see_other(&format!("/chat/{}", new_session.id))
    }
}

// ---------------------------------------------------------------------------
// POST /chat/{id}/messages — submit + spawn worker + SSE.

pub async fn chat_message_send(
    Path(session_id): Path<String>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    // Make sure the user owns this session.
    let active = match chat::get_session(&state.db, &user.id, &session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return sse_error_response("Conversation not found.");
        }
        Err(err) => return sse_error_response(&err.to_string()),
    };

    // Snapshot the request's content-type header before consuming
    // the request — we need it to find the multipart boundary.
    let content_type = req
        .headers()
        .get(rama::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Source IP for `get_user_location`, snapshotted before we consume
    // the request (and before the worker, which has no request in scope).
    let client_ip = crate::server::geoip::client_ip(req.headers())
        .or_else(|| crate::server::geoip::peer_ip(&req));
    let secure =
        crate::server::geoip::transport_is_secure(req.headers(), &state.config.gateway.public_url);
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return sse_error_response(&msg),
    };
    // Pre-generate both turn ids:
    //   * `assistant_turn_id` keys the worker-registry slot below
    //     and (later) the in-progress assistant row.
    //   * `user_turn_id` keys the user-message row AND the S3
    //     prefix for any attachments uploaded on this submit. Same
    //     id at upload time + at render-refresh time so a hard
    //     reload re-presigns against the same object key.
    let assistant_turn_id = uuid::Uuid::new_v4().to_string();
    let user_turn_id = uuid::Uuid::new_v4().to_string();
    let submit = match parse_chat_submit(&content_type, body, &user_turn_id, &state).await {
        Ok(s) => s,
        Err(msg) => return sse_error_response(&msg),
    };
    if submit.user_text.is_empty() && submit.attachments.is_empty() {
        return sse_error_response("message can't be empty");
    }

    // Build the final user_text: typed text + per-attachment marker
    // (and an inlined fenced block for `text/*`-like attachments so
    // the model reads the bytes directly on the current turn).
    let user_msg = augment_user_text(&user_turn_id, &submit);

    // Reserve the per-user worker slot BEFORE persisting anything.
    // The old order (create turns → register) leaked orphaned
    // `in_progress` rows whenever register returned Busy (a quick
    // double-click, a datastar retry on a flaky connection): the rows
    // sat in the DB forever showing the thinking spinner, and the
    // user would see a duplicate of their conversation after reload
    // because the *next* submit succeeded and produced a parallel
    // (user + completed-assistant) pair. The pre-generated id is the
    // turn we'll insert immediately below, so the worker entry's
    // `turn_id` always matches the row that exists.
    let outcome = state
        .chats
        .register(&user.id, &assistant_turn_id, &active.id);
    let worker = match outcome {
        RegisterOutcome::Registered { worker } => worker,
        RegisterOutcome::Busy { .. } => {
            return sse_error_response(
                "A response is still streaming for this user — wait for it or hit stop.",
            );
        }
    };

    // Slot held. Any early-return from here must `state.chats.clear`
    // the worker so the next submit isn't permanently blocked.
    let user_turn =
        match chat::create_user_turn(&state.db, &active.id, &user_turn_id, &user_msg).await {
            Ok(t) => t,
            Err(err) => {
                state.chats.clear(&user.id, &worker);
                return sse_error_response(&err.to_string());
            }
        };
    // Auto-title on the first user turn. Two-stage so the sidebar
    // never sits on "Untitled chat" for long:
    //   1. Immediately persist a heuristic title (the user message,
    //      single-lined and truncated) so the row has something to
    //      show in the time it takes the model to respond.
    //   2. Spawn a background LLM call that asks for a tight 3-6 word
    //      title and overwrites the heuristic when it lands (~hundreds
    //      of ms typically).
    // Both stages push a `TurnUpdate::SidebarChanged` through the
    // worker's broadcast — the heuristic one fires synchronously below
    // (right after the assistant turn insert), the LLM-gen one fires
    // inside `generate_session_title` if the worker is still live.
    let auto_titled = active.title.is_none();
    if auto_titled {
        // Title from the user-typed prefix only — attachment markers
        // would make a noisy sidebar title.
        let fallback = first_message_title(&submit.user_text);
        let _ = chat::set_session_title(&state.db, &active.id, &fallback).await;
    }
    let assistant_turn = match chat::create_assistant_turn_in_progress(
        &state.db,
        &active.id,
        &assistant_turn_id,
        &submit.model,
    )
    .await
    {
        Ok(t) => t,
        Err(err) => {
            state.chats.clear(&user.id, &worker);
            return sse_error_response(&err.to_string());
        }
    };
    let _ = chat::touch_session(&state.db, &active.id).await;

    // Subscribe to the broadcast BEFORE spawning anything that
    // produces. If the worker (or the title-gen task below) lands a
    // message before we subscribe, the receiver misses it — broadcast
    // channels don't replay.
    let broadcast_rx = worker.broadcast.subscribe();

    // Push the heuristic-titled sidebar row update into the broadcast
    // *now* (synchronously, before any other tasks can send) so the
    // forwarding subscriber's first action after the initial bubble
    // append is to repaint the sidebar row with the new title. Without
    // this the sidebar would sit on "Untitled chat" until LLM-gen
    // lands — which might race the worker's Finalized and miss the
    // window.
    if auto_titled {
        let _ = worker.broadcast.send(TurnUpdate::SidebarChanged);
    }

    spawn_assistant_worker(
        &state,
        &user,
        &active.id,
        &assistant_turn_id,
        &submit.model,
        &worker,
        RequestCtx { client_ip, secure },
    )
    .await;

    // Background LLM call that names the conversation. If the worker
    // is still live when this lands, the title-gen task broadcasts a
    // second `SidebarChanged` with the better name; otherwise the
    // user sees it on their next page interaction.
    if auto_titled {
        tokio::spawn(title::generate_session_title(
            state.clone(),
            user.id.clone(),
            active.id.clone(),
            submit.user_text.clone(),
            submit.model.clone(),
        ));
    }

    // Initial SSE event: append the two new bubbles to the
    // conversation.
    let initial_html = format!(
        "{}{}",
        session_core::render::render_user_turn(&user_turn, Some("/chat")),
        session_core::render::render_assistant_turn(
            &chat::TurnWithTools {
                turn: assistant_turn.clone(),
                tool_calls: Vec::new(),
            },
            Some("/chat")
        )
    );
    let initial_patch = sse_patch(Some("#conversation"), Some("append"), &initial_html);

    spawn_session_stream_response(
        state.db.clone(),
        active.id.clone(),
        assistant_turn.id.clone(),
        broadcast_rx,
        Some(initial_patch),
        gateway_sidebar_emitter(state.clone(), user.id.clone(), active.id.clone()),
        Some("/chat".to_string()),
    )
}

// ---------------------------------------------------------------------------
// GET /chat/{id}/tail — attach to whatever worker is running for this
// user + session.

pub async fn chat_tail(
    Path(session_id): Path<String>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    // Confirm the session is readable (owned or shared) + that there's
    // actually a live worker for THIS session. Workers are keyed by the
    // owner's id, so a shared-chat viewer finds none below and just gets the
    // "not streaming" signal — they see the static snapshot, not live tokens.
    match chat::get_session_readable(&state.db, &user.id, &session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return empty_sse_response(),
        Err(err) => return sse_error_response(&err.to_string()),
    };
    let worker = match state.chats.get(&user.id) {
        Some(w) if w.session_id == session_id => w,
        _ => {
            // Nothing live for this session right now. Tell the client
            // to flip its streaming flag off (defensive: if it had
            // optimistically set $chatStreaming = true and the server
            // already finished, this corrects the local state) and
            // close.
            return sse_response(&[sse_signals(r#"{"chatStreaming":false}"#)]);
        }
    };

    let assistant_turn_id = worker.turn_id.clone();
    let broadcast_rx = worker.broadcast.subscribe();
    spawn_session_stream_response(
        state.db.clone(),
        session_id.clone(),
        assistant_turn_id,
        broadcast_rx,
        None,
        gateway_sidebar_emitter(state.clone(), user.id.clone(), session_id),
        Some("/chat".to_string()),
    )
}

// ---------------------------------------------------------------------------
// POST /chat/{id}/cancel — flip the cancel flag.

pub async fn chat_cancel(
    Path(session_id): Path<String>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    chat_cancel_turn(&state.chats, &user.id, &session_id);
    empty_sse_response()
}

// ---------------------------------------------------------------------------
// POST /chat/{id}/turns/{turn_id}/retry  and  …/edit
//
// Retry re-generates an assistant reply; edit rewrites a user message
// and re-generates from it. Both drop the target turn's downstream
// turns (everything below the regeneration point) and re-run the model
// with the currently-selected model. Reuses the same worker machinery
// as a fresh message via `start_regeneration`.

#[derive(serde::Deserialize)]
pub struct TurnPath {
    id: String,
    turn_id: String,
}

#[derive(serde::Deserialize)]
struct RetryForm {
    model: String,
}

#[derive(serde::Deserialize)]
struct EditForm {
    model: String,
    message: String,
}

/// Confirm the session belongs to the caller, then return the target
/// turn. `Err` is a ready-to-return SSE error response.
async fn load_owned_turn(
    state: &RamaState,
    user: &User,
    session_id: &str,
    turn_id: &str,
) -> Result<chat::Turn, Response> {
    match chat::get_session(&state.db, &user.id, session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return Err(sse_error_response("Conversation not found.")),
        Err(err) => return Err(sse_error_response(&err.to_string())),
    }
    match chat::get_turn(&state.db, session_id, turn_id).await {
        Ok(Some(t)) => Ok(t),
        Ok(None) => Err(sse_error_response("Message not found.")),
        Err(err) => Err(sse_error_response(&err.to_string())),
    }
}

pub async fn chat_retry(
    Path(TurnPath { id, turn_id }): Path<TurnPath>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let client_ip = crate::server::geoip::client_ip(req.headers())
        .or_else(|| crate::server::geoip::peer_ip(&req));
    let secure =
        crate::server::geoip::transport_is_secure(req.headers(), &state.config.gateway.public_url);
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return sse_error_response(&msg),
    };
    let form: RetryForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => return sse_error_response(&format!("malformed form: {err}")),
    };

    let turn = match load_owned_turn(&state, &user, &id, &turn_id).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    if turn.role != chat::TurnRole::Assistant {
        return sse_error_response("Retry applies to assistant replies.");
    }
    // Drop this reply + everything below, then regenerate from the
    // preceding user turn.
    if let Err(err) = chat::delete_turns_from_seq(&state.db, &id, turn.seq).await {
        return sse_error_response(&err.to_string());
    }
    start_regeneration(
        state,
        user,
        id,
        form.model,
        RequestCtx { client_ip, secure },
    )
    .await
}

pub async fn chat_edit(
    Path(TurnPath { id, turn_id }): Path<TurnPath>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let client_ip = crate::server::geoip::client_ip(req.headers())
        .or_else(|| crate::server::geoip::peer_ip(&req));
    let secure =
        crate::server::geoip::transport_is_secure(req.headers(), &state.config.gateway.public_url);
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return sse_error_response(&msg),
    };
    let form: EditForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => return sse_error_response(&format!("malformed form: {err}")),
    };
    let new_text = form.message.trim();
    if new_text.is_empty() {
        return sse_error_response("Message must not be empty.");
    }

    let turn = match load_owned_turn(&state, &user, &id, &turn_id).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    if turn.role != chat::TurnRole::User {
        return sse_error_response("Edit applies to your own messages.");
    }
    // Rewrite the message, drop everything below it, regenerate.
    if let Err(err) = chat::update_user_turn_content(&state.db, &id, &turn_id, new_text).await {
        return sse_error_response(&err.to_string());
    }
    if let Err(err) = chat::delete_turns_from_seq(&state.db, &id, turn.seq + 1).await {
        return sse_error_response(&err.to_string());
    }
    start_regeneration(
        state,
        user,
        id,
        form.model,
        RequestCtx { client_ip, secure },
    )
    .await
}

/// Request-derived bits the worker needs that aren't part of the chat
/// session itself: the caller's source IP (for GeoIP) and whether the
/// browser is on a secure context (so a precise-location prompt can even
/// succeed). Bundled so the worker/regeneration signatures stay legible.
struct RequestCtx {
    client_ip: Option<String>,
    secure: bool,
}

/// Build the caller's tool context + allowed-tool set and spawn the
/// per-turn worker that drives `assistant_turn_id`, clearing the
/// registry slot on exit. The single home for the worker/driver wiring
/// shared by the message-send and retry/edit (regeneration) paths — the
/// caller owns the worker registration, the assistant-turn row, and the
/// SSE response framing; this owns everything between.
async fn spawn_assistant_worker(
    state: &Arc<RamaState>,
    user: &User,
    session_id: &str,
    assistant_turn_id: &str,
    model: &str,
    worker: &session_core::workers::ActiveWorker,
    req: RequestCtx,
) {
    // Per-conversation tool overlay. The driver re-resolves the allowed-tool
    // set per round via `allowed_tools_for_session` (core ∪ this-conversation's
    // enabled, intersected with the user's RBAC grant), so a mid-turn
    // `enable_tools` call by the model surfaces the new schemas on the next
    // round. The chat path always goes through this overlay; the proxy path
    // uses the unfiltered per-user set.
    let tool_ctx = crate::server::tools::ToolContext {
        user_id: user.id.clone(),
        roles: user.roles.clone(),
        db: state.db.clone(),
        s3: state
            .config
            .chat
            .s3
            .as_ref()
            .map(|cfg| std::sync::Arc::new(cfg.clone())),
        assistant_turn_id: Some(assistant_turn_id.to_string()),
        session_id: Some(session_id.to_string()),
        client_ip: req.client_ip,
        geoip: state.geoip.clone(),
        // Chat path: hand the tool the live turn's broadcast + the
        // feedback hub so `get_user_location` can prompt the browser
        // for a precise position and wait for the reply.
        chat_feedback: Some(crate::server::tools::ChatFeedback {
            broadcast: worker.broadcast.clone(),
            hub: state.location_feedback.clone(),
            secure: req.secure,
        }),
        // Fresh per-turn set so concurrent uploaders (typst,
        // upload_attachment) serialize their filename picks and
        // each get a unique S3 key — see ToolContext docs.
        attachment_reservations: Some(crate::server::chat_attachments::new_reservation_set()),
        indexer: state.indexer.clone(),
    };
    let driver = Box::new(crate::openai_driver::OpenAiDriver {
        state: state.clone(),
        tool_ctx,
    });
    let driver_ctx = session_core::driver::SessionContext {
        user_id: Some(user.id.clone()),
        session_id: session_id.to_string(),
        assistant_turn_id: assistant_turn_id.to_string(),
        model: model.to_string(),
        cancel: worker.cancel.clone(),
        broadcast: worker.broadcast.clone(),
    };
    let worker_state = state.clone();
    let worker_for_task = worker.clone();
    let user_id_for_clear = user.id.clone();
    let pool_for_worker = state.db.clone();
    tokio::spawn(async move {
        session_core::worker::run_session_turn(pool_for_worker, driver, driver_ctx).await;
        worker_state
            .chats
            .clear(&user_id_for_clear, &worker_for_task);
    });
}

/// Spawn a fresh assistant turn for the (already-truncated) session and
/// stream it, re-rendering the conversation in place so dropped bubbles
/// disappear. Shared by retry + edit; mirrors the worker-spawn tail of
/// `chat_message_send`.
async fn start_regeneration(
    state: Arc<RamaState>,
    user: User,
    session_id: String,
    model: String,
    req: RequestCtx,
) -> Response {
    let assistant_turn_id = uuid::Uuid::new_v4().to_string();
    let worker = match state
        .chats
        .register(&user.id, &assistant_turn_id, &session_id)
    {
        RegisterOutcome::Registered { worker } => worker,
        RegisterOutcome::Busy { .. } => {
            return sse_error_response(
                "A response is still streaming for this user — wait for it or hit stop.",
            );
        }
    };
    let assistant_turn = match chat::create_assistant_turn_in_progress(
        &state.db,
        &session_id,
        &assistant_turn_id,
        &model,
    )
    .await
    {
        Ok(t) => t,
        Err(err) => {
            state.chats.clear(&user.id, &worker);
            return sse_error_response(&err.to_string());
        }
    };
    let _ = chat::touch_session(&state.db, &session_id).await;

    let broadcast_rx = worker.broadcast.subscribe();
    spawn_assistant_worker(
        &state,
        &user,
        &session_id,
        &assistant_turn_id,
        &model,
        &worker,
        req,
    )
    .await;

    // Re-render the (truncated) conversation in place: this removes the
    // dropped bubbles and shows the fresh in-progress assistant bubble.
    // `inner` keeps the `#conversation` element (and its scroll/tail
    // `data-init`) intact rather than re-triggering it.
    let turns = chat::list_turns(&state.db, &session_id)
        .await
        .unwrap_or_default();
    let mut inner = String::new();
    for t in &turns {
        inner.push_str(&session_core::render::render_turn(t, Some("/chat")).to_string());
    }
    let initial_patch = sse_patch(Some("#conversation"), Some("inner"), &inner);

    spawn_session_stream_response(
        state.db.clone(),
        session_id.clone(),
        assistant_turn.id.clone(),
        broadcast_rx,
        Some(initial_patch),
        gateway_sidebar_emitter(state.clone(), user.id.clone(), session_id),
        Some("/chat".to_string()),
    )
}

// ---------------------------------------------------------------------------
// Sidebar emitter glue.
//
// The shared streaming loop in `session_core::chat::spawn_session_stream_response`
// invokes a per-binary callback whenever a `TurnUpdate::SidebarChanged`
// arrives. The gateway's sidebar is the chat-list — repatch the
// session row whose title just changed so the new title appears
// without waiting for the user's next nav.

fn gateway_sidebar_emitter(
    state: Arc<RamaState>,
    user_id: String,
    session_id: String,
) -> SidebarEmitter {
    use rama::futures::sink::SinkExt;

    Box::new(move |mut tx: SseTx| {
        let state = state.clone();
        let user_id = user_id.clone();
        let session_id = session_id.clone();
        Box::pin(async move {
            let session = match chat::get_session(&state.db, &user_id, &session_id).await {
                Ok(Some(s)) => s,
                Ok(None) => return Ok(tx),
                Err(err) => {
                    tracing::warn!(error = %err, "chat stream: get_session for sidebar patch failed");
                    return Ok(tx);
                }
            };
            let sidebar = SidebarSession {
                id: session.id.clone(),
                title: session.title,
            };
            let html = super::render_sidebar_session(&sidebar, Some(&session.id)).to_string();
            let selector = format!("#session-row-{session_id}");
            let patch = sse_patch(Some(&selector), Some("outer"), &html);
            if tx.send(Ok(patch)).await.is_err() {
                Err(())
            } else {
                Ok(tx)
            }
        })
    })
}

/// Truncated first user message → session title. Trimmed to one line,
/// at most 64 chars, plus an ellipsis when truncated.
fn first_message_title(msg: &str) -> String {
    const MAX: usize = 64;
    let single_line: String = msg
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(MAX)
        .collect();
    if msg.chars().count() > MAX {
        format!("{single_line}…")
    } else {
        single_line
    }
}

async fn list_transcription_models(state: &RamaState) -> Vec<String> {
    state
        .upstreams
        .models_for_kind(crate::server::upstreams::PoolKind::Transcription)
}

async fn list_chat_models(state: &RamaState) -> Vec<render::ChatModelOption> {
    state
        .upstreams
        .models_with_compliance_for_kind(crate::server::upstreams::PoolKind::Chat)
        .into_iter()
        .map(|(id, c)| render::ChatModelOption {
            id,
            gdpr: c.gdpr,
            nda: c.nda,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Multipart parsing for /chat/{id}/messages
//
// The composer posts `multipart/form-data` with three named parts:
//   - `model`       text
//   - `message`     text (user-typed prose)
//   - `attachment`  file, repeated 0..N times
//
// Each `attachment` part is uploaded to S3 immediately (so we can
// reference the public URL from the user_text marker) and the raw
// bytes are then dropped — we don't keep them in memory past the
// upload.

struct ChatSubmit {
    model: String,
    user_text: String,
    attachments: Vec<UploadedAttachment>,
}

struct UploadedAttachment {
    outcome: chat_attachments::UploadOutcome,
}

async fn parse_chat_submit(
    content_type: &str,
    body: rama::bytes::Bytes,
    turn_id: &str,
    state: &RamaState,
) -> Result<ChatSubmit, String> {
    let boundary = multer::parse_boundary(content_type).map_err(|err| {
        format!(
            "expected multipart/form-data submit (the composer should set \
             enctype=\"multipart/form-data\"): {err}"
        )
    })?;
    let stream =
        rama::futures::stream::once(async move { Ok::<_, std::convert::Infallible>(body) });
    let mut mp = multer::Multipart::new(stream, boundary);

    let mut model: Option<String> = None;
    let mut user_text = String::new();
    let mut attachments: Vec<UploadedAttachment> = Vec::new();

    while let Some(field) = mp.next_field().await.map_err(|e| e.to_string())? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "model" => {
                model = Some(field.text().await.map_err(|e| e.to_string())?);
            }
            "message" => {
                user_text = field.text().await.map_err(|e| e.to_string())?;
            }
            "attachment" => {
                // Browsers always emit the `attachment` part for the
                // hidden `<input type="file">` even when no file was
                // picked — `filename=""` + zero bytes. Skip those so
                // a plain-text send doesn't fail upload validation.
                let filename = field.file_name().map(str::to_string).unwrap_or_default();
                let mime = field
                    .content_type()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                let bytes = field.bytes().await.map_err(|e| e.to_string())?.to_vec();
                if filename.is_empty() && bytes.is_empty() {
                    continue;
                }
                let cfg = state.config.chat.s3.as_ref().ok_or_else(|| {
                    "chat attachments are not configured (set [chat.s3] \
                         in gateway.toml)"
                        .to_string()
                })?;
                let outcome = chat_attachments::upload(cfg, turn_id, &filename, &mime, bytes)
                    .await
                    .map_err(|e| format!("upload `{filename}`: {e}"))?;
                attachments.push(UploadedAttachment { outcome });
            }
            _ => {
                // Ignore unknown fields — datastar may emit a few
                // bookkeeping bits that we don't care about.
            }
        }
    }

    let model = model
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "no model selected".to_string())?;
    Ok(ChatSubmit {
        model,
        user_text: user_text.trim().to_string(),
        attachments,
    })
}

/// Build the final `user_text` that we persist into `chat_turns`.
/// Layout:
///
///   <user-typed text>
///
///   [gw-attachment file="…" mime="…" url="…" size=N]
///   …
///
/// The marker is the only thing that goes into `user_text` — no
/// fenced-block inlining of text contents, since the LLM payload
/// rewrites the marker to an opaque-id stub anyway and the model
/// fetches bytes on demand via `fetch_attachment`. Inlining would
/// just bloat the persisted row without anyone reading it (the
/// chat-bubble renderer skips the fenced block via `split_markers`).
fn augment_user_text(turn_id: &str, submit: &ChatSubmit) -> String {
    let mut out = submit.user_text.clone();
    for att in &submit.attachments {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&chat_attachments::marker_line(turn_id, &att.outcome));
    }
    out
}

// Attachment URLs are baked into each marker at write time by
// `chat_attachments::marker_line` — see that module for the
// `proxy_url(turn_id, filename)` helper. The renderer just reads
// `att.url` and drops it straight into `<img src>` / chip hrefs.
// The S3 bucket is never reached directly from a browser or
// upstream LLM; bytes always stream through the gateway with
// session + turn-ownership checks applied first. The original
// "unauthenticated egress" concern that motivated the
// presign-everywhere design is gone: the proxy route requires the
// session cookie AND verifies the turn belongs to the cookie
// holder.

// ---------------------------------------------------------------------------
// GET /chat/{id}/export.md  and  GET /chat/{id}/export.pdf
//
// Download the whole conversation as a self-contained document. Both
// formats share the same gate as the chat view (owner OR shared) and the
// same body builder in `session_core::export`; only the serialization and
// the response headers differ. The Markdown path is pure-Rust; the PDF
// path shells out to the bundled `typst` CLI (the same engine the letter
// templates use).

/// GET /chat/{id}/export.md
pub async fn chat_export_markdown(
    Path(session_id): Path<String>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (session, turns) = match load_exportable_chat(&state, &session_id, &req).await {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };
    let opts = export::ExportOpts {
        base_url: &state.config.gateway.public_url,
    };
    let body = export::to_markdown(&session, &turns, &opts);
    download_response(
        "text/markdown; charset=utf-8",
        &export_filename(&session, "md"),
        body.into_bytes(),
    )
}

/// GET /chat/{id}/export.pdf
pub async fn chat_export_pdf(
    Path(session_id): Path<String>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (session, turns) = match load_exportable_chat(&state, &session_id, &req).await {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };
    let opts = export::ExportOpts {
        base_url: &state.config.gateway.public_url,
    };
    let source = export::to_typst(&session, &turns, &opts);
    match crate::server::typst::compile_source(&source).await {
        Ok(pdf) => download_response("application/pdf", &export_filename(&session, "pdf"), pdf),
        Err(crate::server::typst::CompileError::BinaryNotFound) => export_error(
            rama::http::StatusCode::SERVICE_UNAVAILABLE,
            "PDF export unavailable: the typst CLI is not installed on the gateway",
        ),
        Err(err) => {
            tracing::error!(error = %err, %session_id, "chat PDF export compile");
            export_error(
                rama::http::StatusCode::INTERNAL_SERVER_ERROR,
                "PDF export failed",
            )
        }
    }
}

/// Shared loader for the export handlers: authenticate, then fetch the
/// session (owner OR shared) and its turns. Mirrors `chat_session_view`'s
/// readability gate so a shared conversation is exportable by a viewer
/// while a private one stays owner-only.
async fn load_exportable_chat(
    state: &Arc<RamaState>,
    session_id: &str,
    req: &Request,
) -> Result<(chat::Session, Vec<chat::TurnWithTools>), Response> {
    let (_session, user) = require_session_or_redirect(state, req).await?;
    let session = match chat::get_session_readable(&state.db, &user.id, session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return Err(see_other("/chat")),
        Err(err) => return Err(internal_error_html(&user.email, &err.to_string())),
    };
    let turns = match chat::list_turns(&state.db, &session.id).await {
        Ok(t) => t,
        Err(err) => return Err(internal_error_html(&user.email, &err.to_string())),
    };
    Ok((session, turns))
}

/// Build an attachment download response with the right headers.
fn download_response(content_type: &str, filename: &str, bytes: Vec<u8>) -> Response {
    Response::builder()
        .status(rama::http::StatusCode::OK)
        .header(rama::http::header::CONTENT_TYPE, content_type)
        .header(rama::http::header::CONTENT_LENGTH, bytes.len())
        .header(
            rama::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        // Exports reflect live conversation state — never cache.
        .header(rama::http::header::CACHE_CONTROL, "no-store")
        .body(bytes.into())
        .unwrap_or_else(|err| {
            tracing::error!(error = %err, "export response build");
            export_error(
                rama::http::StatusCode::INTERNAL_SERVER_ERROR,
                "response build",
            )
        })
}

fn export_error(status: rama::http::StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header(
            rama::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(msg.to_string().into())
        .unwrap()
}

/// `<slug>.<ext>` download filename derived from the session title, with
/// a stable fallback so an untitled chat still produces a sane name.
fn export_filename(session: &chat::Session, ext: &str) -> String {
    let slug = slugify(session.title.as_deref().unwrap_or(""));
    let stem = if slug.is_empty() {
        let short = session.id.split('-').next().unwrap_or(&session.id);
        format!("chat-{short}")
    } else {
        slug
    };
    format!("{stem}.{ext}")
}

/// Lowercase ASCII slug: alnum kept, every other run collapsed to a
/// single `-`, trimmed, capped so a long title can't blow up the header.
fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed.chars().take(60).collect::<String>()
}

// ---------------------------------------------------------------------------
// GET /chat/attachment/{turn_id}/{filename} — bytes for one attachment.

#[derive(serde::Deserialize)]
pub struct AttachmentPath {
    pub turn_id: String,
    pub filename: String,
}

/// Stream one attachment's bytes through the gateway, gated by the
/// session cookie + a check that the turn belongs to the caller's
/// user. Bucket never sees a browser request; the LLM never sees a
/// presigned URL.
pub async fn chat_attachment(
    Path(AttachmentPath { turn_id, filename }): Path<AttachmentPath>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    // 401 (not redirect) — `<img src>` will just show broken-image
    // if the cookie went bad, and a 401 is honest in operator logs.
    let session = match state.sessions.lookup_from_headers(req.headers()).await {
        Ok(Some(s)) => s,
        _ => return attachment_error(rama::http::StatusCode::UNAUTHORIZED, "auth required"),
    };
    // Readable = the turn's session is owned by the caller OR shared. Mirrors
    // the chat-view gate so attachments in a shared conversation are
    // fetchable by a viewer, while a private turn's files stay owner-only.
    // 404 (not 403) on miss/denied — don't leak whether the turn exists.
    match session_core::db::turn_session_readable(&state.db, &turn_id, &session.user_id).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                requester = %session.user_id, %turn_id,
                "rejected attachment fetch (not owner, not shared)",
            );
            return attachment_error(rama::http::StatusCode::NOT_FOUND, "no such turn");
        }
        Err(err) => {
            tracing::warn!(error = %err, "turn_session_readable");
            return attachment_error(rama::http::StatusCode::INTERNAL_SERVER_ERROR, "db error");
        }
    }
    let Some(cfg) = state.config.chat.s3.as_ref() else {
        return attachment_error(
            rama::http::StatusCode::SERVICE_UNAVAILABLE,
            "chat attachments not configured",
        );
    };
    let fetched = match chat_attachments::fetch(cfg, &turn_id, &filename).await {
        Ok(f) => f,
        Err(chat_attachments::AttachmentError::BadFilename(_)) => {
            return attachment_error(rama::http::StatusCode::BAD_REQUEST, "bad filename");
        }
        Err(err) => {
            tracing::warn!(error = %err, %turn_id, %filename, "attachment fetch");
            return attachment_error(rama::http::StatusCode::NOT_FOUND, "not found");
        }
    };
    Response::builder()
        .status(rama::http::StatusCode::OK)
        .header(rama::http::header::CONTENT_TYPE, fetched.mime)
        .header(rama::http::header::CONTENT_LENGTH, fetched.bytes.len())
        // Content-addressed: <turn_id> is a UUID, filename is fixed
        // for that turn — the bytes can't change. 1 h max-age keeps
        // a viewing session cheap; not `immutable` so future
        // delete/replace semantics don't get cache-pinned forever.
        .header(rama::http::header::CACHE_CONTROL, "private, max-age=3600")
        .body(fetched.bytes.into())
        .unwrap_or_else(|err| {
            tracing::error!(error = %err, "attachment response build");
            attachment_error(
                rama::http::StatusCode::INTERNAL_SERVER_ERROR,
                "response build",
            )
        })
}

fn attachment_error(status: rama::http::StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header(
            rama::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(msg.to_string().into())
        .unwrap()
}
