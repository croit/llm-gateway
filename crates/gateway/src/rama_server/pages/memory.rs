// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The per-user `/memory` page — inspect, add, edit, and delete the
//! structured memories the assistant keeps about you.
//!
//! This is the explicit, user-owned side of the `remember` / `recall`
//! tools: every signed-in user sees their own memories grouped by kind
//! (preferences / project context / facts) and stays in full control of
//! them. All reads + writes are scoped to the session user — no
//! cross-user path. The on/off switch for whether the assistant may use
//! memory at all lives separately, on the /tools page.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::{Path, State};
use rama::http::{Request, Response};
use serde::Deserialize;

use super::{
    NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, read_form,
    require_session_or_redirect, toast,
};
use session_core::chrome::{
    Flash, FlashKind, Theme, is_datastar_request, sse_patch, sse_response, sse_script, sse_toast,
};
use session_core::icons;

use crate::rama_server::state::RamaState;
use crate::server::db::user_memories::{self, Memory, MemoryKind};

/// Sanity bound on how many memories we render at once. Far above any
/// realistic per-user count; just keeps a runaway store from rendering
/// forever.
const LIST_LIMIT: i64 = 500;

/// Matches the per-fact cap the `remember` tool enforces, so the UI and
/// the model agree on what "too long" means.
const MAX_CONTENT_LEN: usize = 2_000;

// ---------------------------------------------------------------------------
// GET /memory

/// GET /memory — the caller's memories, grouped by kind, each editable.
pub async fn memory_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());

    let (session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let memories = user_memories::list_for_user(&state.db, &user.id, LIST_LIMIT)
        .await
        .unwrap_or_default();
    let body = render_memory_body(&memories);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        NavItem::Memory,
        "Memory — LLM Gateway",
        &user.email,
        is_admin(&state, &user),
        session.impersonator_id.is_some(),
        body,
        "/memory",
        &chat,
    )
}

// ---------------------------------------------------------------------------
// POST /memory  (add)

#[derive(Deserialize)]
struct CreateForm {
    kind: String,
    content: String,
}

/// Validate + normalise a submitted (kind, content) pair. On failure
/// returns a short error message the caller surfaces as a toast (kept a
/// `String` rather than a `Response` so the error variant stays small).
fn parse_fields(kind: &str, content: &str) -> Result<(MemoryKind, String), String> {
    let Some(kind) = MemoryKind::parse(kind.trim()) else {
        return Err("unknown memory kind".into());
    };
    let content = content.trim();
    if content.is_empty() {
        return Err("Memory text must not be empty.".into());
    }
    if content.len() > MAX_CONTENT_LEN {
        return Err(format!(
            "Memory text must be under {MAX_CONTENT_LEN} characters."
        ));
    }
    Ok((kind, content.to_string()))
}

/// POST /memory — add a memory the user typed in. Appends the new row to
/// its kind's list and resets the form.
pub async fn memory_create(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let form: CreateForm = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let (kind, content) = match parse_fields(&form.kind, &form.content) {
        Ok(v) => v,
        Err(msg) => return toast(FlashKind::Error, msg),
    };

    let row = match user_memories::insert(&state.db, &user.id, kind, &content).await {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(error = %err, "memory create");
            return toast(FlashKind::Error, "Could not save memory.");
        }
    };

    let list_selector = format!("#mem-list-{}", row.kind.as_str());
    let row_html = render_memory_row(&row).to_string();
    sse_response(&[
        sse_patch(Some(&list_selector), Some("append"), &row_html),
        sse_script("document.getElementById('mem-add-form').reset()"),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: "Memory added.".into(),
        }),
    ])
}

// ---------------------------------------------------------------------------
// POST /memory/{id}/edit

#[derive(Deserialize)]
struct EditForm {
    content: String,
}

/// POST /memory/{id}/edit — change a memory's text. Kind is preserved
/// (reclassifying is left to re-remembering); ownership is enforced by
/// the scoped update.
pub async fn memory_edit(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let form: EditForm = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };

    // Need the existing row to keep its kind (and to confirm ownership
    // before we report success).
    let existing = match user_memories::get(&state.db, &user.id, &id).await {
        Ok(Some(m)) => m,
        Ok(None) => return toast(FlashKind::Error, "Memory not found."),
        Err(err) => {
            tracing::warn!(error = %err, "memory edit lookup");
            return toast(FlashKind::Error, "Could not load memory.");
        }
    };
    let (_, content) = match parse_fields(existing.kind.as_str(), &form.content) {
        Ok(v) => v,
        Err(msg) => return toast(FlashKind::Error, msg),
    };

    match user_memories::update(&state.db, &user.id, &id, existing.kind, &content).await {
        Ok(Some(updated)) => {
            let selector = format!("#mem-row-{id}");
            let row_html = render_memory_row(&updated).to_string();
            sse_response(&[
                sse_patch(Some(&selector), Some("outer"), &row_html),
                sse_toast(&Flash {
                    kind: FlashKind::Success,
                    message: "Memory updated.".into(),
                }),
            ])
        }
        Ok(None) => toast(FlashKind::Error, "Memory not found."),
        Err(err) => {
            tracing::warn!(error = %err, "memory update");
            toast(FlashKind::Error, "Could not update memory.")
        }
    }
}

// ---------------------------------------------------------------------------
// POST /memory/{id}/delete

/// POST /memory/{id}/delete — remove a memory. Scoped to the owner.
pub async fn memory_delete(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match user_memories::delete(&state.db, &user.id, &id).await {
        Ok(true) => {
            let selector = format!("#mem-row-{id}");
            sse_response(&[
                sse_patch(Some(&selector), Some("remove"), ""),
                sse_toast(&Flash {
                    kind: FlashKind::Success,
                    message: "Memory removed.".into(),
                }),
            ])
        }
        Ok(false) => toast(FlashKind::Info, "Memory was already gone."),
        Err(err) => {
            tracing::warn!(error = %err, "memory delete");
            toast(FlashKind::Error, "Could not remove memory.")
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering

fn render_memory_body(memories: &[Memory]) -> Html {
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
        h1(class: "text-2xl font-bold mb-2") { "Memory" }
        p(class: "text-base-content/60 text-sm mb-6") {
            "What the assistant remembers about you, grouped by kind. Add, edit, or delete "
            "entries here — it's your account's memory and fully under your control. Turn the "
            "capability on or off on the Tools page."
        }

        (render_add_form())

        // One section per kind, always rendered so the add-form can
        // append into the right list even when a kind starts empty.
        for kind in MemoryKind::ALL {
            (render_kind_section(kind, memories))
        }
        }
    }
    .to_html()
}

fn render_add_form() -> Html {
    html! {
        form(
            id: "mem-add-form",
            action: "/memory",
            method: "post",
            class: "card border border-base-300 mb-6",
            "data-on:submit__prevent": "@post('/memory', {contentType: 'form'})"
        ) {
            div(class: "card-body gap-3") {
                h2(class: "card-title text-base") { "Add a memory" }
                div(class: "flex flex-col sm:flex-row gap-2") {
                    select(
                        name: "kind",
                        class: "select select-bordered sm:w-48",
                        "aria-label": "Memory kind"
                    ) {
                        for kind in MemoryKind::ALL {
                            option(value: (kind.as_str())) { (kind.label()) }
                        }
                    }
                    input(
                        name: "content",
                        type: "text",
                        required: "required",
                        maxlength: "2000",
                        placeholder: "e.g. Prefers answers in metric units",
                        class: "input input-bordered flex-1 min-w-0"
                    );
                    button(type: "submit", class: "btn btn-primary") { "Remember" }
                }
            }
        }
    }
    .to_html()
}

fn render_kind_section(kind: MemoryKind, memories: &[Memory]) -> Html {
    let list_id = format!("mem-list-{}", kind.as_str());
    let rows: Vec<&Memory> = memories.iter().filter(|m| m.kind == kind).collect();
    html! {
        section(class: "card border border-base-300 mb-6") {
            div(class: "card-body") {
                h2(class: "card-title text-base") { (kind.label()) }
                ul(id: (list_id), class: "flex flex-col divide-y divide-base-300") {
                    for m in rows.iter() {
                        (render_memory_row(m))
                    }
                }
                if rows.is_empty() {
                    p(class: "text-base-content/50 text-sm") { "Nothing here yet." }
                }
            }
        }
    }
    .to_html()
}

/// One memory row: an inline edit form (text + Save) plus a Delete
/// button. Both `@post` and patch the row in place via SSE.
fn render_memory_row(m: &Memory) -> Html {
    let row_id = format!("mem-row-{}", m.id);
    let content = m.content.clone();
    let edit_action = format!("/memory/{}/edit", m.id);
    let delete_action = format!("/memory/{}/delete", m.id);
    let edit_directive = format!("@post('{edit_action}', {{contentType: 'form'}})");
    let delete_directive = format!("@post('{delete_action}', {{contentType: 'form'}})");
    html! {
        li(id: (row_id), class: "flex items-center gap-2 py-2") {
            form(
                action: (edit_action),
                method: "post",
                class: "flex items-center gap-2 flex-1 min-w-0 m-0",
                "data-on:submit__prevent": (edit_directive)
            ) {
                input(
                    name: "content",
                    type: "text",
                    value: (content),
                    maxlength: "2000",
                    required: "required",
                    class: "input input-bordered input-sm flex-1 min-w-0",
                    "aria-label": "Memory text"
                );
                button(type: "submit", class: "btn btn-outline btn-sm") { "Save" }
            }
            form(
                action: (delete_action),
                method: "post",
                class: "m-0",
                "data-on:submit__prevent": (delete_directive)
            ) {
                button(
                    type: "submit",
                    class: "btn btn-ghost btn-square btn-sm",
                    title: "Delete memory",
                    "aria-label": "Delete memory"
                ) {
                    (icons::trash(16))
                }
            }
        }
    }
    .to_html()
}
