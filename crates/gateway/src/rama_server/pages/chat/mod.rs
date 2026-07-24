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
    NavSections, Theme, is_datastar_request, read_body_to_bytes, see_other, sse_patch,
    sse_response, sse_signals, sse_toast,
};
use session_core::i18n::{self, Lang, t, t_args};
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
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
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
            theme,
            lang,
            nav,
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
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
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
        theme,
        lang,
        nav,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn render_chat_response(
    state: Arc<RamaState>,
    user: &User,
    active: chat::Session,
    datastar: bool,
    impersonating: bool,
    theme: Theme,
    lang: Lang,
    nav: NavSections,
) -> Response {
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
    let access = state.pool_access_for(&user.roles);
    let models = list_chat_models(&state, &access).await;
    let transcription_models = list_transcription_models(&state, &access).await;
    // Effort + capability menu are owner-only (a read-only viewer has no
    // composer to attach them to): a default + empty set keeps the render
    // cheap for that path.
    let (effort, capabilities) = if read_only {
        (crate::server::reasoning::Effort::Standard, Vec::new())
    } else {
        let effort = crate::server::reasoning::Effort::from_db(
            crate::server::db::chat_session_settings::get_effort(&state.db, &active.id)
                .await
                .ok()
                .flatten()
                .as_deref(),
        );
        (
            effort,
            build_capabilities(&state, user, &active.id, lang).await,
        )
    };
    // The session's document canvas (active = most-recently-updated
    // document), pre-rendered to HTML for the always-present slot. `None`
    // when the conversation has no documents.
    let document_canvas_html =
        crate::server::tools::document::render_canvas_html(&state.db, &active.id, None, None, lang)
            .await
            .ok()
            .flatten();
    let assets = match chat_attachments::list_session_attachments(&state.db, &active.id).await {
        Ok(assets) => assets,
        Err(err) => {
            tracing::warn!(error = %err, session_id = %active.id, "list chat assets");
            Vec::new()
        }
    };
    // Compaction cutoff, if this conversation has been compacted — drives the
    // transcript's "earlier messages condensed" divider. A read error degrades
    // to "no divider".
    let compacted_up_to_seq = crate::server::db::chat_compactions::get(&state.db, &active.id)
        .await
        .ok()
        .flatten()
        .map(|c| c.up_to_seq);
    let body = render::render_chat_page(render::ChatPage {
        active: &active,
        turns: &turns,
        in_flight_turn_id: in_flight_turn_id.as_deref(),
        models: &models,
        transcription_models: &transcription_models,
        error_msg: None,
        read_only,
        shared: active.shared,
        effort,
        capabilities: &capabilities,
        document_canvas_html: document_canvas_html.as_deref(),
        assets: &assets,
        compacted_up_to_seq,
        // The full voice loop needs both a TTS (speech) pool and a
        // transcription model. Owner only — a read-only shared viewer has no
        // composer to attach it to.
        voice_available: !read_only
            && state.upstreams.has_speech()
            && !transcription_models.is_empty(),
        lang,
    });
    let chat_sidebar = SidebarChat {
        sessions: sessions
            .into_iter()
            .map(|s| SidebarSession {
                id: s.id,
                title: s.title,
                pinned: s.pinned,
            })
            .collect(),
        active_session_id: Some(active.id.clone()),
    };
    let title = active
        .title
        .clone()
        .unwrap_or_else(|| t(lang, "chat-default-title"));
    let url = format!("/chat/{}", active.id);
    if datastar {
        nav_or_html_page(
            true,
            theme,
            lang,
            nav,
            NavItem::Chat,
            &format!("{title} — LLM Gateway"),
            &user.email,
            is_admin(&state, user),
            state.user_skills_enabled(),
            impersonating,
            body,
            &url,
            &chat_sidebar,
        )
    } else {
        html_authed_page(
            theme,
            lang,
            nav,
            Some(NavItem::Chat),
            &format!("{title} — LLM Gateway"),
            &user.email,
            is_admin(&state, user),
            state.user_skills_enabled(),
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
            Theme::from_headers(req.headers()),
            Lang::from_headers(req.headers()),
            NavSections::from_headers(req.headers()),
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
    let lang = Lang::from_headers(req.headers());
    let deleted = match chat::delete_session(&state.db, &user.id, &session_id).await {
        Ok(v) => v,
        Err(err) => return internal_error_html(&user.email, &err.to_string()),
    };
    if !deleted {
        return sse_response(&[sse_toast(&super::Flash {
            kind: super::FlashKind::Info,
            message: t(lang, "chat-toast-conversation-already-gone"),
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
        Theme::from_headers(req.headers()),
        lang,
        NavSections::from_headers(req.headers()),
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
    let lang = Lang::from_headers(req.headers());
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
    let control = render::render_share_control(&share_url, now_shared, lang).to_string();
    let toast = if now_shared {
        sse_toast(&super::Flash {
            kind: super::FlashKind::Success,
            message: t(lang, "chat-toast-share-copied"),
        })
    } else {
        sse_toast(&super::Flash {
            kind: super::FlashKind::Info,
            message: t(lang, "chat-toast-share-stopped"),
        })
    };
    sse_response(&[
        sse_patch(Some("#share-toggle"), Some("outer"), &control),
        toast,
    ])
}

// ---------------------------------------------------------------------------
// POST /chat/{id}/pin — owner toggles the conversation's pinned flag. Pinned
// conversations float to the top of the sidebar list. Pure UI affordance —
// pinning never changes who can read the session.

#[derive(serde::Deserialize)]
struct PinForm {
    /// The id of the currently-open conversation (the active sidebar row),
    /// forwarded so the re-rendered list can keep its highlight. Empty when
    /// the user pinned from a page with no open chat.
    #[serde(default)]
    active: String,
}

pub async fn chat_session_pin(
    Path(session_id): Path<String>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let datastar = is_datastar_request(req.headers());
    let lang = Lang::from_headers(req.headers());
    // Owner-only on both reads and writes: get_session is owner-scoped, and
    // set_pinned's UPDATE is `WHERE id = ? AND user_id = ?`. A non-owner POST
    // finds no session and is redirected away with no effect.
    let current = match chat::get_session(&state.db, &user.id, &session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return see_other("/chat"),
        Err(err) => return internal_error_html(&user.email, &err.to_string()),
    };
    let now_pinned = !current.pinned;
    if let Err(err) = chat::set_pinned(&state.db, &user.id, &session_id, now_pinned).await {
        return internal_error_html(&user.email, &err.to_string());
    }
    let form: PinForm = match super::read_form(req.into_body()).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let active = (!form.active.is_empty()).then_some(form.active);
    if !datastar {
        // No-JS fallback: full-page redirect back to the open chat (or the
        // index when pinning from elsewhere).
        return match &active {
            Some(id) => see_other(&format!("/chat/{id}")),
            None => see_other("/chat"),
        };
    }
    // datastar: re-render the whole conversation list. Pinning re-sorts it
    // (pinned rows float to the top), so a single-row patch wouldn't reflect
    // the move — patch the full `#session-list`.
    let sidebar = super::fetch_sidebar_chat(&state, &user.id, active).await;
    let list = super::render_session_list(
        &sidebar.sessions,
        sidebar.active_session_id.as_deref(),
        lang,
    )
    .to_string();
    let toast = if now_pinned {
        sse_toast(&super::Flash {
            kind: super::FlashKind::Success,
            message: t(lang, "chat-toast-pinned"),
        })
    } else {
        sse_toast(&super::Flash {
            kind: super::FlashKind::Info,
            message: t(lang, "chat-toast-unpinned"),
        })
    };
    sse_response(&[
        sse_patch(Some("#session-list"), Some("outer"), &list),
        toast,
    ])
}

// ---------------------------------------------------------------------------
// GET /chat/search — search across the user's conversations.

#[derive(serde::Deserialize)]
struct SearchQuery {
    q: String,
}

pub async fn chat_search(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let datastar = is_datastar_request(req.headers());
    let lang = Lang::from_headers(req.headers());

    let form: SearchQuery = match serde_urlencoded::from_str(req.uri().query().unwrap_or("")) {
        Ok(f) => f,
        Err(_) => SearchQuery { q: String::new() },
    };

    let hits = chat::search_sessions(&state.db, &user.id, &form.q, 50)
        .await
        .unwrap_or_default();

    if !datastar {
        // No-JS fallback: render a full results page in the main content
        // area (the query is echoed back so it doesn't look ignored). The
        // JS path never reaches here — it SSE-patches the sidebar list.
        let theme = Theme::from_headers(req.headers());
        let nav = NavSections::from_headers(req.headers());
        let chat = super::fetch_sidebar_chat(&state, &user.id, None).await;
        let body = super::render_search_page_body(&form.q, &hits, lang);
        return nav_or_html_page(
            false,
            theme,
            lang,
            nav,
            NavItem::Chat,
            &t(lang, "nav-search-title"),
            &user.email,
            is_admin(&state, &user),
            state.user_skills_enabled(),
            session.impersonator_id.is_some(),
            body,
            "/chat/search",
            &chat,
        );
    }

    let list = super::render_search_results(&hits, lang).to_string();
    sse_response(&[sse_patch(Some("#session-list"), Some("outer"), &list)])
}

// ---------------------------------------------------------------------------
// POST /chat/{id}/capabilities — owner pins/unpins a tool, MCP integration, or
// skill for this conversation (the composer's "+" menu). Writes the same
// per-conversation overlay the model drives via `enable_tools` / `read_skill`,
// with `source = "user"`, and re-patches the `#capabilities` region.

pub async fn chat_capabilities_toggle(
    Path(session_id): Path<String>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    // Owner-only: get_session is owner-scoped, so a non-owner POST finds no
    // session and bounces with no effect.
    if let Ok(None) | Err(_) = chat::get_session(&state.db, &user.id, &session_id).await {
        return see_other("/chat");
    }
    let datastar = is_datastar_request(req.headers());
    let lang = Lang::from_headers(req.headers());
    // The toggle button isn't inside a form (the composer is the only form on
    // the page), so the kind+key ride in the query string, not a body.
    let form: CapabilityForm = match serde_urlencoded::from_str(req.uri().query().unwrap_or("")) {
        Ok(f) => f,
        Err(err) => return internal_error_html(&user.email, &format!("malformed query: {err}")),
    };
    // Normalise the requested tier; bail on anything unexpected.
    let target = match form.state.as_deref() {
        Some("on") => render::ToolState::On,
        Some("off") => render::ToolState::Off,
        Some("auto") => render::ToolState::Auto,
        other => {
            return internal_error_html(
                &user.email,
                &format!("unknown capability state: {other:?}"),
            );
        }
    };

    // `group`/`all` fan a single click out across the affected rows; `tool`/
    // `skill` apply to one. Resolve the set from the *current* capability list
    // so a group's membership matches exactly what's on screen.
    let caps_before = build_capabilities(&state, &user, &session_id, lang).await;
    let targets: Vec<(render::CapKind, String)> = match form.kind.as_str() {
        "all" => caps_before
            .iter()
            .map(|c| (c.kind, c.key.clone()))
            .collect(),
        "group" => caps_before
            .iter()
            .filter(|c| render::group_slug(c.group) == form.key)
            .map(|c| (c.kind, c.key.clone()))
            .collect(),
        "skill" => vec![(render::CapKind::Skill, form.key.clone())],
        // "tool" (built-in group or MCP connector toggle key).
        _ => vec![(render::CapKind::Tool, form.key.clone())],
    };

    for (kind, key) in &targets {
        if let Err(err) = apply_capability_state(&state, &session_id, *kind, key, target).await {
            return internal_error_html(&user.email, &err.to_string());
        }
    }

    if !datastar {
        return see_other(&format!("/chat/{session_id}"));
    }
    // Patch only the volatile leaves — the segmented controls, the aggregate
    // pills, and the chips — never the section containers or the search box, so
    // the open/collapse and search signals on `#cap-wrap` survive untouched.
    let caps_after = build_capabilities(&state, &user, &session_id, lang).await;
    let patches = render::render_capability_patches(&session_id, &caps_after, lang);
    let events: Vec<_> = patches
        .iter()
        .map(|(sel, html)| sse_patch(Some(sel), Some("outer"), html))
        .collect();
    sse_response(&events)
}

/// Apply one tier to one capability, routing to the right overlay. Tools use
/// the tri-state `chat_session_tools` rows (On=`set(true)`, Off=`set(false)`,
/// Auto=`clear`); skills are two-state in `chat_session_skills` (On=record,
/// anything else=remove), so applying Off/Auto to a skill simply unloads it.
async fn apply_capability_state(
    state: &RamaState,
    session_id: &str,
    kind: render::CapKind,
    key: &str,
    target: render::ToolState,
) -> Result<(), crate::server::db::DbError> {
    use render::{CapKind, ToolState};
    match kind {
        CapKind::Skill => {
            if target == ToolState::On {
                crate::server::db::chat_session_skills::record(&state.db, session_id, key).await
            } else {
                crate::server::db::chat_session_skills::remove(&state.db, session_id, key).await
            }
        }
        CapKind::Tool => match target {
            ToolState::On => {
                crate::server::db::chat_session_tools::set(&state.db, session_id, key, true, "user")
                    .await
            }
            ToolState::Off => {
                crate::server::db::chat_session_tools::set(
                    &state.db, session_id, key, false, "user",
                )
                .await
            }
            ToolState::Auto => {
                crate::server::db::chat_session_tools::clear(&state.db, session_id, key).await
            }
        },
    }
}

#[derive(serde::Deserialize)]
struct CapabilityForm {
    kind: String,
    /// Toggle key for `tool`/`skill`, group slug for `group`, `"*"` for `all`.
    key: String,
    /// Requested tier: `on` | `off` | `auto`.
    state: Option<String>,
}

// ---------------------------------------------------------------------------
// POST /chat/{id}/effort — owner sets the conversation's "Denkaufwand". One
// knob driving both the reasoning budget and the tool-round cap.

pub async fn chat_effort_set(
    Path(session_id): Path<String>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Ok(None) | Err(_) = chat::get_session(&state.db, &user.id, &session_id).await {
        return see_other("/chat");
    }
    let datastar = is_datastar_request(req.headers());
    let lang = Lang::from_headers(req.headers());
    // The picker isn't a form (it lives inside the composer's form), so the
    // chosen level rides in the query string.
    let form: EffortForm = match serde_urlencoded::from_str(req.uri().query().unwrap_or("")) {
        Ok(f) => f,
        Err(err) => return internal_error_html(&user.email, &format!("malformed query: {err}")),
    };
    // Normalise through the enum so only known levels ever land in the DB.
    let effort = crate::server::reasoning::Effort::from_db(Some(&form.effort));
    if let Err(err) = crate::server::db::chat_session_settings::set_effort(
        &state.db,
        &session_id,
        effort.as_str(),
    )
    .await
    {
        return internal_error_html(&user.email, &err.to_string());
    }
    if !datastar {
        return see_other(&format!("/chat/{session_id}"));
    }
    sse_response(&[sse_toast(&super::Flash {
        kind: super::FlashKind::Success,
        message: t_args(
            lang,
            "chat-toast-effort-set",
            &i18n::args([("level", effort.label().to_string().into())]),
        ),
    })])
}

#[derive(serde::Deserialize)]
struct EffortForm {
    effort: String,
}

/// Build the high-level capability list for the composer's "+" menu — connected
/// MCP integrations + permitted skills, each tagged with whether this
/// conversation has it on. (Built-in tools are intentionally excluded; see the
/// body.)
async fn build_capabilities(
    state: &RamaState,
    user: &User,
    session_id: &str,
    lang: Lang,
) -> Vec<render::CapabilityRow> {
    use crate::server::tools::catalog::Category;
    use render::{CapKind, CapabilityRow, SKILL_ORDER, ToolState};

    let mut out: Vec<CapabilityRow> = Vec::new();
    // Per-conversation tri-state for every recorded tool key (present → on/off;
    // absent → Auto). One query covers On, Off, and Auto.
    let states = crate::server::db::chat_session_tools::states_for_session(&state.db, session_id)
        .await
        .unwrap_or_default();

    // Built-in tool groups the user's roles grant — the same grouped, de-noised
    // catalog the `/tools` page renders, carrying each group's plain-language
    // description. MCP connectors are dropped here (the `Integrations` rows
    // would be generic); the user's actual connected connectors are added below
    // with their real names + brand icons.
    for entry in crate::rama_server::pages::entries_for_roles(state, &user.roles) {
        if entry.category == Category::Integrations {
            continue;
        }
        let state_tier = ToolState::from_row(states.get(&entry.key).copied());
        out.push(CapabilityRow {
            key: entry.key,
            kind: CapKind::Tool,
            label: entry.title,
            description: entry.description,
            group: entry.category.label(),
            order: entry.category.order(),
            state: state_tier,
            icon: None,
        });
    }

    // Integrations: the connectors the caller has connected. One row per
    // connector, keyed `mcp__<connector>` — that single toggle governs every
    // tool the connector bridges (`entry_key_for` collapses `mcp__x__*` to
    // `mcp__x`), so enabling it exposes the whole integration to the model.
    // Only catalog-enabled connectors passing the `allowed_groups` gate appear.
    let role_ids = state.role_ids_for(&user.roles);
    let admin = state.rbac.is_admin(&role_ids);
    if let Ok(connected) = crate::server::db::user_mcp::connected_keys(&state.db, &user.id).await {
        for ck in connected {
            let Ok(Some(c)) = crate::server::db::mcp_catalog::get(&state.db, &ck).await else {
                continue;
            };
            if !c.enabled {
                continue;
            }
            if !c.allows(&role_ids, admin) {
                continue;
            }
            let key = format!("{}{ck}", crate::server::tools::mcp::MCP_ID_PREFIX);
            let state_tier = ToolState::from_row(states.get(&key).copied());
            let description = c.description.clone().unwrap_or_else(|| {
                t_args(
                    lang,
                    "chat-mcp-bridged-description",
                    &i18n::args([("name", c.name.clone().into())]),
                )
            });
            out.push(CapabilityRow {
                key,
                kind: CapKind::Tool,
                label: c.name,
                description,
                group: Category::Integrations.label(),
                order: Category::Integrations.order(),
                state: state_tier,
                icon: c.icon,
            });
        }
    }

    // Permitted skills (sticky once loaded). Two-state: On (loaded) / Auto (the
    // model may load it on demand). Label with the skill's human-readable title
    // (frontmatter `title`, else a prettified slug) rather than the bare slug.
    let loaded = crate::server::db::chat_session_skills::loaded_for_session(&state.db, session_id)
        .await
        .unwrap_or_default();
    let skill_reg = state.combined_skills_for(&user.id);
    for name in state.allowed_skills_for(&user.roles, &user.id) {
        let on = loaded.iter().any(|s| s == &name);
        let (label, description) = skill_reg
            .as_ref()
            .and_then(|r| r.get(&name))
            .map(|s| (s.title.clone(), s.description.clone()))
            .unwrap_or_else(|| (name.clone(), String::new()));
        out.push(CapabilityRow {
            key: name,
            kind: CapKind::Skill,
            label,
            description,
            group: "Skills",
            order: SKILL_ORDER,
            state: if on { ToolState::On } else { ToolState::Auto },
            icon: None,
        });
    }

    // Stable order: by group order, then label, so sections render
    // deterministically and the grouping pass sees contiguous runs.
    out.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.label.cmp(&b.label)));
    out
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
    let lang = Lang::from_headers(req.headers());

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
            message: t(lang, "chat-toast-already-in-your-chats"),
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
        render_chat_response(
            state.clone(),
            &user,
            new_session,
            true,
            impersonating,
            Theme::from_headers(req.headers()),
            lang,
            NavSections::from_headers(req.headers()),
        )
        .await
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
    let lang = Lang::from_headers(req.headers());

    // Make sure the user owns this session.
    let active = match chat::get_session(&state.db, &user.id, &session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return sse_error_response(&t(lang, "chat-error-conversation-not-found"));
        }
        Err(err) => return sse_error_response(&err.to_string()),
    };

    // Rate-limit / quota gate — before reserving a worker or touching the DB,
    // so an over-budget user is turned away cleanly (details on `/usage`).
    {
        let role_ids = state.role_ids_for(&user.roles);
        if state.enforcer.check(&user.id, &role_ids).await.is_err() {
            return sse_error_response(&t(lang, "chat-error-rate-limited"));
        }
    }

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
        return sse_error_response(&t(lang, "chat-error-message-empty"));
    }

    // Build the final user_text: typed text + per-attachment marker
    // (and an inlined fenced block for `text/*`-like attachments so
    // the model reads the bytes directly on the current turn).
    let user_msg = augment_user_text(&user_turn_id, &submit);
    // Per-turn voice-mode flag (drives the brevity directive in the driver).
    // Captured before `submit` is consumed below.
    let voice_mode = submit.voice;

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
            return sse_error_response(&t(lang, "chat-error-still-streaming"));
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
        RequestCtx {
            client_ip,
            secure,
            voice_mode,
        },
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
        session_core::render::render_user_turn(&user_turn, Some("/chat"), lang),
        session_core::render::render_assistant_turn(
            &chat::TurnWithTools {
                turn: assistant_turn.clone(),
                tool_calls: Vec::new(),
            },
            Some("/chat"),
            lang
        )
    );
    let initial_patch = sse_patch(Some("#conversation"), Some("append"), &initial_html);

    spawn_session_stream_response(
        state.db.clone(),
        active.id.clone(),
        assistant_turn.id.clone(),
        broadcast_rx,
        Some(initial_patch),
        gateway_sidebar_emitter(state.clone(), user.id.clone(), active.id.clone(), lang),
        Some("/chat".to_string()),
        lang,
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
    let lang = Lang::from_headers(req.headers());

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
        gateway_sidebar_emitter(state.clone(), user.id.clone(), session_id, lang),
        Some("/chat".to_string()),
        lang,
    )
}

// ---------------------------------------------------------------------------
// GET /chat/{id}/document/{doc_id} — render a document (optionally an older
// `?version=N`) into the canvas slot. This is the datastar `@get` target for
// the panel's document- and version-switchers: it returns a single SSE patch
// replacing `#document-canvas-slot`'s contents. Readable-gated like the tail
// (owner or a shared viewer); a missing doc/version closes with no change.

pub async fn chat_document_view(
    Path((session_id, doc_id)): Path<(String, String)>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match chat::get_session_readable(&state.db, &user.id, &session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return empty_sse_response(),
        Err(err) => return sse_error_response(&err.to_string()),
    };
    let version = req.uri().query().and_then(parse_version_query);
    let html = match crate::server::tools::document::render_canvas_html(
        &state.db,
        &session_id,
        Some(&doc_id),
        version,
        lang,
    )
    .await
    {
        Ok(Some(h)) => h,
        // No such document/version in this session, or a read error — leave
        // the panel as-is.
        _ => return empty_sse_response(),
    };
    sse_response(&[sse_patch(
        Some("#document-canvas-slot"),
        Some("inner"),
        &html,
    )])
}

/// Pull `version=N` out of a raw query string (`a=b&version=3`). `None`
/// when absent or unparseable, which the caller reads as "latest".
fn parse_version_query(q: &str) -> Option<i64> {
    q.split('&')
        .find_map(|kv| kv.strip_prefix("version="))
        .and_then(|v| v.parse().ok())
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
    lang: Lang,
) -> Result<chat::Turn, Response> {
    match chat::get_session(&state.db, &user.id, session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(sse_error_response(&t(
                lang,
                "chat-error-conversation-not-found",
            )));
        }
        Err(err) => return Err(sse_error_response(&err.to_string())),
    }
    match chat::get_turn(&state.db, session_id, turn_id).await {
        Ok(Some(t)) => Ok(t),
        Ok(None) => Err(sse_error_response(&t(lang, "chat-error-message-not-found"))),
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
    let lang = Lang::from_headers(req.headers());
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

    let turn = match load_owned_turn(&state, &user, &id, &turn_id, lang).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    if turn.role != chat::TurnRole::Assistant {
        return sse_error_response(&t(lang, "chat-error-retry-assistant-only"));
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
        RequestCtx {
            client_ip,
            secure,
            voice_mode: false,
        },
        lang,
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
    let lang = Lang::from_headers(req.headers());
    let client_ip = crate::server::geoip::client_ip(req.headers())
        .or_else(|| crate::server::geoip::peer_ip(&req));
    let secure =
        crate::server::geoip::transport_is_secure(req.headers(), &state.config.gateway.public_url);
    // Snapshot the content-type before consuming the request — the edit
    // form now posts `multipart/form-data` (so it can carry pasted/dropped
    // attachments, exactly like the composer); older cached clients may
    // still post plain urlencoded.
    let content_type = req
        .headers()
        .get(rama::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return sse_error_response(&msg),
    };

    // Verify ownership + role BEFORE parsing multipart — parsing uploads
    // any attachment bytes to S3 under this turn's prefix, and we won't do
    // that for a turn the caller doesn't own or that isn't theirs to edit.
    let turn = match load_owned_turn(&state, &user, &id, &turn_id, lang).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    if turn.role != chat::TurnRole::User {
        return sse_error_response(&t(lang, "chat-error-edit-own-messages-only"));
    }

    // The edited message text + any newly attached files. Multipart carries
    // attachments (uploaded here, their markers appended to the text);
    // urlencoded is the text-only fallback.
    let (model, new_text) = if content_type.starts_with("multipart/form-data") {
        let submit = match parse_chat_submit(&content_type, body, &turn_id, &state).await {
            Ok(s) => s,
            Err(msg) => return sse_error_response(&msg),
        };
        // `submit.user_text` already carries the existing content (incl. any
        // prior attachment markers the textarea preserved); append markers
        // for the freshly attached files. Build the text before moving
        // `submit.model` out (it borrows `submit`).
        let text = augment_user_text(&turn_id, &submit);
        (submit.model, text)
    } else {
        let form: EditForm = match serde_urlencoded::from_bytes(&body) {
            Ok(f) => f,
            Err(err) => return sse_error_response(&format!("malformed form: {err}")),
        };
        (form.model, form.message.trim().to_string())
    };
    if new_text.is_empty() {
        return sse_error_response(&t(lang, "chat-error-message-must-not-be-empty"));
    }

    // Rewrite the message, drop everything below it, regenerate.
    if let Err(err) = chat::update_user_turn_content(&state.db, &id, &turn_id, &new_text).await {
        return sse_error_response(&err.to_string());
    }
    if let Err(err) = chat::delete_turns_from_seq(&state.db, &id, turn.seq + 1).await {
        return sse_error_response(&err.to_string());
    }
    start_regeneration(
        state,
        user,
        id,
        model,
        RequestCtx {
            client_ip,
            secure,
            voice_mode: false,
        },
        lang,
    )
    .await
}

/// Path for `POST /chat/{id}/turns/{turn_id}/attachment/{filename}/remove`.
/// `filename` arrives percent-decoded (same as the `chat_attachment`
/// proxy route).
#[derive(serde::Deserialize)]
pub struct AttachmentRemovePath {
    pub id: String,
    pub turn_id: String,
    pub filename: String,
}

/// Remove a single attachment from a message: drop its `[gw-attachment …]`
/// marker from the turn's content, reclaim the S3 object, and patch the
/// re-rendered turn back into the page. Unlike edit/retry this does NOT
/// regenerate anything — the user just doesn't want that file there
/// anymore. Works on both user uploads (`user_content`) and model-produced
/// files like generated images (`content`).
pub async fn chat_attachment_remove(
    Path(AttachmentRemovePath {
        id,
        turn_id,
        filename,
    }): Path<AttachmentRemovePath>,
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let lang = Lang::from_headers(req.headers());
    let turn = match load_owned_turn(&state, &user, &id, &turn_id, lang).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    // User uploads live in `user_content`; model-generated files (e.g.
    // `generate_image`) live in `content`. Rewrite whichever column owns
    // this turn's markers.
    let is_user = turn.role == chat::TurnRole::User;
    let content = if is_user {
        turn.user_content.clone().unwrap_or_default()
    } else {
        turn.content.clone().unwrap_or_default()
    };
    let new_content =
        session_core::attachments::remove_markers_where(&content, |a| a.filename == filename);
    let write = if is_user {
        chat::update_user_turn_content(&state.db, &id, &turn_id, &new_content)
            .await
            .map(|_| ())
    } else {
        chat::set_content(&state.db, &turn_id, &new_content).await
    };
    if let Err(err) = write {
        return sse_error_response(&err.to_string());
    }

    // Reclaim the bytes. Best-effort: the marker is already gone, so a
    // failed delete only leaves an orphaned object (idempotent DELETE
    // makes a later retry safe) — never fail the user's action over it.
    if let Some(cfg) = state.config.chat.s3.as_ref()
        && let Err(err) = chat_attachments::delete(cfg, &turn_id, &filename).await
    {
        tracing::warn!(error = %err, %turn_id, %filename, "attachment S3 delete (marker already removed)");
    }

    // Re-render the affected turn and patch it in place — no regeneration.
    let selector = format!("#turn-{turn_id}");
    let html = if is_user {
        let updated = match chat::get_turn(&state.db, &id, &turn_id).await {
            Ok(Some(t)) => t,
            _ => return sse_error_response(&t(lang, "chat-error-message-not-found")),
        };
        session_core::render::render_user_turn(&updated, Some("/chat"), lang).to_string()
    } else {
        let turns = match chat::list_turns(&state.db, &id).await {
            Ok(t) => t,
            Err(err) => return sse_error_response(&err.to_string()),
        };
        match turns.into_iter().find(|t| t.turn.id == turn_id) {
            Some(tw) => {
                session_core::render::render_assistant_turn(&tw, Some("/chat"), lang).to_string()
            }
            None => return sse_error_response(&t(lang, "chat-error-message-not-found")),
        }
    };
    sse_response(&[sse_patch(Some(&selector), Some("outer"), &html)])
}

/// Request-derived bits the worker needs that aren't part of the chat
/// session itself: the caller's source IP (for GeoIP) and whether the
/// browser is on a secure context (so a precise-location prompt can even
/// succeed). Bundled so the worker/regeneration signatures stay legible.
struct RequestCtx {
    client_ip: Option<String>,
    secure: bool,
    /// This turn came from voice-conversation mode → the driver injects the
    /// brevity/spoken-style directive. False for retry/edit regeneration.
    voice_mode: bool,
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
    // uses the unfiltered per-user set. Everything but the two interactive
    // handles below is shared with the headless scheduler via
    // `build_tool_context`.
    let tool_ctx = crate::openai_driver::build_tool_context(
        state,
        user.id.clone(),
        user.roles.clone(),
        session_id.to_string(),
        assistant_turn_id.to_string(),
        req.client_ip,
        // Chat path: hand the tool the live turn's broadcast + the
        // feedback hub so `get_user_location` can prompt the browser
        // for a precise position and wait for the reply.
        Some(crate::server::tools::ChatFeedback {
            broadcast: worker.broadcast.clone(),
            hub: state.location_feedback.clone(),
            secure: req.secure,
        }),
    );
    let driver = Box::new(crate::openai_driver::OpenAiDriver {
        state: state.clone(),
        tool_ctx,
        source: crate::server::db::usage::UsageSource::Chat,
        history_limit: None,
        voice_mode: req.voice_mode,
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
    let session_id_for_push = session_id.to_string();
    let turn_id_for_push = assistant_turn_id.to_string();
    tokio::spawn(async move {
        session_core::worker::run_session_turn(pool_for_worker, driver, driver_ctx).await;
        worker_state
            .chats
            .clear(&user_id_for_clear, &worker_for_task);
        // Turn's done — ping the user's subscribed browsers (no-op unless push
        // is enabled and they opted in). Runs after the worker so the row is
        // finalized and the status is readable.
        notify_turn_complete(
            &worker_state,
            &user_id_for_clear,
            &session_id_for_push,
            &turn_id_for_push,
        )
        .await;
    });
}

/// Fire a Web Push "turn finished" notification to the user's subscribed
/// browsers once a turn they started finalizes.
///
/// No-op unless push is enabled and the user has at least one subscription.
/// Best-effort: every failure is logged, never surfaced — the turn itself
/// already succeeded. Subscriptions the push service reports gone (404/410)
/// are pruned. The server always sends; whether the user is *actually* looking
/// at the app is decided client-side in the service worker, which suppresses
/// the notification when a focused tab already has this conversation open.
async fn notify_turn_complete(
    state: &RamaState,
    user_id: &str,
    session_id: &str,
    assistant_turn_id: &str,
) {
    use crate::server::push::{PushMessage, SendOutcome};
    use session_core::db::TurnStatus;

    let Some(push) = state.push.clone() else {
        return;
    };

    // Only notify on a real end state. `Cancelled` means the user pressed stop
    // (they're present), and `InProgress` shouldn't reach here.
    let turns = match session_core::db::list_turns(&state.db, session_id).await {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(error = %err, "push: reading finalized turn");
            return;
        }
    };
    let Some(view) = turns.iter().find(|t| t.turn.id == assistant_turn_id) else {
        return;
    };
    let errored = match view.turn.status {
        TurnStatus::Completed => false,
        TurnStatus::Errored => true,
        _ => return,
    };

    let subs = match crate::server::db::push_subscriptions::list_for_user(&state.db, user_id).await
    {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => return,
        Err(err) => {
            tracing::warn!(error = %err, "push: listing subscriptions");
            return;
        }
    };

    // Conversation title for the heading; an untitled chat gets a localized
    // fallback per subscription below.
    let session_title = session_core::db::get_session(&state.db, user_id, session_id)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.title)
        .filter(|t| !t.trim().is_empty());

    let url = format!("/chat/{session_id}");
    for sub in subs {
        let lang = sub
            .lang
            .as_deref()
            .and_then(Lang::from_code)
            .unwrap_or(Lang::En);
        // Cap the title: it's a user-influenced conversation title, and the
        // whole payload rides in one aes128gcm record with a 4 KB budget (and
        // FCM's own 4 KB body cap). 80 chars is plenty for a heading.
        let title = session_title
            .clone()
            .map(|t| session_core::render::truncate_chars(&t, 80))
            .unwrap_or_else(|| t(lang, "push-untitled-conversation"));
        let body = t(
            lang,
            if errored {
                "push-turn-error-body"
            } else {
                "push-turn-complete-body"
            },
        );
        let message = PushMessage {
            title,
            body,
            url: url.clone(),
            tag: session_id.to_string(),
        };
        if push.send(&sub, &message).await == SendOutcome::Gone
            && let Err(err) =
                crate::server::db::push_subscriptions::delete(&state.db, &sub.id).await
        {
            tracing::warn!(error = %err, "push: pruning gone subscription");
        }
    }
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
    lang: Lang,
) -> Response {
    let assistant_turn_id = uuid::Uuid::new_v4().to_string();
    let worker = match state
        .chats
        .register(&user.id, &assistant_turn_id, &session_id)
    {
        RegisterOutcome::Registered { worker } => worker,
        RegisterOutcome::Busy { .. } => {
            return sse_error_response(&t(lang, "chat-error-still-streaming"));
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
    for turn in &turns {
        inner.push_str(&session_core::render::render_turn(turn, Some("/chat"), lang).to_string());
    }
    let initial_patch = sse_patch(Some("#conversation"), Some("inner"), &inner);

    spawn_session_stream_response(
        state.db.clone(),
        session_id.clone(),
        assistant_turn.id.clone(),
        broadcast_rx,
        Some(initial_patch),
        gateway_sidebar_emitter(state.clone(), user.id.clone(), session_id, lang),
        Some("/chat".to_string()),
        lang,
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
    lang: Lang,
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
                pinned: session.pinned,
            };
            let html =
                super::render_sidebar_session(&sidebar, Some(&session.id), lang, None).to_string();
            let selector = format!("#session-row-{session_id}");
            let patch = sse_patch(Some(&selector), Some("outer"), &html);
            if tx.send(Ok(patch)).await.is_err() {
                Err(())
            } else {
                let assets = match chat_attachments::list_session_attachments(
                    &state.db,
                    &session_id,
                )
                .await
                {
                    Ok(assets) => assets,
                    Err(err) => {
                        tracing::warn!(error = %err, "chat stream: list assets for canvas patch failed");
                        return Ok(tx);
                    }
                };
                let assets_html = render::render_assets_panel(&assets, lang);
                let assets_patch = sse_patch(
                    Some("#assets-canvas-slot"),
                    Some("inner"),
                    &assets_html.to_string(),
                );
                if tx.send(Ok(assets_patch)).await.is_err() {
                    return Err(());
                }
                let signals = if assets.is_empty() {
                    sse_signals(r#"{"hasAssets": false, "assetCount": 0}"#)
                } else {
                    sse_signals(&format!(
                        "{{\"hasCanvas\": true, \"hasAssets\": true, \"assetCount\": {}}}",
                        assets.len()
                    ))
                };
                if tx.send(Ok(signals)).await.is_err() {
                    return Err(());
                }
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

async fn list_transcription_models(
    state: &RamaState,
    access: &crate::server::upstreams::PoolAccess,
) -> Vec<String> {
    use crate::server::feature_defaults::{self, Feature};
    let mut models = state
        .upstreams
        .models_for_kind_for(crate::server::upstreams::PoolKind::Transcription, access);
    // Move the operator-configured default (if still served) to the front so
    // the voice picker's first-option-wins default pre-selects it.
    let configured = feature_defaults::get(&state.db, Feature::Transcription).await;
    feature_defaults::promote(configured.as_deref(), &mut models, |m| m.as_str());
    models
}

async fn list_chat_models(
    state: &RamaState,
    access: &crate::server::upstreams::PoolAccess,
) -> Vec<render::ChatModelOption> {
    use crate::server::feature_defaults::{self, Feature};
    let mut models: Vec<render::ChatModelOption> = state
        .upstreams
        .models_with_compliance_for_kind_for(crate::server::upstreams::PoolKind::Chat, access)
        .into_iter()
        .map(|(id, c)| render::ChatModelOption {
            id,
            gdpr: c.gdpr,
            nda: c.nda,
        })
        .collect();
    // The chat picker (and the compliance banner it seeds) treats the first
    // option as the default; promoting the configured model there pre-selects
    // it without a separate `selected` concept. Unset / unavailable → no-op.
    let configured = feature_defaults::get(&state.db, Feature::Chat).await;
    feature_defaults::promote(configured.as_deref(), &mut models, |m| m.id.as_str());
    models
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
    /// This turn was submitted from voice-conversation mode — the worker
    /// injects the brevity/spoken-style directive. Per-turn only; not persisted.
    voice: bool,
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
    let mut voice = false;

    // Track the filenames already claimed under this turn so each upload
    // lands on a distinct S3 key. Seeded with any filenames already
    // attached to the turn (empty on the new-message path; populated when
    // editing a turn that already has attachments). Without this, several
    // clipboard-pasted images — the browser names every one `image.png` —
    // upload to the SAME key and overwrite each other, so every marker
    // ends up pointing at the last image. Dedup renames the collisions to
    // `image-2.png`, `image-3.png`, … exactly like `reserve_filename`.
    let existing_content = chat::get_content(&state.db, turn_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut used_names = session_core::attachments::existing_filenames(&existing_content);

    while let Some(field) = mp.next_field().await.map_err(|e| e.to_string())? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "model" => {
                model = Some(field.text().await.map_err(|e| e.to_string())?);
            }
            "message" => {
                user_text = field.text().await.map_err(|e| e.to_string())?;
            }
            "voice" => {
                let v = field.text().await.map_err(|e| e.to_string())?;
                voice = matches!(v.trim(), "true" | "1" | "on");
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
                // Nameless blobs (some drag/paste sources) get a
                // mime-appropriate default before dedup so we never upload
                // an empty-named object.
                let desired = if filename.trim().is_empty() {
                    format!(
                        "pasted{}",
                        chat_attachments::ext_for_mime(&mime).unwrap_or(".bin")
                    )
                } else {
                    filename
                };
                let filename =
                    session_core::attachments::dedupe_filename_against(&used_names, &desired);
                used_names.insert(filename.clone());
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
        voice,
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
    let lang = Lang::from_headers(req.headers());
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
            &t(lang, "chat-error-pdf-export-unavailable"),
        ),
        Err(err) => {
            tracing::error!(error = %err, %session_id, "chat PDF export compile");
            export_error(
                rama::http::StatusCode::INTERNAL_SERVER_ERROR,
                &t(lang, "chat-error-pdf-export-failed"),
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
    let lang = Lang::from_headers(req.headers());
    // 401 (not redirect) — `<img src>` will just show broken-image
    // if the cookie went bad, and a 401 is honest in operator logs.
    let session = match state.sessions.lookup_from_headers(req.headers()).await {
        Ok(Some(s)) => s,
        _ => {
            return attachment_error(
                rama::http::StatusCode::UNAUTHORIZED,
                &t(lang, "chat-error-auth-required"),
            );
        }
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
            return attachment_error(
                rama::http::StatusCode::NOT_FOUND,
                &t(lang, "chat-error-no-such-turn"),
            );
        }
        Err(err) => {
            tracing::warn!(error = %err, "turn_session_readable");
            return attachment_error(
                rama::http::StatusCode::INTERNAL_SERVER_ERROR,
                &t(lang, "chat-error-db-error"),
            );
        }
    }
    let Some(cfg) = state.config.chat.s3.as_ref() else {
        return attachment_error(
            rama::http::StatusCode::SERVICE_UNAVAILABLE,
            &t(lang, "chat-error-attachments-not-configured"),
        );
    };
    let fetched = match chat_attachments::fetch(cfg, &turn_id, &filename).await {
        Ok(f) => f,
        Err(chat_attachments::AttachmentError::BadFilename(_)) => {
            return attachment_error(
                rama::http::StatusCode::BAD_REQUEST,
                &t(lang, "chat-error-bad-filename"),
            );
        }
        Err(err) => {
            tracing::warn!(error = %err, %turn_id, %filename, "attachment fetch");
            return attachment_error(
                rama::http::StatusCode::NOT_FOUND,
                &t(lang, "chat-error-attachment-not-found"),
            );
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
