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

use gateway_core::rama_server::session::Session;
use gateway_core::server::db::rag as rag_db;
use gateway_core::server::db::rag_documents;
use gateway_core::server::db::users;
use gateway_runtime::rama_server::state::RamaState;

/// Wire shape returned from every list / get / update response.
#[derive(Serialize)]
struct CollectionView {
    id: i64,
    name: String,
    description: Option<String>,
    git_url: String,
    git_ref: String,
    pat_set: bool,
    /// Which provider reaches this collection's files: `git`, or a
    /// registered remote source. See `GET /api/v0/rag/providers`.
    source_kind: String,
    /// The provider's non-secret settings. Secrets are never returned;
    /// `source_secrets_set` says whether any are stored.
    source_config: std::collections::BTreeMap<String, String>,
    source_secrets_set: bool,
    /// Extraction profile id, or null. Names are resolved on write; the id
    /// is what the row stores.
    profile_id: Option<i64>,
    extraction_model: Option<String>,
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
            source_kind: c.source.kind,
            source_config: c.source.config,
            source_secrets_set: c.source.secrets.is_some(),
            profile_id: c.profile_id,
            extraction_model: c.extraction_model,
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
    /// `git` (the default) or a registered provider kind.
    #[serde(default = "default_source_kind")]
    source_kind: String,
    /// Flat settings map for the chosen provider, secrets included. Which
    /// keys are secret is the provider's call (`config_fields`), so the
    /// caller sends one map and the server splits and seals it.
    #[serde(default)]
    source_config: std::collections::BTreeMap<String, String>,
    /// Extraction profile to apply to each document, by name. Absent = no
    /// extraction, which is right for a code collection.
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    extraction_model: Option<String>,
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
fn default_source_kind() -> String {
    "git".into()
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
    /// Replace the source. Both must be sent together: a settings map means
    /// nothing without the kind that defines its schema.
    #[serde(default)]
    source_kind: Option<String>,
    #[serde(default)]
    source_config: Option<std::collections::BTreeMap<String, String>>,
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
    if let Err(resp) = require_admin(&state, &req).await {
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
    if let Err(resp) = require_admin(&state, &req).await {
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
    if let Err(resp) = require_admin(&state, &req).await {
        return resp;
    }
    let body = match read_json::<CreateRequest>(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    if body.name.trim().is_empty() || body.name.len() > 64 {
        return invalid_request("`name` must be 1..=64 characters");
    }
    let source = match build_source(&state, &body.source_kind, &body.source_config, None) {
        Ok(spec) => spec,
        Err(msg) => return invalid_request(&msg),
    };
    // A profile is named rather than numbered on the wire: ids are an
    // implementation detail of this gateway's DB, names are what an operator
    // writes in a script.
    let profile_id = match body.profile.as_deref().filter(|p| !p.is_empty()) {
        None => None,
        Some(name) => match rag_documents::find_profile_by_name(&state.db, name).await {
            Ok(Some(p)) => Some(p.id),
            Ok(None) => {
                return invalid_request(&format!(
                    "no extraction profile named `{name}` — see GET /api/v0/rag/profiles"
                ));
            }
            Err(err) => {
                tracing::warn!(error = %err, "looking up extraction profile");
                return internal_error("profile lookup failed");
            }
        },
    };
    // A remote source keeps its location in the provider config, so only a
    // git collection needs a repository URL.
    if source.is_git() && body.git_url.trim().is_empty() {
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
        source,
        profile_id,
        extraction_model: body.extraction_model.filter(|s| !s.is_empty()),
        embedding_model: body.embedding_model.trim().to_string(),
        include_globs: body.include_globs,
        exclude_globs: body.exclude_globs,
        chunk_size: body.chunk_size,
        chunk_overlap: body.chunk_overlap,
        // The JSON API creates single-repo (versioned) collections; aggregate
        // multi-source collections are driven through the /rag admin UI.
        search_mode: rag_db::SearchMode::Versioned,
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

/// Validate a `source_kind` + settings map against its provider and seal the
/// secret half.
///
/// The split between config and secrets is the provider's to make
/// (`ConfigField::kind`), so callers send one flat map and never have to know
/// which keys are sensitive — the same contract the admin form uses.
/// Turn a wire `source_kind` + flat `source_config` into a storable spec.
///
/// `existing` is the currently stored spec on a PATCH. It matters for two
/// reasons that only show up with an OAuth source: a secret the caller did
/// not resend must keep its stored value rather than be dropped, and the
/// refresh token minted by the consent callback is a secret **no caller ever
/// sends** — rebuilding the blob from the request alone would disconnect the
/// collection every time someone edited an unrelated field.
fn build_source(
    state: &RamaState,
    kind: &str,
    config: &std::collections::BTreeMap<String, String>,
    existing: Option<&rag_db::SourceSpec>,
) -> Result<rag_db::SourceSpec, String> {
    use gateway_features::server::rag::source::{AuthKind, FieldKind, ProviderConfig};

    if kind == "git" {
        return Ok(rag_db::SourceSpec::default());
    }
    let registry = source_registry(state);
    let factory = registry.get(kind).ok_or_else(|| {
        let known: Vec<&str> = std::iter::once("git")
            .chain(registry.factories().iter().map(|f| f.kind()))
            .collect();
        format!(
            "unknown `source_kind` `{kind}` (known: {})",
            known.join(", ")
        )
    })?;
    let secret_keys: Vec<&str> = factory
        .config_fields()
        .iter()
        .filter(|f| f.kind == FieldKind::Secret)
        .map(|f| f.key)
        .collect();
    let mut values = std::collections::BTreeMap::new();
    let mut secrets = existing
        .filter(|s| s.kind == kind)
        .map(|s| s.open_secrets(&state.crypto))
        .unwrap_or_default();
    for (k, v) in config {
        if v.is_empty() {
            continue;
        }
        if secret_keys.contains(&k.as_str()) {
            secrets.insert(k.clone(), v.clone());
        } else {
            values.insert(k.clone(), v.clone());
        }
    }
    let cfg = ProviderConfig::new(values.clone(), secrets.clone());
    factory.validate(&cfg).map_err(|e| e.to_string())?;
    // Build once so a malformed URL is a 400 here rather than a failed build
    // discovered later on the indexing timeline.
    //
    // Except before consent: an OAuth source cannot be built until someone
    // has clicked through the provider's consent screen, and that needs a
    // saved collection to hang the flow on. Same rule the web form applies —
    // without it, creating a Drive collection over the API is impossible.
    let awaiting_consent = matches!(factory.auth(), AuthKind::OAuth2 { .. })
        && !secrets.contains_key(gateway_features::server::rag::source::REFRESH_TOKEN_KEY);
    if !awaiting_consent {
        factory
            .build(&cfg, state.http.clone())
            .map_err(|e| e.to_string())?;
    }

    let sealed = if secrets.is_empty() {
        None
    } else {
        let json = serde_json::to_string(&secrets).map_err(|e| e.to_string())?;
        Some(state.crypto.seal_str(&json).map_err(|e| e.to_string())?)
    };
    Ok(rag_db::SourceSpec {
        kind: kind.to_string(),
        config: values,
        secrets: sealed,
    })
}

/// The registry to validate against, falling back to the built-ins when no
/// indexer is wired so the endpoint still describes what exists.
fn source_registry(state: &RamaState) -> &gateway_features::server::rag::source::ProviderRegistry {
    state.provider_registry()
}

/// GET /api/v0/rag/providers — the source kinds this gateway can index, and
/// the settings each one takes.
///
/// Exists so a client can build a form (or a config file) for a provider it
/// has no compiled-in knowledge of — the same descriptors the admin UI
/// renders from.
pub async fn list_providers(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_admin(&state, &req).await {
        return resp;
    }
    use gateway_features::server::rag::source::FieldKind;
    let mut providers = vec![json!({
        "kind": "git",
        "label": "Git repository",
        "description": "Clones a repository and indexes its files.",
        "auth": {"kind": "fields"},
        "fields": [],
    })];
    providers.extend(source_registry(&state).factories().iter().map(|f| {
        let fields: Vec<serde_json::Value> = f
            .config_fields()
            .iter()
            .map(|field| {
                json!({
                    "key": field.key,
                    "label": field.label,
                    "help": field.help,
                    "required": field.required,
                    "kind": field.kind.as_str(),
                    "secret": field.kind == FieldKind::Secret,
                    "default": field.default,
                })
            })
            .collect();
        // How the provider is authorised, so a client can tell "fill in these
        // fields and you are done" from "fill these in, save, then send a
        // human through a browser". Without this a caller cannot explain why
        // its freshly created collection is not indexing.
        let auth = match f.auth() {
            gateway_features::server::rag::source::AuthKind::Fields => json!({"kind": "fields"}),
            gateway_features::server::rag::source::AuthKind::OAuth2 { scopes, .. } => json!({
                "kind": "oauth2",
                "scopes": scopes,
                "connect_path": "/rag/{collection_id}/connect",
            }),
        };
        json!({
            "kind": f.kind(),
            "label": f.label(),
            "description": f.description(),
            "auth": auth,
            "fields": fields,
        })
    }));
    json_ok(&json!({ "data": providers }))
}

/// GET /api/v0/rag/profiles — the extraction profiles this gateway knows,
/// and the fields each one pulls out of a document.
///
/// A collection references a profile **by name** on create/PATCH; this is how
/// a caller discovers which names exist and what querying against one will
/// give them.
pub async fn list_profiles(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_admin(&state, &req).await {
        return resp;
    }
    let profiles = match rag_documents::list_profiles(&state.db).await {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "listing extraction profiles");
            return internal_error("listing profiles failed");
        }
    };
    let data: Vec<serde_json::Value> = profiles
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "description": p.description,
                "version": p.version,
                "fields": p.fields.iter().map(|f| json!({
                    "key": f.key,
                    "label": f.label,
                    "type": f.field_type,
                    "description": f.description,
                    "values": f.values,
                    "filterable": f.filterable,
                    "sortable": f.sortable,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json_ok(&json!({ "data": data }))
}

/// True when `err` is a SQLite UNIQUE-constraint violation; reaches
/// through the `DbError::Query(sqlx::Error::Database(...))` envelope
/// because `DbError`'s `Display` is intentionally terse (`"query"`).
fn is_unique_violation(err: &gateway_core::server::db::DbError) -> bool {
    use gateway_core::server::db::DbError;
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
    if let Err(resp) = require_admin(&state, &req).await {
        return resp;
    }
    let body = match read_json::<UpdateRequest>(req).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let before = match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return not_found(&format!("no collection with id {id}")),
        Err(err) => {
            tracing::warn!(error = %err, %id, "pre-update lookup");
            return internal_error("collection lookup failed");
        }
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
    match (body.source_kind, body.source_config) {
        (Some(kind), Some(config)) => {
            let source = match build_source(&state, &kind, &config, Some(&before.source)) {
                Ok(spec) => spec,
                Err(msg) => return invalid_request(&msg),
            };
            let config_json = serde_json::to_string(&source.config).unwrap_or_else(|_| "{}".into());
            sets.push("source_kind = ?");
            bindings.push(UpdateBinding::Str(source.kind));
            sets.push("source_config_json = ?");
            bindings.push(UpdateBinding::Str(config_json));
            sets.push("source_secrets_ct = ?");
            bindings.push(UpdateBinding::OptBlob(
                source.secrets.as_ref().map(|s| s.ciphertext.clone()),
            ));
            sets.push("source_secrets_nonce = ?");
            bindings.push(UpdateBinding::OptBlob(
                source.secrets.as_ref().map(|s| s.nonce.clone()),
            ));
        }
        (None, None) => {}
        _ => {
            return invalid_request(
                "`source_kind` and `source_config` must be sent together — a settings map \
                 has no meaning without the kind whose schema defines it",
            );
        }
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
            UpdateBinding::OptBlob(b) => q.bind(b),
        };
    }
    q = q.bind(id);
    if let Err(err) = q.execute(&state.db).await {
        tracing::warn!(error = %err, %id, "updating rag collection");
        return internal_error("updating collection failed");
    }
    let after = match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return not_found(&format!("no collection with id {id}")),
        Err(err) => {
            tracing::warn!(error = %err, %id, "post-update lookup");
            return internal_error("collection lookup failed");
        }
    };
    // Same rule as the web editor, from the same predicate: swapping the
    // source, the profile or the embedding model over the API has to re-queue
    // too, or the corpus keeps answering out of a store that no longer matches
    // its own settings.
    if rag_db::index_shape_changed(&before, &after)
        && let Some(indexer) = state.indexer.as_ref()
    {
        for r in rag_db::list_refs(&state.db, id).await.unwrap_or_default() {
            let _ = indexer.request_full_rebuild(r.id).await;
        }
    }
    json_ok(&CollectionView::from(after))
}

enum UpdateBinding {
    OptStr(Option<String>),
    Str(String),
    Int(i64),
    /// Sealed ciphertext / nonce, which are BLOB columns.
    OptBlob(Option<Vec<u8>>),
}

pub async fn delete_collection(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    if let Err(resp) = require_admin(&state, &req).await {
        return resp;
    }
    // Capture every ref's store folder before the cascade delete so we can
    // reap them all (each ref has its own <data_dir>/<uuid>/).
    let refs = rag_db::list_refs(&state.db, id).await.unwrap_or_default();
    match rag_db::delete_collection(&state.db, id).await {
        Ok(true) => {
            if let Some(indexer) = state.indexer.as_ref() {
                for r in &refs {
                    indexer.drop_ref_storage(r.id, &r.data_uuid);
                }
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
    if let Err(resp) = require_admin(&state, &req).await {
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

/// Gate for every `/api/v0/rag/*` handler. The RAG collection registry
/// is an operator-global resource (no per-row owner — see
/// `migrations/0013_rag.sql`), so these endpoints are admin-only, exactly
/// like the HTML surface in `pages::rag_*` which gates on
/// `require_admin_or_403`. Anonymous → 401 JSON; an authenticated
/// non-admin → 403 JSON. Returns the session on success.
async fn require_admin(state: &RamaState, req: &Request) -> Result<Session, Response> {
    let session = match state.sessions.lookup_from_headers(req.headers()).await {
        Ok(Some(s)) => s,
        Ok(None) => return Err(unauthorized("no active session — sign in at /auth/login")),
        Err(err) => {
            tracing::warn!(error = %err, "session lookup");
            return Err(internal_error("session lookup failed"));
        }
    };
    let user = match users::find_by_id(&state.db, &session.user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => return Err(unauthorized("session references a missing user")),
        Err(err) => {
            tracing::warn!(error = %err, "user lookup");
            return Err(internal_error("user lookup failed"));
        }
    };
    let role_ids = state.rbac.role_ids_for(&user.roles);
    if !state.rbac.is_admin(&role_ids) {
        return Err(forbidden("admin role required"));
    }
    Ok(session)
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
fn forbidden(message: &str) -> Response {
    error_envelope(StatusCode::FORBIDDEN, "forbidden", message)
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
