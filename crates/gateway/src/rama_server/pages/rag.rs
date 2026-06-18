// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/rag` page — operator-facing CRUD for indexed RAG collections.
//!
//! Mirrors `pages/tokens.rs` in shape: a list of cards, a create form
//! at the top, and per-row Re-index / Delete actions wired via
//! datastar `@post` + SSE patches so the page updates surgically
//! without a full reload. Admin-gated (`require_admin_or_403`); the
//! sidebar entry is only rendered for admins, matching `/admin/*`.
//!
//! V1 has no live status pump: the indexer flips collection rows
//! between `pending` / `cloning` / `indexing` / `ready` / `error` in
//! its background poll, and the page shows whatever the DB says on
//! the next request. Re-index POST replies with a freshly-rendered
//! row, so the status badge does update immediately after the click.
//! True push-from-indexer-to-browser is a Phase 5 concern.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::{Path, State};
use rama::http::{Request, Response};
use serde::Deserialize;

use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_admin_or_403};
use session_core::chrome::{
    Flash, FlashKind, Theme, is_datastar_request, read_body_to_bytes, sse_patch, sse_response,
    sse_script, sse_toast,
};
use session_core::icons;

use crate::rama_server::state::RamaState;
use crate::server::db::rag as rag_db;
use crate::server::upstreams::PoolKind;

#[derive(Deserialize)]
struct CreateForm {
    name: String,
    description: Option<String>,
    git_url: String,
    git_ref: Option<String>,
    pat: Option<String>,
    embedding_model: String,
    include_globs: Option<String>,
    exclude_globs: Option<String>,
    chunk_size: Option<i64>,
    chunk_overlap: Option<i64>,
}

/// GET /rag — admin-only list of indexed collections with a create
/// form at the top.
pub async fn rag_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (_session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let collections = match rag_db::list_collections(&state.db).await {
        Ok(l) => l,
        Err(err) => {
            tracing::warn!(error = %err, "listing rag collections");
            Vec::new()
        }
    };
    let embedding_models = {
        let mut m = state.upstreams.models_for_kind(PoolKind::Embedding);
        m.sort();
        m
    };
    let body = render_body(&collections, &embedding_models);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        NavItem::Rag,
        "RAG collections — LLM Gateway",
        &user.email,
        is_admin(&state, &user),
        body,
        "/rag",
        &chat,
    )
}

/// POST /rag — create a new collection. Form-encoded body. SSE response
/// patches the list with the new row and resets the form.
pub async fn rag_create(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: CreateForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => return toast(FlashKind::Error, format!("malformed form: {err}")),
    };
    let new = match validate(form) {
        Ok(n) => n,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let created = match rag_db::create_collection(&state.db, &new).await {
        Ok(c) => c,
        Err(err) => {
            let s = err.to_string();
            tracing::warn!(error = %err, "create rag collection");
            return toast(
                FlashKind::Error,
                if s.contains("UNIQUE") || s.contains("constraint") {
                    format!("a collection named `{}` already exists", new.name)
                } else {
                    "could not create collection".to_string()
                },
            );
        }
    };
    let row_html = render_row(&created).to_string();
    sse_response(&[
        sse_patch(Some("#rag-list"), Some("append"), &row_html),
        sse_script("document.getElementById('rag-create-form').reset()"),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: format!("Indexing `{}` was queued.", created.name),
        }),
    ])
}

/// POST /rag/{id}/reindex — flip the row back to `pending`. The
/// background worker picks it up on the next tick.
pub async fn rag_reindex(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    if let Err(err) = rag_db::request_reindex(&state.db, id).await {
        tracing::warn!(error = %err, %id, "rag reindex");
        return toast(FlashKind::Error, "could not queue re-index");
    }
    let Ok(Some(updated)) = rag_db::find_collection_by_id(&state.db, id).await else {
        return toast(FlashKind::Error, "collection not found");
    };
    let selector = format!("#rag-row-{}", updated.id);
    sse_response(&[
        sse_patch(
            Some(&selector),
            Some("outer"),
            &render_row(&updated).to_string(),
        ),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: format!("Queued re-index of `{}`.", updated.name),
        }),
    ])
}

/// POST /rag/{id}/edit-form — SSE-swap the row to an editable form.
/// Pre-fills every field from the stored row and resolves the embedding
/// model against the live pool list so the select pre-selects the right
/// option (with a graceful "no longer advertised" fallback if the pool
/// has changed out from under us).
pub async fn rag_edit_form(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let collection = match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return toast(FlashKind::Error, "Collection not found."),
        Err(err) => {
            tracing::warn!(error = %err, %id, "lookup rag collection");
            return toast(FlashKind::Error, "Could not load collection.");
        }
    };
    let mut models = state.upstreams.models_for_kind(PoolKind::Embedding);
    models.sort();
    let selector = format!("#rag-row-{id}");
    sse_response(&[sse_patch(
        Some(&selector),
        Some("outer"),
        &render_edit_form(&collection, &models).to_string(),
    )])
}

/// POST /rag/{id}/cancel-edit — SSE-swap the row back to its
/// display form. The user gave up on the edit; nothing is saved.
pub async fn rag_cancel_edit(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let collection = match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return toast(FlashKind::Error, "Collection not found."),
        Err(err) => {
            tracing::warn!(error = %err, %id, "lookup rag collection");
            return toast(FlashKind::Error, "Could not load collection.");
        }
    };
    let selector = format!("#rag-row-{id}");
    sse_response(&[sse_patch(
        Some(&selector),
        Some("outer"),
        &render_row(&collection).to_string(),
    )])
}

#[derive(Deserialize)]
struct UpdateForm {
    description: Option<String>,
    git_url: String,
    git_ref: Option<String>,
    /// New PAT value. Empty (or absent) means "leave the stored PAT
    /// alone"; combined with `clear_pat` it can also mean "remove it".
    pat: Option<String>,
    /// Checkbox value when set means "clear the stored PAT regardless
    /// of what's in `pat`". Lets the operator drop a PAT without
    /// knowing the current one.
    #[serde(default)]
    clear_pat: Option<String>,
    embedding_model: String,
    include_globs: Option<String>,
    exclude_globs: Option<String>,
    chunk_size: Option<i64>,
    chunk_overlap: Option<i64>,
}

/// POST /rag/{id}/update — save the edited form. Patches the row back
/// to its display shape; toasts a success / error message.
pub async fn rag_update(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: UpdateForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => return toast(FlashKind::Error, format!("malformed form: {err}")),
    };

    // Pull the current row so we can resolve "leave unchanged" semantics
    // on PAT and ground the success toast in a stable name.
    let existing = match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return toast(FlashKind::Error, "Collection not found."),
        Err(err) => {
            tracing::warn!(error = %err, %id, "lookup rag collection");
            return toast(FlashKind::Error, "Could not load collection.");
        }
    };

    let git_url = form.git_url.trim();
    if git_url.is_empty() {
        return toast(FlashKind::Error, "Git URL is required.");
    }
    let embedding_model = form.embedding_model.trim();
    if embedding_model.is_empty() {
        return toast(FlashKind::Error, "Embedding model is required.");
    }
    let git_ref = form
        .git_ref
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string());
    let chunk_size = form.chunk_size.unwrap_or(existing.chunk_size);
    let chunk_overlap = form.chunk_overlap.unwrap_or(existing.chunk_overlap);
    if chunk_size <= 0 || chunk_size > 8000 {
        return toast(FlashKind::Error, "Chunk size must be in (0, 8000].");
    }
    if chunk_overlap < 0 || chunk_overlap >= chunk_size {
        return toast(
            FlashKind::Error,
            "Chunk overlap must be in [0, chunk_size).",
        );
    }
    let description = form
        .description
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let include_globs = split_globs(form.include_globs);
    let exclude_globs = split_globs(form.exclude_globs);
    let include_json = serde_json::to_string(&include_globs).unwrap_or_else(|_| "[]".into());
    let exclude_json = serde_json::to_string(&exclude_globs).unwrap_or_else(|_| "[]".into());

    let clear_pat = form.clear_pat.is_some();
    let new_pat: Option<String> = form
        .pat
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // Tri-state: explicit clear, explicit set, otherwise leave alone.
    let pat_to_store: Option<String> = if clear_pat {
        None
    } else if let Some(p) = new_pat {
        Some(p)
    } else {
        existing.pat.clone()
    };

    let now = jiff::Timestamp::now().to_string();
    let res = sqlx::query(
        r#"UPDATE rag_collections SET
               description = ?,
               git_url = ?,
               git_ref = ?,
               pat = ?,
               embedding_model = ?,
               include_globs_json = ?,
               exclude_globs_json = ?,
               chunk_size = ?,
               chunk_overlap = ?,
               updated_at = ?
           WHERE id = ?"#,
    )
    .bind(&description)
    .bind(git_url)
    .bind(&git_ref)
    .bind(&pat_to_store)
    .bind(embedding_model)
    .bind(&include_json)
    .bind(&exclude_json)
    .bind(chunk_size)
    .bind(chunk_overlap)
    .bind(&now)
    .bind(id)
    .execute(&state.db)
    .await;
    if let Err(err) = res {
        tracing::warn!(error = %err, %id, "update rag collection");
        return toast(FlashKind::Error, "Saving collection failed.");
    }
    let updated = match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return toast(FlashKind::Error, "Collection vanished after save."),
        Err(err) => {
            tracing::warn!(error = %err, %id, "post-update lookup");
            return toast(FlashKind::Error, "Saved but reload failed.");
        }
    };
    let selector = format!("#rag-row-{id}");
    sse_response(&[
        sse_patch(
            Some(&selector),
            Some("outer"),
            &render_row(&updated).to_string(),
        ),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: format!("Saved `{}`.", existing.name),
        }),
    ])
}

/// POST /rag/{id}/delete — drop the collection (cascades chunks + files).
/// SSE removes the row from the list. The on-disk usearch file +
/// clone-cache directory survive — the next collection that gets the
/// same id wouldn't either way, since `INTEGER PRIMARY KEY AUTOINCREMENT`
/// monotonically advances. Operators can wipe them with `rm`.
pub async fn rag_delete(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    match rag_db::delete_collection(&state.db, id).await {
        Ok(true) => {
            let selector = format!("#rag-row-{id}");
            sse_response(&[
                sse_patch(Some(&selector), Some("remove"), ""),
                sse_toast(&Flash {
                    kind: FlashKind::Success,
                    message: "Collection removed.".into(),
                }),
            ])
        }
        Ok(false) => toast(FlashKind::Info, "Collection already gone."),
        Err(err) => {
            tracing::warn!(error = %err, %id, "rag delete");
            toast(FlashKind::Error, "Delete failed.")
        }
    }
}

fn toast(kind: FlashKind, message: impl Into<String>) -> Response {
    sse_response(&[sse_toast(&Flash {
        kind,
        message: message.into(),
    })])
}

fn validate(form: CreateForm) -> Result<rag_db::NewCollection, String> {
    let name = form.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err("Name must be 1..=64 characters.".into());
    }
    let git_url = form.git_url.trim();
    if git_url.is_empty() {
        return Err("Git URL is required.".into());
    }
    let embedding_model = form.embedding_model.trim();
    if embedding_model.is_empty() {
        return Err("Embedding model is required.".into());
    }
    let chunk_size = form.chunk_size.unwrap_or(800);
    let chunk_overlap = form.chunk_overlap.unwrap_or(100);
    if chunk_size <= 0 || chunk_size > 8000 {
        return Err("Chunk size must be in (0, 8000].".into());
    }
    if chunk_overlap < 0 || chunk_overlap >= chunk_size {
        return Err("Chunk overlap must be in [0, chunk_size).".into());
    }
    Ok(rag_db::NewCollection {
        name: name.to_string(),
        description: form
            .description
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        git_url: git_url.to_string(),
        git_ref: form
            .git_ref
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "main".to_string()),
        pat: form
            .pat
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        embedding_model: embedding_model.to_string(),
        include_globs: split_globs(form.include_globs),
        exclude_globs: split_globs(form.exclude_globs),
        chunk_size,
        chunk_overlap,
    })
}

fn split_globs(raw: Option<String>) -> Vec<String> {
    raw.unwrap_or_default()
        .split([',', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn status_badge(status: rag_db::CollectionStatus) -> Html {
    let (cls, label) = match status {
        rag_db::CollectionStatus::Pending => ("badge badge-outline", "pending"),
        rag_db::CollectionStatus::Cloning => ("badge badge-info", "cloning"),
        rag_db::CollectionStatus::Indexing => ("badge badge-info", "indexing"),
        rag_db::CollectionStatus::Ready => ("badge badge-success", "ready"),
        rag_db::CollectionStatus::Error => ("badge badge-error", "error"),
    };
    html! {
        span(class: (cls)) { (label) }
    }
    .to_html()
}

fn render_row(c: &rag_db::Collection) -> Html {
    let dom_id = format!("rag-row-{}", c.id);
    let reindex_action = format!("/rag/{}/reindex", c.id);
    let delete_action = format!("/rag/{}/delete", c.id);
    let edit_action = format!("/rag/{}/edit-form", c.id);
    let reindex_directive = format!("@post('{reindex_action}', {{contentType: 'form'}})");
    let delete_directive = format!("@post('{delete_action}', {{contentType: 'form'}})");
    let edit_directive = format!("@post('{edit_action}', {{contentType: 'form'}})");
    let last_indexed = c
        .last_indexed_at
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "never".to_string());
    let last_commit = c
        .last_indexed_commit
        .as_deref()
        .unwrap_or("—")
        .chars()
        .take(8)
        .collect::<String>();
    let pat_hint = if c.pat.is_some() { "PAT set" } else { "no PAT" };
    let description = c.description.clone().unwrap_or_default();
    let status = c.status;
    html! {
        li(
            id: (dom_id),
            class: "flex flex-col gap-2 py-4"
        ) {
            div(class: "flex items-start gap-4") {
                div(class: "flex-1 min-w-0") {
                    div(class: "flex items-center gap-2") {
                        span(class: "text-base font-medium") { (c.name.clone()) }
                        (status_badge(status))
                    }
                    if !description.is_empty() {
                        p(class: "text-sm text-base-content/70 mt-0.5") { (description) }
                    }
                    p(class: "text-xs text-base-content/60 mt-1 font-mono break-all") {
                        (c.git_url.clone()) " @ " (c.git_ref.clone()) " · " (pat_hint)
                    }
                    p(class: "text-xs text-base-content/60 mt-1") {
                        "embed: " (c.embedding_model.clone()) " · last indexed " (last_indexed)
                        " · commit " (last_commit)
                    }
                    if let Some(err) = c.last_error.as_ref() {
                        p(class: "text-xs text-error mt-1 break-all") { "error: " (err.clone()) }
                    }
                }
                div(class: "flex flex-col gap-2 shrink-0") {
                    form(
                        action: (edit_action.clone()),
                        method: "post",
                        class: "m-0",
                        "data-on:submit__prevent": (edit_directive)
                    ) {
                        button(type: "submit", class: "btn btn-sm btn-outline") { "Edit" }
                    }
                    form(
                        action: (reindex_action.clone()),
                        method: "post",
                        class: "m-0",
                        "data-on:submit__prevent": (reindex_directive)
                    ) {
                        button(type: "submit", class: "btn btn-sm") { "Re-index" }
                    }
                    form(
                        action: (delete_action.clone()),
                        method: "post",
                        class: "m-0",
                        "data-on:submit__prevent": (delete_directive)
                    ) {
                        button(type: "submit", class: "btn btn-sm btn-outline btn-error") { "Delete" }
                    }
                }
            }
        }
    }
    .to_html()
}

fn render_create_form(embedding_models: &[String]) -> Html {
    html! {
        form(
            id: "rag-create-form",
            action: "/rag",
            method: "post",
            class: "card border border-base-300 mb-6",
            "data-on:submit__prevent": "@post('/rag', {contentType: 'form'})"
        ) {
            div(class: "card-body") {
                h2(class: "card-title") { "Index a new collection" }
                p(class: "text-base-content/70 text-sm") {
                    "The indexer clones the repo, chunks each file, and embeds it through "
                    "the configured embedding model. PATs are stored verbatim (the gateway "
                    "runs on trusted infra)."
                }
                div(class: "grid grid-cols-1 md:grid-cols-2 gap-4 mt-2") {
                    label(class: "form-control w-full") {
                        div(class: "label") { span(class: "label-text") { "Name" } }
                        input(
                            name: "name",
                            type: "text",
                            required: "required",
                            placeholder: "e.g. gateway-repo",
                            class: "input input-bordered w-full"
                        );
                    }
                    (embedding_model_field(embedding_models, None))
                    label(class: "form-control w-full md:col-span-2") {
                        div(class: "label") { span(class: "label-text") { "Description (optional)" } }
                        input(
                            name: "description",
                            type: "text",
                            placeholder: "short, human-readable",
                            class: "input input-bordered w-full"
                        );
                    }
                    label(class: "form-control w-full") {
                        div(class: "label") { span(class: "label-text") { "Git URL" } }
                        input(
                            name: "git_url",
                            type: "text",
                            required: "required",
                            placeholder: "https://example.com/org/repo.git",
                            class: "input input-bordered w-full"
                        );
                    }
                    label(class: "form-control w-full") {
                        div(class: "label") { span(class: "label-text") { "Branch / tag" } }
                        input(
                            name: "git_ref",
                            type: "text",
                            value: "main",
                            class: "input input-bordered w-full"
                        );
                    }
                    label(class: "form-control w-full md:col-span-2") {
                        div(class: "label") {
                            span(class: "label-text") { "Personal access token (optional)" }
                        }
                        input(
                            name: "pat",
                            type: "password",
                            placeholder: "for private repos",
                            class: "input input-bordered w-full"
                        );
                    }
                    label(class: "form-control w-full") {
                        div(class: "label") {
                            span(class: "label-text") { "Include globs (comma- or newline-separated)" }
                        }
                        input(
                            name: "include_globs",
                            type: "text",
                            placeholder: "*.rs, *.md",
                            class: "input input-bordered w-full"
                        );
                    }
                    label(class: "form-control w-full") {
                        div(class: "label") {
                            span(class: "label-text") { "Exclude globs" }
                        }
                        input(
                            name: "exclude_globs",
                            type: "text",
                            placeholder: "target/, node_modules/",
                            class: "input input-bordered w-full"
                        );
                    }
                    label(class: "form-control w-full") {
                        div(class: "label") { span(class: "label-text") { "Chunk size" } }
                        input(
                            name: "chunk_size",
                            type: "number",
                            value: "800",
                            min: "1",
                            max: "8000",
                            class: "input input-bordered w-full"
                        );
                    }
                    label(class: "form-control w-full") {
                        div(class: "label") { span(class: "label-text") { "Chunk overlap" } }
                        input(
                            name: "chunk_overlap",
                            type: "number",
                            value: "100",
                            min: "0",
                            class: "input input-bordered w-full"
                        );
                    }
                }
                div(class: "card-actions justify-end mt-2") {
                    button(type: "submit", class: "btn btn-primary") { "Queue indexing" }
                }
            }
        }
    }
    .to_html()
}

/// The row swapped in by `rag_edit_form`. Same `<li id="rag-row-{id}">`
/// shell so the SSE outer-replace round-trips cleanly between display
/// and edit modes. Fields are pre-filled from the stored row.
fn render_edit_form(c: &rag_db::Collection, embedding_models: &[String]) -> Html {
    let dom_id = format!("rag-row-{}", c.id);
    let update_action = format!("/rag/{}/update", c.id);
    let cancel_action = format!("/rag/{}/cancel-edit", c.id);
    let update_directive = format!("@post('{update_action}', {{contentType: 'form'}})");
    let cancel_directive = format!("@post('{cancel_action}', {{contentType: 'form'}})");
    let description = c.description.clone().unwrap_or_default();
    let include_csv = c.include_globs.join(", ");
    let exclude_csv = c.exclude_globs.join(", ");
    let chunk_size = c.chunk_size.to_string();
    let chunk_overlap = c.chunk_overlap.to_string();
    let pat_present = c.pat.is_some();
    html! {
        li(
            id: (dom_id),
            class: "py-4"
        ) {
            form(
                action: (update_action.clone()),
                method: "post",
                class: "card border border-base-300 bg-base-200",
                "data-on:submit__prevent": (update_directive)
            ) {
                div(class: "card-body") {
                    div(class: "flex items-center gap-2") {
                        h3(class: "card-title text-base m-0") {
                            "Editing " (c.name.clone())
                        }
                        (status_badge(c.status))
                    }
                    div(class: "grid grid-cols-1 md:grid-cols-2 gap-4 mt-2") {
                        label(class: "form-control w-full md:col-span-2") {
                            div(class: "label") { span(class: "label-text") { "Description" } }
                            input(
                                name: "description",
                                type: "text",
                                value: (description),
                                class: "input input-bordered w-full"
                            );
                        }
                        label(class: "form-control w-full") {
                            div(class: "label") { span(class: "label-text") { "Git URL" } }
                            input(
                                name: "git_url",
                                type: "text",
                                required: "required",
                                value: (c.git_url.clone()),
                                class: "input input-bordered w-full"
                            );
                        }
                        label(class: "form-control w-full") {
                            div(class: "label") { span(class: "label-text") { "Branch / tag" } }
                            input(
                                name: "git_ref",
                                type: "text",
                                value: (c.git_ref.clone()),
                                class: "input input-bordered w-full"
                            );
                        }
                        (embedding_model_field(embedding_models, Some(&c.embedding_model)))
                        div(class: "form-control w-full") {
                            div(class: "label") {
                                span(class: "label-text") {
                                    "Personal access token"
                                    if pat_present {
                                        span(class: "ml-2 badge badge-success badge-outline") {
                                            "currently set"
                                        }
                                    } else {
                                        span(class: "ml-2 badge badge-ghost") { "none stored" }
                                    }
                                }
                            }
                            input(
                                name: "pat",
                                type: "password",
                                placeholder: (
                                    if pat_present { "leave blank to keep existing" }
                                    else { "for private repos" }
                                ),
                                class: "input input-bordered w-full"
                            );
                            if pat_present {
                                label(class: "label cursor-pointer justify-start gap-2 mt-1") {
                                    input(
                                        type: "checkbox",
                                        name: "clear_pat",
                                        value: "1",
                                        class: "checkbox checkbox-sm"
                                    );
                                    span(class: "label-text text-sm") {
                                        "Remove the stored PAT (no longer authenticate)"
                                    }
                                }
                            }
                        }
                        label(class: "form-control w-full") {
                            div(class: "label") {
                                span(class: "label-text") { "Include globs" }
                            }
                            input(
                                name: "include_globs",
                                type: "text",
                                value: (include_csv),
                                placeholder: "*.rs, *.md",
                                class: "input input-bordered w-full"
                            );
                        }
                        label(class: "form-control w-full") {
                            div(class: "label") {
                                span(class: "label-text") { "Exclude globs" }
                            }
                            input(
                                name: "exclude_globs",
                                type: "text",
                                value: (exclude_csv),
                                placeholder: "target/, node_modules/",
                                class: "input input-bordered w-full"
                            );
                        }
                        label(class: "form-control w-full") {
                            div(class: "label") { span(class: "label-text") { "Chunk size" } }
                            input(
                                name: "chunk_size",
                                type: "number",
                                value: (chunk_size),
                                min: "1",
                                max: "8000",
                                class: "input input-bordered w-full"
                            );
                        }
                        label(class: "form-control w-full") {
                            div(class: "label") { span(class: "label-text") { "Chunk overlap" } }
                            input(
                                name: "chunk_overlap",
                                type: "number",
                                value: (chunk_overlap),
                                min: "0",
                                class: "input input-bordered w-full"
                            );
                        }
                    }
                    div(class: "card-actions justify-end mt-2 gap-2") {
                        form(
                            action: (cancel_action.clone()),
                            method: "post",
                            class: "m-0 inline",
                            "data-on:submit__prevent": (cancel_directive)
                        ) {
                            button(type: "submit", class: "btn btn-sm btn-outline") { "Cancel" }
                        }
                        button(type: "submit", class: "btn btn-sm btn-primary") { "Save changes" }
                    }
                }
            }
        }
    }
    .to_html()
}

/// Embedding-model `<select>` (with an Other → text-input escape hatch
/// when the operator wants to point at a model the gateway doesn't yet
/// know). When no embedding pools are configured, falls back to a plain
/// text input so the page stays usable in test scaffolding + before any
/// upstream has reported its first `/models` probe. `selected` pre-fills
/// the chosen option in edit forms.
fn embedding_model_field(models: &[String], selected: Option<&str>) -> Html {
    if models.is_empty() {
        let value = selected.unwrap_or("");
        return html! {
            label(class: "form-control w-full") {
                div(class: "label") { span(class: "label-text") { "Embedding model" } }
                input(
                    name: "embedding_model",
                    type: "text",
                    required: "required",
                    value: (value),
                    placeholder: "no embedding pools configured — type a model id",
                    class: "input input-bordered w-full"
                );
            }
        }
        .to_html();
    }
    let options: Vec<(String, bool)> = models
        .iter()
        .map(|m| {
            let is_selected = Some(m.as_str()) == selected;
            (m.clone(), is_selected)
        })
        .collect();
    // If `selected` is set to a model that's no longer in the registry
    // (operator dropped the pool), keep it as the chosen value so the
    // operator can see what's stored — the form will still submit it.
    let stale_selected = selected
        .filter(|s| !s.is_empty() && !models.iter().any(|m| m == s))
        .map(str::to_string);
    html! {
        label(class: "form-control w-full") {
            div(class: "label") { span(class: "label-text") { "Embedding model" } }
            select(
                name: "embedding_model",
                required: "required",
                class: "select select-bordered w-full"
            ) {
                if selected.is_none() && stale_selected.is_none() {
                    option(value: "", disabled: "disabled", selected: "selected") {
                        "Choose an embedding model…"
                    }
                }
                if let Some(stale) = stale_selected.as_ref() {
                    option(value: (stale.clone()), selected: "selected") {
                        (stale.clone()) " (no longer advertised)"
                    }
                }
                for (model, is_selected) in options.iter() {
                    if *is_selected {
                        option(value: (model.clone()), selected: "selected") { (model.clone()) }
                    } else {
                        option(value: (model.clone())) { (model.clone()) }
                    }
                }
            }
        }
    }
    .to_html()
}

fn render_body(list: &[rag_db::Collection], embedding_models: &[String]) -> Html {
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
            div(class: "flex items-center gap-2 mb-2") {
                (icons::folder(20))
                h1(class: "text-2xl font-bold m-0") { "RAG collections" }
            }
            p(class: "text-base-content/60 text-sm mb-6") {
                "Codebases the gateway has indexed. The "
                code(class: "font-mono text-xs") { "rag_search" }
                " tool reaches into these collections to answer questions about the code."
            }

            (render_create_form(embedding_models))

            section(class: "card border border-base-300") {
                div(class: "card-body") {
                    h2(class: "card-title") { "Configured collections" }
                    ul(
                        id: "rag-list",
                        class: "flex flex-col divide-y divide-base-300"
                    ) {
                        for c in list.iter() {
                            (render_row(c))
                        }
                    }
                    if list.is_empty() {
                        p(class: "text-base-content/60 text-sm") {
                            "No collections yet. Create one above."
                        }
                    }
                }
            }
        }
    }
    .to_html()
}
