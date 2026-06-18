// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/api/v0/rag/*` — session-authenticated admin API for the RAG
//! collection registry.
//!
//! Wire shapes are kept inline rather than in `shared::api` because they
//! are admin-only (the CLI doesn't speak them) and likely to evolve as
//! the indexer gains knobs. The PAT field is treated as a one-way
//! secret: it can be *set* on create/update, but every response
//! surfaces `pat_set: bool` instead of the plaintext.

use std::sync::Arc;

use jiff::Timestamp;
use rama::http::service::web::extract::{Path, State};
use rama::http::service::web::response::IntoResponse;
use rama::http::{Request, Response, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::rama_server::session::Session;
use crate::rama_server::state::RamaState;
use crate::server::db::rag as rag_db;

/// Wire shape returned from every list / get / update response.
#[derive(Serialize)]
struct CollectionView {
    id: i64,
    name: String,
    description: Option<String>,
    git_url: String,
    git_ref: String,
    pat_set: bool,
    embedding_model: String,
    include_globs: Vec<String>,
    exclude_globs: Vec<String>,
    chunk_size: i64,
    chunk_overlap: i64,
    status: String,
    last_indexed_at: Option<String>,
    last_indexed_commit: Option<String>,
    last_error: Option<String>,
    created_at: String,
    updated_at: String,
}

impl From<rag_db::Collection> for CollectionView {
    fn from(c: rag_db::Collection) -> Self {
        CollectionView {
            id: c.id,
            name: c.name,
            description: c.description,
            git_url: c.git_url,
            git_ref: c.git_ref,
            pat_set: c.pat.is_some(),
            embedding_model: c.embedding_model,
            include_globs: c.include_globs,
            exclude_globs: c.exclude_globs,
            chunk_size: c.chunk_size,
            chunk_overlap: c.chunk_overlap,
            status: c.status.as_str().to_string(),
            last_indexed_at: c.last_indexed_at.map(|t| t.to_string()),
            last_indexed_commit: c.last_indexed_commit,
            last_error: c.last_error,
            created_at: c.created_at.to_string(),
            updated_at: c.updated_at.to_string(),
        }
    }
}

#[derive(Deserialize)]
struct CreateRequest {
    name: String,
    #[serde(default)]
    description: Option<String>,
    git_url: String,
    #[serde(default = "default_ref")]
    git_ref: String,
    #[serde(default)]
    pat: Option<String>,
    embedding_model: String,
    #[serde(default)]
    include_globs: Vec<String>,
    #[serde(default)]
    exclude_globs: Vec<String>,
    #[serde(default = "default_chunk_size")]
    chunk_size: i64,
    #[serde(default = "default_chunk_overlap")]
    chunk_overlap: i64,
}

fn default_ref() -> String {
    "main".into()
}
fn default_chunk_size() -> i64 {
    800
}
fn default_chunk_overlap() -> i64 {
    100
}

#[derive(Deserialize, Default)]
struct UpdateRequest {
    #[serde(default)]
    description: Option<Option<String>>,
    #[serde(default)]
    git_ref: Option<String>,
    /// `Some(Some(token))` → set; `Some(None)` → clear; missing → leave.
    /// Using `Option<Option<String>>` is the canonical "tri-state PATCH"
    /// idiom for fields that are themselves nullable in the model.
    #[serde(default, deserialize_with = "deserialize_option_option")]
    pat: Option<Option<String>>,
    #[serde(default)]
    embedding_model: Option<String>,
    #[serde(default)]
    include_globs: Option<Vec<String>>,
    #[serde(default)]
    exclude_globs: Option<Vec<String>>,
    #[serde(default)]
    chunk_size: Option<i64>,
    #[serde(default)]
    chunk_overlap: Option<i64>,
}

// Distinguish "field omitted" from "field set to null" for the PAT.
fn deserialize_option_option<'de, D>(de: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(Some(Option::<String>::deserialize(de)?))
}

pub async fn list_collections(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_session(&state, &req).await {
        return resp;
    }
    let rows = match rag_db::list_collections(&state.db).await {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(error = %err, "listing rag collections");
            return internal_error("listing collections failed");
        }
    };
    let view: Vec<CollectionView> = rows.into_iter().map(Into::into).collect();
    json_ok(&json!({ "data": view }))
}

pub async fn get_collection(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    if let Err(resp) = require_session(&state, &req).await {
        return resp;
    }
    match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => json_ok(&CollectionView::from(c)),
        Ok(None) => not_found(&format!("no collection with id {id}")),
        Err(err) => {
            tracing::warn!(error = %err, %id, "get rag collection");
            internal_error("collection lookup failed")
        }
    }
}

pub async fn create_collection(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_session(&state, &req).await {
        return resp;
    }
    let body = match read_json::<CreateRequest>(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    if body.name.trim().is_empty() || body.name.len() > 64 {
        return invalid_request("`name` must be 1..=64 characters");
    }
    if body.git_url.trim().is_empty() {
        return invalid_request("`git_url` must not be empty");
    }
    if body.embedding_model.trim().is_empty() {
        return invalid_request("`embedding_model` must not be empty");
    }
    if body.chunk_size <= 0 || body.chunk_size > 8000 {
        return invalid_request("`chunk_size` must be in (0, 8000]");
    }
    if body.chunk_overlap < 0 || body.chunk_overlap >= body.chunk_size {
        return invalid_request("`chunk_overlap` must satisfy 0 <= overlap < chunk_size");
    }
    let new = rag_db::NewCollection {
        name: body.name.trim().to_string(),
        description: body.description.map(|s| s.trim().to_string()),
        git_url: body.git_url.trim().to_string(),
        git_ref: body.git_ref,
        pat: body.pat.filter(|s| !s.is_empty()),
        embedding_model: body.embedding_model.trim().to_string(),
        include_globs: body.include_globs,
        exclude_globs: body.exclude_globs,
        chunk_size: body.chunk_size,
        chunk_overlap: body.chunk_overlap,
    };
    match rag_db::create_collection(&state.db, &new).await {
        Ok(c) => (
            StatusCode::CREATED,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&CollectionView::from(c)).unwrap_or_default(),
        )
            .into_response(),
        // sqlx wraps the underlying sqlite error inside `DbError::Query`;
        // pull it out so the operator gets "name already exists" instead
        // of a vague 500.
        Err(err) => {
            if is_unique_violation(&err) {
                return invalid_request(&format!(
                    "a collection named `{}` already exists",
                    new.name
                ));
            }
            tracing::warn!(error = %err, "creating rag collection");
            internal_error("creating collection failed")
        }
    }
}

/// True when `err` is a SQLite UNIQUE-constraint violation; reaches
/// through the `DbError::Query(sqlx::Error::Database(...))` envelope
/// because `DbError`'s `Display` is intentionally terse (`"query"`).
fn is_unique_violation(err: &crate::server::db::DbError) -> bool {
    use crate::server::db::DbError;
    let DbError::Query(sqlx::Error::Database(db_err)) = err else {
        return false;
    };
    // SQLite uses code "2067" for UNIQUE constraint failures; the
    // string form ("SQLITE_CONSTRAINT_UNIQUE") shows up via `.code()`
    // depending on sqlx version, so check both.
    db_err.code().as_deref() == Some("2067") || db_err.message().contains("UNIQUE")
}

pub async fn update_collection(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    if let Err(resp) = require_session(&state, &req).await {
        return resp;
    }
    let body = match read_json::<UpdateRequest>(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let mut sets: Vec<&'static str> = Vec::new();
    let mut bindings: Vec<UpdateBinding> = Vec::new();
    if let Some(desc) = body.description {
        sets.push("description = ?");
        bindings.push(UpdateBinding::OptStr(desc));
    }
    if let Some(git_ref) = body.git_ref {
        if git_ref.trim().is_empty() {
            return invalid_request("`git_ref` must not be empty");
        }
        sets.push("git_ref = ?");
        bindings.push(UpdateBinding::Str(git_ref));
    }
    if let Some(pat) = body.pat {
        sets.push("pat = ?");
        bindings.push(UpdateBinding::OptStr(pat.filter(|s| !s.is_empty())));
    }
    if let Some(model) = body.embedding_model {
        if model.trim().is_empty() {
            return invalid_request("`embedding_model` must not be empty");
        }
        sets.push("embedding_model = ?");
        bindings.push(UpdateBinding::Str(model));
    }
    if let Some(globs) = body.include_globs {
        let s = match serde_json::to_string(&globs) {
            Ok(s) => s,
            Err(_) => return invalid_request("could not encode include_globs"),
        };
        sets.push("include_globs_json = ?");
        bindings.push(UpdateBinding::Str(s));
    }
    if let Some(globs) = body.exclude_globs {
        let s = match serde_json::to_string(&globs) {
            Ok(s) => s,
            Err(_) => return invalid_request("could not encode exclude_globs"),
        };
        sets.push("exclude_globs_json = ?");
        bindings.push(UpdateBinding::Str(s));
    }
    if let Some(cs) = body.chunk_size {
        if cs <= 0 || cs > 8000 {
            return invalid_request("`chunk_size` must be in (0, 8000]");
        }
        sets.push("chunk_size = ?");
        bindings.push(UpdateBinding::Int(cs));
    }
    if let Some(co) = body.chunk_overlap {
        if co < 0 {
            return invalid_request("`chunk_overlap` must be >= 0");
        }
        sets.push("chunk_overlap = ?");
        bindings.push(UpdateBinding::Int(co));
    }
    if sets.is_empty() {
        // Nothing to do — still surface the current row so the caller
        // can write a UI that doesn't special-case the empty diff.
        return match rag_db::find_collection_by_id(&state.db, id).await {
            Ok(Some(c)) => json_ok(&CollectionView::from(c)),
            Ok(None) => not_found(&format!("no collection with id {id}")),
            Err(err) => {
                tracing::warn!(error = %err, %id, "lookup rag collection");
                internal_error("collection lookup failed")
            }
        };
    }
    let now = Timestamp::now().to_string();
    sets.push("updated_at = ?");
    bindings.push(UpdateBinding::Str(now));
    let sql = format!(
        "UPDATE rag_collections SET {} WHERE id = ?",
        sets.join(", ")
    );
    let mut q = sqlx::query(&sql);
    for b in &bindings {
        q = match b {
            UpdateBinding::OptStr(s) => q.bind(s),
            UpdateBinding::Str(s) => q.bind(s),
            UpdateBinding::Int(i) => q.bind(i),
        };
    }
    q = q.bind(id);
    if let Err(err) = q.execute(&state.db).await {
        tracing::warn!(error = %err, %id, "updating rag collection");
        return internal_error("updating collection failed");
    }
    match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => json_ok(&CollectionView::from(c)),
        Ok(None) => not_found(&format!("no collection with id {id}")),
        Err(err) => {
            tracing::warn!(error = %err, %id, "post-update lookup");
            internal_error("collection lookup failed")
        }
    }
}

enum UpdateBinding {
    OptStr(Option<String>),
    Str(String),
    Int(i64),
}

pub async fn delete_collection(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    if let Err(resp) = require_session(&state, &req).await {
        return resp;
    }
    // Capture the store-folder id before deleting the registry row, so we
    // can reap the on-disk folder afterwards.
    let uuid = rag_db::find_collection_by_id(&state.db, id)
        .await
        .ok()
        .flatten()
        .and_then(|c| c.data_uuid);
    match rag_db::delete_collection(&state.db, id).await {
        Ok(true) => {
            if let (Some(indexer), Some(uuid)) = (state.indexer.as_ref(), uuid) {
                indexer.drop_collection_storage(id, &uuid);
            }
            json_ok(&json!({ "deleted": true }))
        }
        Ok(false) => not_found(&format!("no collection with id {id}")),
        Err(err) => {
            tracing::warn!(error = %err, %id, "delete rag collection");
            internal_error("delete failed")
        }
    }
}

/// POST /api/v0/rag/collections/{id}/reindex — bump back to `pending`
/// so the worker picks it up on the next tick. Clears any prior error.
pub async fn reindex_collection(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    if let Err(resp) = require_session(&state, &req).await {
        return resp;
    }
    if let Err(err) = rag_db::request_reindex(&state.db, id).await {
        tracing::warn!(error = %err, %id, "reindex request");
        return internal_error("reindex request failed");
    }
    match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => json_ok(&CollectionView::from(c)),
        Ok(None) => not_found(&format!("no collection with id {id}")),
        Err(err) => {
            tracing::warn!(error = %err, %id, "post-reindex lookup");
            internal_error("collection lookup failed")
        }
    }
}

// ----- helpers ------------------------------------------------------------

async fn require_session(state: &RamaState, req: &Request) -> Result<Session, Response> {
    match state.sessions.lookup_from_headers(req.headers()).await {
        Ok(Some(s)) => Ok(s),
        Ok(None) => Err(unauthorized("no active session — sign in at /auth/login")),
        Err(err) => {
            tracing::warn!(error = %err, "session lookup");
            Err(internal_error("session lookup failed"))
        }
    }
}

async fn read_json<T: for<'de> Deserialize<'de>>(req: Request) -> Result<T, Response> {
    let (_, body) = req.into_parts();
    let bytes = match body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return Err(invalid_request(&msg)),
    };
    serde_json::from_slice(&bytes).map_err(|err| invalid_request(&format!("invalid body: {err}")))
}

async fn body_to_bytes(body: rama::http::Body) -> Result<rama::bytes::Bytes, String> {
    use rama::http::body::util::BodyExt;
    body.collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| format!("reading request body: {e}"))
}

fn json_ok<T: Serialize>(value: &T) -> Response {
    let body = match serde_json::to_string(value) {
        Ok(s) => s,
        Err(err) => return internal_error(&format!("serialising response: {err}")),
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn not_found(message: &str) -> Response {
    error_envelope(StatusCode::NOT_FOUND, "not_found", message)
}
fn invalid_request(message: &str) -> Response {
    error_envelope(StatusCode::BAD_REQUEST, "invalid_request", message)
}
fn unauthorized(message: &str) -> Response {
    error_envelope(StatusCode::UNAUTHORIZED, "unauthorized", message)
}
fn internal_error(message: &str) -> Response {
    error_envelope(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
}
fn error_envelope(status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({
        "error": {
            "message": message,
            "type": code,
            "code": code,
        }
    });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}
