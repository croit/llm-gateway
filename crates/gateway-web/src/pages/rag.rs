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
//! Live status: while the page is open it polls `GET /rag/status` on a
//! datastar interval and morphs each ref's `#rag-ref-{id}` status row, so
//! the background indexer's progress (`pending` → `cloning` → `indexing`
//! → `ready`/`error`) — and especially *failures* like a branch that
//! doesn't exist — show up without a manual reload. Each ref also has a
//! "Log" button (`GET /rag/refs/{ref_id}/log`) that opens its full
//! indexing timeline; the ref itself only carries the latest `last_error`,
//! the log keeps the history. The poll deliberately re-patches only the
//! status rows, leaving the add-source inputs and any open log untouched.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::{Path, State};
use rama::http::service::web::response::IntoResponse;
use rama::http::{Request, Response};
use serde::Deserialize;

use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_admin_or_403};
use session_core::chrome::{
    Flash, FlashKind, NavSections, Theme, is_datastar_request, read_body_to_bytes, sse_patch,
    sse_response, sse_script, sse_toast,
};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

use gateway_core::server::crypto::Crypto;
use gateway_core::server::db::rag as rag_db;
use gateway_core::server::db::rag_documents as docs_db;
use gateway_core::server::upstreams::PoolKind;
use gateway_features::server::rag::source::ProviderRegistry;
use gateway_runtime::rama_server::state::RamaState;

use super::rag_source;

#[derive(Deserialize)]
struct CreateForm {
    name: String,
    description: Option<String>,
    git_url: String,
    git_ref: Option<String>,
    pat: Option<String>,
    /// Extraction profile to apply, or absent/`0` for none.
    #[serde(default)]
    profile_id: Option<i64>,
    #[serde(default)]
    extraction_model: Option<String>,
    embedding_model: String,
    include_globs: Option<String>,
    exclude_globs: Option<String>,
    chunk_size: Option<i64>,
    chunk_overlap: Option<i64>,
    /// Checkbox: absent when unticked, `Some(..)` when ticked. Aggregate =
    /// one searchable corpus spanning many source repos (each added as a
    /// source); versioned = branches/tags of one repo.
    #[serde(default)]
    aggregate: Option<String>,
}

/// GET /rag — admin-only list of indexed collections with a create
/// form at the top.
pub async fn rag_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
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
    // Pair each collection with its refs for rendering.
    let mut rows: Vec<(rag_db::Collection, Vec<rag_db::CollectionRef>)> =
        Vec::with_capacity(collections.len());
    for c in collections {
        let refs = rag_db::list_refs(&state.db, c.id).await.unwrap_or_default();
        rows.push((c, refs));
    }
    let embedding_models = {
        let mut m = state.upstreams.models_for_kind(PoolKind::Embedding);
        m.sort();
        m
    };
    // Operator-configured default embedding model, but only when it's still
    // advertised — a stale/unset value leaves the create form on its "choose a
    // model" placeholder (we never *fall back* to a model for embeddings, since
    // committing the wrong one would corrupt a collection's vector space).
    let default_embedding = gateway_core::server::feature_defaults::get(
        &state.db,
        gateway_core::server::feature_defaults::Feature::Embedding,
    )
    .await
    .filter(|m| embedding_models.iter().any(|a| a == m));
    let body = render_body(
        lang,
        &rows,
        &embedding_models,
        default_embedding.as_deref(),
        providers(&state),
        &docs_db::list_profiles(&state.db).await.unwrap_or_default(),
    );
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    let title = t(lang, "rag-page-title");
    {
        let pctx = super::PageCtx {
            theme,
            lang,
            nav,
            datastar,
            user_email: user.email.clone(),
            is_admin: is_admin(&state, &user),
            skills_enabled: state.user_skills_enabled(),
            impersonating: session.impersonator_id.is_some(),
        };
        nav_or_html_page(&pctx, NavItem::Rag, &title, body, "/rag", &chat)
    }
}

/// POST /rag — create a new collection. Form-encoded body. SSE response
/// patches the list with the new row and resets the form.
pub async fn rag_create(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
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
        Err(err) => return malformed_form_toast(lang, err),
    };
    let source = match rag_source::to_spec(
        lang,
        rag_source::parse_form(&form_pairs(&body)),
        providers(&state),
        &state.crypto,
        None,
        state.http.clone(),
    ) {
        Ok(spec) => spec,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let new = match validate(lang, form, source) {
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
                    t_args(
                        lang,
                        "rag-toast-name-exists",
                        &i18n::args([("name", new.name.clone().into())]),
                    )
                } else {
                    t(lang, "rag-toast-create-failed")
                },
            );
        }
    };
    // Versioned collections get their first (primary) ref from the form's
    // branch/tag field, kicked to index now. Aggregate collections start
    // empty — the operator adds each source repo below (singly or in bulk).
    let toast_msg = match new.search_mode {
        rag_db::SearchMode::Versioned => {
            match rag_db::add_ref(&state.db, created.id, &new.git_ref, None, true).await {
                Ok(r) => {
                    if let Some(indexer) = state.indexer.as_ref() {
                        let _ = indexer.request_reindex(r.id).await;
                    }
                }
                Err(err) => tracing::warn!(error = %err, "create initial ref"),
            }
            t_args(
                lang,
                "rag-toast-indexing-queued",
                &i18n::args([
                    ("name", created.name.clone().into()),
                    ("ref", new.git_ref.clone().into()),
                ]),
            )
        }
        rag_db::SearchMode::Aggregate => t_args(
            lang,
            "rag-toast-created-aggregate",
            &i18n::args([("name", created.name.clone().into())]),
        ),
    };
    let refs = rag_db::list_refs(&state.db, created.id)
        .await
        .unwrap_or_default();
    let row_html = render_row(lang, &created, &refs).to_string();
    sse_response(&[
        sse_patch(Some("#rag-list"), Some("append"), &row_html),
        sse_script("document.getElementById('rag-create-form').reset()"),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: toast_msg,
        }),
    ])
}

/// The provider registry the forms render from.
///
/// Falls back to the built-in set when no indexer is wired (RAG is not
/// configured on this deployment), so the source picker still renders and
/// the operator sees what *would* be available rather than a form with a
/// silently missing control.
fn providers(state: &RamaState) -> &ProviderRegistry {
    state.provider_registry()
}

/// Decode a urlencoded body into ordered pairs, for the provider fields
/// whose names are not known at compile time.
fn form_pairs(body: &[u8]) -> Vec<(String, String)> {
    serde_urlencoded::from_bytes::<Vec<(String, String)>>(body).unwrap_or_default()
}

/// POST /rag/test-source — reach the source described by the submitted
/// form and report what came back, as a toast.
///
/// Exists because the alternative is finding out that a folder path or an
/// app password is wrong *after* committing to a multi-hour first index, by
/// reading an error on the timeline. `collection_id` (optional) lets an edit
/// form be tested without retyping a stored secret.
pub async fn rag_test_source(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let pairs = form_pairs(&body);
    let parsed = rag_source::parse_form(&pairs);
    if parsed.kind == rag_source::GIT_KIND {
        return toast(FlashKind::Info, t(lang, "rag-source-test-git"));
    }

    // An edit form supplies the collection so its stored secret can stand in
    // for a blank password field — testing must not require retyping it.
    //
    // But only against the settings that secret was stored for: see
    // `stored_secrets_may_stand_in`. Otherwise this probe will present a
    // stored credential to whatever host the form asked for.
    let stored = match pairs.iter().find(|(k, _)| k == "collection_id") {
        Some((_, id)) => match id.parse::<i64>() {
            Ok(id) => rag_db::find_collection_by_id(&state.db, id)
                .await
                .ok()
                .flatten()
                .map(|c| c.source),
            Err(_) => None,
        },
        None => None,
    };
    let registry = providers(&state);
    let existing = match (stored, registry.get(&parsed.kind)) {
        (Some(spec), Some(factory)) => {
            rag_source::stored_secrets_may_stand_in(factory.as_ref(), &parsed.config, &spec)
                .then_some(spec)
        }
        _ => None,
    };

    let provider = match rag_source::provider_for_probe(
        lang,
        parsed,
        registry,
        &state.crypto,
        existing.as_ref(),
        state.http.clone(),
    ) {
        Ok(p) => p,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    match provider.probe().await {
        Ok(report) => {
            let mut message = match report.account.as_deref() {
                Some(account) => t_args(
                    lang,
                    "rag-source-test-ok",
                    &i18n::args([
                        ("account", account.to_string().into()),
                        ("entries", (report.root_entries as i64).into()),
                    ]),
                ),
                None => t_args(
                    lang,
                    "rag-source-test-ok-plain",
                    &i18n::args([("entries", (report.root_entries as i64).into())]),
                ),
            };
            // Whether the server's extensions were detected decides whether
            // this collection gets cheap re-syncs and move-proof identity, so
            // it belongs in the operator's first look at the connection.
            if let Some(server) = report.server.as_deref() {
                message.push(' ');
                message.push_str(&t_args(
                    lang,
                    "rag-source-detected",
                    &i18n::args([("server", server.to_string().into())]),
                ));
            }
            toast(FlashKind::Success, message)
        }
        Err(err) => toast(
            FlashKind::Error,
            t_args(
                lang,
                "rag-source-test-failed",
                &i18n::args([("error", err.to_string().into())]),
            ),
        ),
    }
}

/// The token from `/hooks/rag/{token}`, with its case intact.
///
/// See [`rag_sync_hook`]: the `Path` extractor would lowercase it.
fn sync_hook_token(path: &str) -> Option<String> {
    let tail = path.rsplit_once("/hooks/rag/")?.1;
    let token = tail.trim_end_matches('/');
    (!token.is_empty() && !token.contains('/')).then(|| token.to_string())
}

/// POST /hooks/rag/{token} — re-sync the collection this token belongs to.
///
/// Unauthenticated by design: the token in the URL *is* the credential, the
/// same shape `/hooks/{secret}` uses for user webhooks. Nextcloud's
/// webhook_listeners app (or ownCloud's, or a cron line, or anything that can
/// make an HTTP request) points at this on file events.
///
/// The body is ignored. This is a doorbell, not a change feed: what actually
/// changed is established by the walk that follows, which is cheap on a
/// source that supports subtree pruning. Accepting a payload here would mean
/// trusting an unauthenticated caller's description of the corpus.
pub async fn rag_sync_hook(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    // Read the token off the raw URI, not through `Path`: rama's extractor
    // lowercases every segment, and `rotate_sync_token` mints from a
    // mixed-case alphabet. Through `Path` a token containing any capital —
    // which is to say very nearly all of them — hashes to something that
    // matches no row, and the hook 404s forever.
    let Some(token) = sync_hook_token(req.uri().path()) else {
        return (
            rama::http::StatusCode::NOT_FOUND,
            [(rama::http::header::CONTENT_TYPE, "application/json")],
            r#"{"error":"unknown sync token"}"#,
        )
            .into_response();
    };
    // A missing collection and a wrong token get the same answer: anything
    // else turns this into an oracle for guessing valid tokens.
    let Ok(Some(collection)) = rag_db::find_by_sync_token(&state.db, &token).await else {
        return (
            rama::http::StatusCode::NOT_FOUND,
            [(rama::http::header::CONTENT_TYPE, "application/json")],
            r#"{"error":"unknown sync token"}"#,
        )
            .into_response();
    };
    let Some(indexer) = state.indexer.as_ref() else {
        return (
            rama::http::StatusCode::SERVICE_UNAVAILABLE,
            [(rama::http::header::CONTENT_TYPE, "application/json")],
            r#"{"error":"the indexer is not running"}"#,
        )
            .into_response();
    };
    let refs = rag_db::list_refs(&state.db, collection.id)
        .await
        .unwrap_or_default();
    let mut queued = 0usize;
    for r in &refs {
        // Already-pending refs are left alone: a burst of file events must
        // not re-queue a build that is about to run anyway.
        if r.status == rag_db::CollectionStatus::Pending {
            continue;
        }
        if indexer.request_reindex(r.id).await.is_ok() {
            queued += 1;
        }
    }
    tracing::info!(
        collection = %collection.name,
        queued,
        "rag: sync hook fired"
    );
    (
        rama::http::StatusCode::ACCEPTED,
        [(rama::http::header::CONTENT_TYPE, "application/json")],
        // Serialised, not interpolated: a collection name containing a quote
        // or a backslash would otherwise emit invalid JSON to the caller.
        serde_json::json!({ "collection": collection.name, "queued": queued }).to_string(),
    )
        .into_response()
}

/// POST /rag/{id}/sync-token — mint (or rotate) a collection's sync token.
///
/// The plaintext is shown once, in the toast: only its hash is stored, so
/// there is no way to read it back. Rotating invalidates the old URL, which
/// is the point.
pub async fn rag_sync_token(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let token = match rag_db::rotate_sync_token(&state.db, id).await {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(error = %err, %id, "minting rag sync token");
            return toast(FlashKind::Error, t(lang, "rag-toast-save-failed"));
        }
    };
    let url = format!(
        "{}/hooks/rag/{token}",
        state.config.gateway.public_url.trim_end_matches('/')
    );
    row_patch(
        &state,
        lang,
        id,
        t_args(
            lang,
            "rag-toast-sync-token",
            &i18n::args([("url", url.into())]),
        ),
    )
    .await
}

/// POST /rag/{id}/sync-token/clear — disable the hook.
pub async fn rag_sync_token_clear(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    if let Err(err) = rag_db::clear_sync_token(&state.db, id).await {
        tracing::warn!(error = %err, %id, "clearing rag sync token");
        return toast(FlashKind::Error, t(lang, "rag-toast-save-failed"));
    }
    row_patch(&state, lang, id, t(lang, "rag-toast-sync-token-cleared")).await
}

/// Re-patch a collection's `#rag-row-{id}` with its current refs + a toast.
async fn row_patch(state: &RamaState, lang: Lang, collection_id: i64, msg: String) -> Response {
    row_patch_inner(state, lang, collection_id, msg, None).await
}

/// Like [`row_patch`] but also resets the named form after the patch. Used by
/// the add-source / bulk-add handlers: datastar morphs the row in place, which
/// otherwise preserves the value the operator just typed — making a successful
/// add look like it did nothing.
async fn row_patch_reset(
    state: &RamaState,
    lang: Lang,
    collection_id: i64,
    msg: String,
    reset_form_id: &str,
) -> Response {
    row_patch_inner(state, lang, collection_id, msg, Some(reset_form_id)).await
}

async fn row_patch_inner(
    state: &RamaState,
    lang: Lang,
    collection_id: i64,
    msg: String,
    reset_form_id: Option<&str>,
) -> Response {
    match row_html(state, lang, collection_id).await {
        Some(html) => {
            let selector = format!("#rag-row-{collection_id}");
            let mut events = vec![sse_patch(Some(&selector), Some("outer"), &html)];
            if let Some(form_id) = reset_form_id {
                events.push(sse_script(&format!(
                    "document.getElementById('{form_id}')?.reset()"
                )));
            }
            events.push(sse_toast(&Flash {
                kind: FlashKind::Success,
                message: msg,
            }));
            sse_response(&events)
        }
        None => toast(FlashKind::Error, t(lang, "rag-toast-collection-not-found")),
    }
}

/// Re-queue a ref: flip it to `pending` (so the worker rebuilds it) and,
/// if an indexer is wired, wake it immediately. The DB write is what makes
/// the re-index happen; the kick just makes it prompt.
async fn requeue_ref(state: &RamaState, ref_id: i64) {
    if let Some(indexer) = state.indexer.as_ref() {
        let _ = indexer.request_reindex(ref_id).await;
    } else {
        let _ = rag_db::request_ref_reindex(&state.db, ref_id).await;
    }
}

/// After a source change on an AGGREGATE collection, re-queue its primary ref —
/// the primary holds the one unified index (built from every source), so it
/// must rebuild for the change to take effect. No-op for versioned collections,
/// whose refs build independently. Returns true if it re-queued the primary.
async fn requeue_unified_if_aggregate(state: &RamaState, collection_id: i64) -> bool {
    let Ok(Some(c)) = rag_db::find_collection_by_id(&state.db, collection_id).await else {
        return false;
    };
    if c.search_mode != rag_db::SearchMode::Aggregate {
        return false;
    }
    if let Ok(Some(p)) = rag_db::primary_ref(&state.db, collection_id).await {
        requeue_ref(state, p.id).await;
        return true;
    }
    false
}

/// POST /rag/{id}/reindex — re-queue *all* of a collection's refs.
pub async fn rag_reindex(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let refs = match rag_db::list_refs(&state.db, id).await {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(error = %err, %id, "rag reindex");
            return toast(FlashKind::Error, t(lang, "rag-toast-reindex-queue-failed"));
        }
    };
    // An explicit Re-index means "rebuild it", not "check for changes" —
    // the cheap incremental check already runs on the poll and via the sync
    // hook. Without this there would be no way to force a rebuild of a
    // remote collection at all, since its diff would find nothing changed.
    for r in &refs {
        match state.indexer.as_ref() {
            Some(indexer) => {
                let _ = indexer.request_full_rebuild(r.id).await;
            }
            None => {
                let _ = rag_db::request_full_rebuild(&state.db, r.id).await;
            }
        }
    }
    row_patch(
        &state,
        lang,
        id,
        t_args(
            lang,
            "rag-toast-reindex-queued-count",
            &i18n::args([("count", refs.len().to_string().into())]),
        ),
    )
    .await
}

#[derive(serde::Deserialize)]
struct AddRefForm {
    git_ref: String,
    /// Optional per-source repo URL (aggregate collections). Empty/absent →
    /// inherit the collection's `git_url` (versioned collections).
    #[serde(default)]
    git_url: Option<String>,
}

/// POST /rag/{id}/refs — add a branch/tag/commit ref to a collection and
/// queue its first index.
pub async fn rag_add_ref(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: AddRefForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => return malformed_form_toast(lang, err),
    };
    let git_ref = form.git_ref.trim();
    if git_ref.is_empty() {
        return toast(FlashKind::Error, t(lang, "rag-toast-ref-required"));
    }
    let git_url = form
        .git_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // The first ref of a collection becomes its primary (search default).
    let is_primary = rag_db::list_refs(&state.db, id)
        .await
        .map(|r| r.is_empty())
        .unwrap_or(false);
    match rag_db::add_ref(&state.db, id, git_ref, git_url, is_primary).await {
        Ok(r) => {
            if let Some(indexer) = state.indexer.as_ref() {
                let _ = indexer.request_reindex(r.id).await;
            }
        }
        Err(err) => {
            let s = err.to_string();
            tracing::warn!(error = %err, %id, "add rag ref");
            return toast(
                FlashKind::Error,
                if s.contains("UNIQUE") || s.contains("constraint") {
                    t_args(
                        lang,
                        "rag-toast-ref-exists",
                        &i18n::args([("ref", git_ref.to_string().into())]),
                    )
                } else {
                    t(lang, "rag-toast-add-ref-failed")
                },
            );
        }
    }
    // Aggregate: rebuild the unified index (on the primary) to fold in the
    // new source. (The new source row itself is config-only there.)
    requeue_unified_if_aggregate(&state, id).await;
    row_patch_reset(
        &state,
        lang,
        id,
        t_args(
            lang,
            "rag-toast-indexing-queued-ref",
            &i18n::args([("ref", git_ref.to_string().into())]),
        ),
        &format!("rag-addsrc-{id}"),
    )
    .await
}

#[derive(serde::Deserialize)]
struct BulkAddForm {
    sources: String,
}

/// Parse one bulk-add line into `(git_url, git_ref)`. Format per line:
/// `<url>` or `<url> <ref>` or `<url> @<ref>` (whitespace-separated). Blank
/// lines and `#` comments yield `None`. `default_ref` fills in a missing ref.
fn parse_bulk_line(line: &str, default_ref: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parts = line.split_whitespace();
    let url = parts.next()?.to_string();
    let git_ref = parts
        .next()
        .map(|r| r.trim_start_matches('@'))
        .filter(|r| !r.is_empty())
        .unwrap_or(default_ref)
        .to_string();
    Some((url, git_ref))
}

/// POST /rag/{id}/refs/bulk — add many sources at once (one repo per line).
/// The ergonomic path for aggregate collections like Proxmox (~40 repos).
/// Each line lacking an explicit ref inherits the collection's `git_ref`.
pub async fn rag_add_sources_bulk(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let collection = match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return toast(FlashKind::Error, t(lang, "rag-toast-collection-not-found")),
        Err(err) => {
            tracing::warn!(error = %err, %id, "bulk add: lookup");
            return toast(
                FlashKind::Error,
                t(lang, "rag-toast-load-collection-failed"),
            );
        }
    };
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: BulkAddForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => return malformed_form_toast(lang, err),
    };
    let parsed: Vec<(String, String)> = form
        .sources
        .lines()
        .filter_map(|l| parse_bulk_line(l, &collection.git_ref))
        .collect();
    if parsed.is_empty() {
        return toast(FlashKind::Error, t(lang, "rag-toast-no-source-urls"));
    }
    let had_refs = rag_db::list_refs(&state.db, id)
        .await
        .map(|r| !r.is_empty())
        .unwrap_or(false);
    let mut added = 0usize;
    let mut skipped = 0usize;
    for (i, (url, git_ref)) in parsed.iter().enumerate() {
        // First source of an empty collection becomes primary (harmless in
        // aggregate mode, where search ignores primacy, but keeps the
        // one-primary invariant satisfied for the UI).
        let is_primary = !had_refs && i == 0;
        match rag_db::add_ref(&state.db, id, git_ref, Some(url.as_str()), is_primary).await {
            Ok(r) => {
                added += 1;
                if let Some(indexer) = state.indexer.as_ref() {
                    let _ = indexer.request_reindex(r.id).await;
                }
            }
            // A duplicate (same url+ref already present) is skipped, not fatal —
            // bulk re-paste should be idempotent.
            Err(_) => skipped += 1,
        }
    }
    // Aggregate: rebuild the unified index (primary) once, covering all the
    // newly-added sources.
    requeue_unified_if_aggregate(&state, id).await;
    let msg = if skipped > 0 {
        t_args(
            lang,
            "rag-toast-bulk-queued-skipped",
            &i18n::args([
                ("added", added.to_string().into()),
                ("skipped", skipped.to_string().into()),
            ]),
        )
    } else {
        t_args(
            lang,
            "rag-toast-bulk-queued",
            &i18n::args([("added", added.to_string().into())]),
        )
    };
    row_patch_reset(&state, lang, id, msg, &format!("rag-bulk-{id}")).await
}

/// POST /rag/refs/{ref_id}/reindex — re-queue a single ref.
pub async fn rag_ref_reindex(
    State(state): State<Arc<RamaState>>,
    Path(ref_id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let Ok(Some(r)) = rag_db::find_ref_by_id(&state.db, ref_id).await else {
        return toast(FlashKind::Error, t(lang, "rag-toast-ref-not-found"));
    };
    // Aggregate: there's one unified index (on the primary); re-index rebuilds
    // the whole collection. Versioned: re-index just this ref.
    if !requeue_unified_if_aggregate(&state, r.collection_id).await {
        requeue_ref(&state, ref_id).await;
    }
    row_patch(
        &state,
        lang,
        r.collection_id,
        t_args(
            lang,
            "rag-toast-reindex-queued-ref",
            &i18n::args([("ref", r.git_ref.clone().into())]),
        ),
    )
    .await
}

/// POST /rag/refs/{ref_id}/primary — make this ref the search default.
pub async fn rag_ref_set_primary(
    State(state): State<Arc<RamaState>>,
    Path(ref_id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let Ok(Some(r)) = rag_db::find_ref_by_id(&state.db, ref_id).await else {
        return toast(FlashKind::Error, t(lang, "rag-toast-ref-not-found"));
    };
    if let Err(err) = rag_db::set_primary(&state.db, ref_id).await {
        tracing::warn!(error = %err, %ref_id, "set primary ref");
        return toast(FlashKind::Error, t(lang, "rag-toast-set-primary-failed"));
    }
    row_patch(
        &state,
        lang,
        r.collection_id,
        t_args(
            lang,
            "rag-toast-now-default",
            &i18n::args([("ref", r.git_ref.clone().into())]),
        ),
    )
    .await
}

/// POST /rag/refs/{ref_id}/delete — drop one ref + its store folder.
pub async fn rag_ref_delete(
    State(state): State<Arc<RamaState>>,
    Path(ref_id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let Ok(Some(r)) = rag_db::find_ref_by_id(&state.db, ref_id).await else {
        return toast(FlashKind::Error, t(lang, "rag-toast-ref-not-found"));
    };
    let collection_id = r.collection_id;
    match rag_db::delete_ref(&state.db, ref_id).await {
        Ok(uuid) => {
            if let (Some(indexer), Some(uuid)) = (state.indexer.as_ref(), uuid) {
                indexer.drop_ref_storage(ref_id, &uuid);
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, %ref_id, "delete rag ref");
            return toast(FlashKind::Error, t(lang, "rag-toast-delete-ref-failed"));
        }
    }
    // Aggregate: rebuild the unified index (on the possibly-newly-promoted
    // primary) so the removed source drops out of the corpus.
    requeue_unified_if_aggregate(&state, collection_id).await;
    row_patch(
        &state,
        lang,
        collection_id,
        t_args(
            lang,
            "rag-toast-ref-removed",
            &i18n::args([("ref", r.git_ref.clone().into())]),
        ),
    )
    .await
}

/// GET /rag/refs/{ref_id}/log — open a ref's indexing timeline. Fills the
/// `#rag-reflog-{ref_id}` container with the recorded events (newest first):
/// every build's clone/ready/error and any advisory. This is the "why did it
/// fail" surface — the ref carries only the latest `last_error`; the log keeps
/// the history.
pub async fn rag_ref_log(
    State(state): State<Arc<RamaState>>,
    Path(ref_id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let entries = match rag_db::list_log_entries(&state.db, ref_id, 30).await {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(error = %err, %ref_id, "rag: load ref log");
            return toast(FlashKind::Error, t(lang, "rag-toast-load-log-failed"));
        }
    };
    let selector = format!("#rag-reflog-{ref_id}");
    let html = render_ref_log(lang, ref_id, &entries).to_string();
    sse_response(&[sse_patch(Some(&selector), Some("inner"), &html)])
}

/// POST /rag/refs/{ref_id}/edit-form — open an inline editor for a single
/// source's clone target (its Git URL + branch/tag). Rendered into the ref's
/// `#rag-reflog-{ref_id}` sibling container — the same spot the Log uses —
/// because the status poll re-patches `#rag-ref-{id}` every few seconds and
/// would otherwise clobber a form placed there. This is how an aggregate
/// collection's per-source repo URLs get edited (the collection-level Edit
/// only carries the single versioned URL).
pub async fn rag_ref_edit_form(
    State(state): State<Arc<RamaState>>,
    Path(ref_id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let Ok(Some(r)) = rag_db::find_ref_by_id(&state.db, ref_id).await else {
        return toast(FlashKind::Error, t(lang, "rag-toast-ref-not-found"));
    };
    let Ok(Some(c)) = rag_db::find_collection_by_id(&state.db, r.collection_id).await else {
        return toast(FlashKind::Error, t(lang, "rag-toast-collection-not-found"));
    };
    let selector = format!("#rag-reflog-{ref_id}");
    let html = render_ref_edit_form(lang, &c, &r).to_string();
    sse_response(&[sse_patch(Some(&selector), Some("inner"), &html)])
}

/// POST /rag/refs/{ref_id}/cancel-edit — abandon a source edit; just clear the
/// inline editor container (leaving the live row untouched).
pub async fn rag_ref_cancel_edit(
    State(state): State<Arc<RamaState>>,
    Path(ref_id): Path<i64>,
    req: Request,
) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let selector = format!("#rag-reflog-{ref_id}");
    sse_response(&[sse_patch(Some(&selector), Some("inner"), "")])
}

#[derive(Deserialize)]
struct RefUpdateForm {
    /// Per-source repo URL. Empty → inherit the collection's `git_url`
    /// (how versioned refs and un-overridden sources work).
    git_url: Option<String>,
    git_ref: Option<String>,
}

/// POST /rag/refs/{ref_id}/update — save an edited source's Git URL + ref, then
/// re-queue so the new target is fetched. Aggregate: rebuilds the unified index
/// (the URL change only matters once the corpus is rebuilt); versioned: rebuilds
/// this ref. Patches the whole collection row back (which also removes the
/// inline editor from the reflog container).
pub async fn rag_ref_update(
    State(state): State<Arc<RamaState>>,
    Path(ref_id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: RefUpdateForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => return malformed_form_toast(lang, err),
    };
    let Ok(Some(existing)) = rag_db::find_ref_by_id(&state.db, ref_id).await else {
        return toast(FlashKind::Error, t(lang, "rag-toast-ref-not-found"));
    };
    let git_ref = form
        .git_ref
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(existing.git_ref.as_str())
        .to_string();
    // Empty URL means "inherit the collection URL" (stored as NULL). Aggregate
    // sources need a URL somewhere: their collection has none, so an empty
    // source URL would leave the source pointing nowhere — reject it.
    let git_url = form.git_url.as_deref().map(str::trim).unwrap_or("");
    let collection = rag_db::find_collection_by_id(&state.db, existing.collection_id)
        .await
        .ok()
        .flatten();
    if git_url.is_empty()
        && collection
            .as_ref()
            .is_some_and(|c| c.search_mode == rag_db::SearchMode::Aggregate)
    {
        return toast(
            FlashKind::Error,
            t(lang, "rag-toast-git-url-required-aggregate"),
        );
    }
    let git_url_opt = (!git_url.is_empty()).then_some(git_url);
    let collection_id = match rag_db::update_ref(&state.db, ref_id, git_url_opt, &git_ref).await {
        Ok(Some(cid)) => cid,
        Ok(None) => return toast(FlashKind::Error, t(lang, "rag-toast-ref-not-found")),
        Err(err) => {
            tracing::warn!(error = %err, %ref_id, "update rag ref");
            return toast(FlashKind::Error, t(lang, "rag-toast-update-source-failed"));
        }
    };
    // Fetch the new target: aggregate rebuilds the unified (primary) index so
    // the changed source is re-folded in; versioned rebuilds just this ref.
    if !requeue_unified_if_aggregate(&state, collection_id).await {
        requeue_ref(&state, ref_id).await;
    }
    row_patch(
        &state,
        lang,
        collection_id,
        t(lang, "rag-toast-source-updated"),
    )
    .await
}

/// Inline per-source editor: Git URL + branch/tag, with Save / Cancel. Lives in
/// the ref's `#rag-reflog-{id}` container. Mirrors the collection edit form's
/// datastar wiring (`@post` on submit, morph the response in).
fn render_ref_edit_form(lang: Lang, c: &rag_db::Collection, r: &rag_db::CollectionRef) -> Html {
    let update_action = format!("/rag/refs/{}/update", r.id);
    let cancel_action = format!("/rag/refs/{}/cancel-edit", r.id);
    let update_directive = format!("@post('{update_action}', {{contentType: 'form'}})");
    let cancel_directive = format!("@post('{cancel_action}', {{contentType: 'form'}})");
    let aggregate = c.search_mode == rag_db::SearchMode::Aggregate;
    // Aggregate sources carry their own URL; versioned refs inherit the
    // collection's (leave blank), so show the stored override if any.
    let url_value = r.git_url.clone().unwrap_or_default();
    let url_label = if aggregate {
        t(lang, "rag-label-git-url-source")
    } else {
        t(lang, "rag-label-git-url-inherit")
    };
    html! {
        div(class: "mt-1 mb-1 rounded border border-base-300 bg-base-200/40 p-3") {
            form(
                action: (update_action),
                method: "post",
                class: "flex flex-col gap-2",
                "data-on:submit__prevent": (update_directive)
            ) {
                div(class: "grid grid-cols-1 md:grid-cols-2 gap-3") {
                    label(class: "flex flex-col gap-1 w-full") {
                        div(class: "label") { span(class: "label-text text-xs") { (url_label) } }
                        input(
                            name: "git_url",
                            type: "text",
                            value: (url_value),
                            placeholder: (t(lang, "rag-placeholder-git-url")),
                            class: "input input-bordered input-sm w-full"
                        );
                    }
                    label(class: "flex flex-col gap-1 w-full") {
                        div(class: "label") { span(class: "label-text text-xs") { (t(lang, "rag-label-branch-tag")) } }
                        input(
                            name: "git_ref",
                            type: "text",
                            value: (r.git_ref.clone()),
                            class: "input input-bordered input-sm w-full"
                        );
                    }
                }
                div(class: "flex items-center gap-2") {
                    button(type: "submit", class: "btn btn-xs btn-primary") { (t(lang, "rag-button-save-source")) }
                    button(
                        type: "button",
                        class: "btn btn-xs btn-ghost",
                        "data-on:click": (cancel_directive)
                    ) { (t(lang, "rag-button-cancel")) }
                }
            }
        }
    }
    .to_html()
}

/// GET /rag/status — live-status poll target. Re-patches each ref's status
/// row (`#rag-ref-{id}`) with its current badge, last-indexed provenance, and
/// any error/advisory, so the admin sees the background indexer's progress
/// without reloading. Deliberately patches *only* the `#rag-ref-*` rows (not
/// whole collection rows) so the add-source inputs and an open log are left
/// alone. Cheap: a couple of indexed reads + a small render per ref.
pub async fn rag_status(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let collections = rag_db::list_collections(&state.db)
        .await
        .unwrap_or_default();
    let mut events: Vec<rama::bytes::Bytes> = Vec::new();
    for c in &collections {
        let refs = rag_db::list_refs(&state.db, c.id).await.unwrap_or_default();
        let primary = refs.iter().find(|r| r.is_primary);
        for r in &refs {
            let selector = format!("#rag-ref-{}", r.id);
            let html = render_ref(lang, c, r, primary).to_string();
            events.push(sse_patch(Some(&selector), Some("outer"), &html));
        }
    }
    sse_response(&events)
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
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let collection = match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return toast(
                FlashKind::Error,
                t(lang, "rag-toast-collection-not-found-cap"),
            );
        }
        Err(err) => {
            tracing::warn!(error = %err, %id, "lookup rag collection");
            return toast(
                FlashKind::Error,
                t(lang, "rag-toast-load-collection-failed-cap"),
            );
        }
    };
    let mut models = state.upstreams.models_for_kind(PoolKind::Embedding);
    models.sort();
    let selector = format!("#rag-row-{id}");
    sse_response(&[sse_patch(
        Some(&selector),
        Some("outer"),
        &render_edit_form(
            lang,
            &collection,
            &models,
            providers(&state),
            &docs_db::list_profiles(&state.db).await.unwrap_or_default(),
            &state.crypto,
        )
        .to_string(),
    )])
}

/// POST /rag/{id}/cancel-edit — SSE-swap the row back to its
/// display form. The user gave up on the edit; nothing is saved.
pub async fn rag_cancel_edit(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let Some(html) = row_html(&state, lang, id).await else {
        return toast(
            FlashKind::Error,
            t(lang, "rag-toast-collection-not-found-cap"),
        );
    };
    let selector = format!("#rag-row-{id}");
    sse_response(&[sse_patch(Some(&selector), Some("outer"), &html)])
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
    #[serde(default)]
    profile_id: Option<i64>,
    #[serde(default)]
    extraction_model: Option<String>,
    embedding_model: String,
    include_globs: Option<String>,
    exclude_globs: Option<String>,
    chunk_size: Option<i64>,
    chunk_overlap: Option<i64>,
    /// Comma-separated gateway groups allowed to list + search this collection.
    /// Empty = unrestricted. See `db::gateway_groups`.
    #[serde(default)]
    allowed_groups: Option<String>,
}

/// POST /rag/{id}/update — save the edited form. Patches the row back
/// to its display shape; toasts a success / error message.
pub async fn rag_update(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
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
        Err(err) => return malformed_form_toast(lang, err),
    };

    // Pull the current row so we can resolve "leave unchanged" semantics
    // on PAT and ground the success toast in a stable name.
    let existing = match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return toast(
                FlashKind::Error,
                t(lang, "rag-toast-collection-not-found-cap"),
            );
        }
        Err(err) => {
            tracing::warn!(error = %err, %id, "lookup rag collection");
            return toast(
                FlashKind::Error,
                t(lang, "rag-toast-load-collection-failed-cap"),
            );
        }
    };

    // The source is resolved against the stored spec, so a secret left blank
    // keeps its stored value rather than being wiped.
    let source = match rag_source::to_spec(
        lang,
        rag_source::parse_form(&form_pairs(&body)),
        providers(&state),
        &state.crypto,
        Some(&existing.source),
        state.http.clone(),
    ) {
        Ok(spec) => spec,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    // Changing where a collection's documents come from invalidates what is
    // indexed, so the operator gets told to re-index rather than left with a
    // corpus that silently belongs to the old source.

    let git_url = form.git_url.trim();
    // Aggregate collections carry no single repo URL (each source has its
    // own), so the collection-level Git URL is optional for them — only
    // versioned git collections require it.
    if git_url.is_empty()
        && source.is_git()
        && existing.search_mode == rag_db::SearchMode::Versioned
    {
        return toast(FlashKind::Error, t(lang, "rag-toast-git-url-required"));
    }
    let embedding_model = form.embedding_model.trim();
    if embedding_model.is_empty() {
        return toast(
            FlashKind::Error,
            t(lang, "rag-toast-embedding-model-required"),
        );
    }
    let git_ref = form
        .git_ref
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string());
    let chunk_size = form.chunk_size.unwrap_or(existing.chunk_size);
    let chunk_overlap = form.chunk_overlap.unwrap_or(existing.chunk_overlap);
    if chunk_size <= 0 || chunk_size > 8000 {
        return toast(FlashKind::Error, t(lang, "rag-toast-chunk-size-range"));
    }
    if chunk_overlap < 0 || chunk_overlap >= chunk_size {
        return toast(FlashKind::Error, t(lang, "rag-toast-chunk-overlap-range"));
    }
    let description = form
        .description
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let include_globs = split_globs(form.include_globs);
    let exclude_globs = split_globs(form.exclude_globs);
    let include_json = serde_json::to_string(&include_globs).unwrap_or_else(|_| "[]".into());
    let exclude_json = serde_json::to_string(&exclude_globs).unwrap_or_else(|_| "[]".into());
    let source_config_json = serde_json::to_string(&source.config).unwrap_or_else(|_| "{}".into());
    // `0` is the form's "no extraction" option.
    let profile_id = form.profile_id.filter(|id| *id > 0);
    // Blank means "use the pool's default model", which is a real choice — so
    // an empty field clears the override rather than silently keeping the old
    // one. It is not a secret; there is nothing to protect by refusing to let
    // it go back to empty.
    let extraction_model = form
        .extraction_model
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
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
               profile_id = ?,
               extraction_model = ?,
               source_kind = ?,
               source_config_json = ?,
               source_secrets_ct = ?,
               source_secrets_nonce = ?,
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
    .bind(profile_id)
    .bind(&extraction_model)
    .bind(&source.kind)
    .bind(&source_config_json)
    .bind(source.secrets.as_ref().map(|s| s.ciphertext.clone()))
    .bind(source.secrets.as_ref().map(|s| s.nonce.clone()))
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
        return toast(FlashKind::Error, t(lang, "rag-toast-save-failed"));
    }
    // Per-group access list (comma-separated gateway groups; empty = all).
    let allowed_groups = super::parse_csv(form.allowed_groups.as_deref().unwrap_or(""));
    if let Err(err) = rag_db::set_allowed_groups(&state.db, id, &allowed_groups).await {
        tracing::warn!(error = %err, %id, "update rag allowed_groups");
        return toast(FlashKind::Error, t(lang, "rag-toast-save-failed"));
    }
    let updated = match rag_db::find_collection_by_id(&state.db, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return toast(FlashKind::Error, t(lang, "rag-toast-vanished")),
        Err(err) => {
            tracing::warn!(error = %err, %id, "post-update lookup");
            return toast(FlashKind::Error, t(lang, "rag-toast-saved-reload-failed"));
        }
    };
    // Pointing a collection at a different source — or asking for a different
    // set of extracted fields — invalidates what is indexed, so re-queue
    // rather than leave a corpus that silently answers with the old shape.
    // Derived from the saved rows so this surface and the API can't drift.
    if rag_db::index_shape_changed(&existing, &updated)
        && let Some(indexer) = state.indexer.as_ref()
    {
        for r in rag_db::list_refs(&state.db, id).await.unwrap_or_default() {
            let _ = indexer.request_full_rebuild(r.id).await;
        }
    }
    let refs = rag_db::list_refs(&state.db, id).await.unwrap_or_default();
    let selector = format!("#rag-row-{id}");
    sse_response(&[
        sse_patch(
            Some(&selector),
            Some("outer"),
            &render_row(lang, &updated, &refs).to_string(),
        ),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: t_args(
                lang,
                "rag-toast-saved",
                &i18n::args([("name", existing.name.clone().into())]),
            ),
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
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
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
            let selector = format!("#rag-row-{id}");
            sse_response(&[
                sse_patch(Some(&selector), Some("remove"), ""),
                sse_toast(&Flash {
                    kind: FlashKind::Success,
                    message: t(lang, "rag-toast-collection-removed"),
                }),
            ])
        }
        Ok(false) => toast(
            FlashKind::Info,
            t(lang, "rag-toast-collection-already-gone"),
        ),
        Err(err) => {
            tracing::warn!(error = %err, %id, "rag delete");
            toast(FlashKind::Error, t(lang, "rag-toast-delete-failed"))
        }
    }
}

fn toast(kind: FlashKind, message: impl Into<String>) -> Response {
    sse_response(&[sse_toast(&Flash {
        kind,
        message: message.into(),
    })])
}

/// Shorthand for the "form body didn't parse" error toast every form
/// handler falls back to on a `serde_urlencoded` failure.
fn malformed_form_toast(lang: Lang, err: impl std::fmt::Display) -> Response {
    toast(
        FlashKind::Error,
        t_args(
            lang,
            "rag-toast-malformed-form",
            &i18n::args([("err", err.to_string().into())]),
        ),
    )
}

fn validate(
    lang: Lang,
    form: CreateForm,
    source: rag_db::SourceSpec,
) -> Result<rag_db::NewCollection, String> {
    // `0` is the form's "no extraction" option: a code collection has no
    // fields worth pulling out, and the pass costs one LLM call per file.
    let profile_id = form.profile_id.filter(|id| *id > 0);
    let extraction_model = form
        .extraction_model
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let name = form.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(t(lang, "rag-toast-name-length"));
    }
    let search_mode = if form.aggregate.is_some() {
        rag_db::SearchMode::Aggregate
    } else {
        rag_db::SearchMode::Versioned
    };
    let git_url = form.git_url.trim();
    // Aggregate collections carry no single repo — each source brings its
    // own URL — so the collection-level Git URL is optional there. A remote
    // source has no repo at all: its location lives in the provider config.
    if git_url.is_empty() && source.is_git() && search_mode == rag_db::SearchMode::Versioned {
        return Err(t(lang, "rag-toast-git-url-required"));
    }
    let embedding_model = form.embedding_model.trim();
    if embedding_model.is_empty() {
        return Err(t(lang, "rag-toast-embedding-model-required"));
    }
    let chunk_size = form.chunk_size.unwrap_or(800);
    let chunk_overlap = form.chunk_overlap.unwrap_or(100);
    if chunk_size <= 0 || chunk_size > 8000 {
        return Err(t(lang, "rag-toast-chunk-size-range"));
    }
    if chunk_overlap < 0 || chunk_overlap >= chunk_size {
        return Err(t(lang, "rag-toast-chunk-overlap-range"));
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
        source,
        profile_id,
        extraction_model,
        embedding_model: embedding_model.to_string(),
        include_globs: split_globs(form.include_globs),
        exclude_globs: split_globs(form.exclude_globs),
        chunk_size,
        chunk_overlap,
        search_mode,
    })
}

fn split_globs(raw: Option<String>) -> Vec<String> {
    raw.unwrap_or_default()
        .split([',', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn status_badge(lang: Lang, status: rag_db::CollectionStatus) -> Html {
    let (cls, key) = match status {
        rag_db::CollectionStatus::Pending => ("badge badge-outline", "rag-status-pending"),
        rag_db::CollectionStatus::Cloning => ("badge badge-info", "rag-status-cloning"),
        rag_db::CollectionStatus::Indexing => ("badge badge-info", "rag-status-indexing"),
        rag_db::CollectionStatus::Ready => ("badge badge-success", "rag-status-ready"),
        rag_db::CollectionStatus::Error => ("badge badge-error", "rag-status-error"),
    };
    let label = t(lang, key);
    html! {
        span(class: (cls)) { (label) }
    }
    .to_html()
}

fn render_row(lang: Lang, c: &rag_db::Collection, refs: &[rag_db::CollectionRef]) -> Html {
    let dom_id = format!("rag-row-{}", c.id);
    let delete_action = format!("/rag/{}/delete", c.id);
    let edit_action = format!("/rag/{}/edit-form", c.id);
    let add_ref_action = format!("/rag/{}/refs", c.id);
    let delete_directive = format!("@post('{delete_action}', {{contentType: 'form'}})");
    let edit_directive = format!("@post('{edit_action}', {{contentType: 'form'}})");
    let add_ref_directive = format!("@post('{add_ref_action}', {{contentType: 'form'}})");
    let sync_token_action = format!("/rag/{}/sync-token", c.id);
    let sync_clear_action = format!("/rag/{}/sync-token/clear", c.id);
    let sync_token_directive = format!("@post('{sync_token_action}', {{contentType: 'form'}})");
    let sync_clear_directive = format!("@post('{sync_clear_action}', {{contentType: 'form'}})");
    let sync_hook_set = c.sync_hook_set;
    let sync_token_label = if sync_hook_set {
        t(lang, "rag-button-sync-token-rotate")
    } else {
        t(lang, "rag-button-sync-token")
    };
    let bulk_action = format!("/rag/{}/refs/bulk", c.id);
    let bulk_directive = format!("@post('{bulk_action}', {{contentType: 'form'}})");
    // Stable form ids so the add-source / bulk handlers can reset the form
    // after a successful submit (datastar morph otherwise keeps the typed
    // value in the field, which reads as "did nothing").
    let add_src_form_id = format!("rag-addsrc-{}", c.id);
    let bulk_form_id = format!("rag-bulk-{}", c.id);
    let pat_hint = if c.pat.is_some() {
        t(lang, "rag-pat-set")
    } else {
        t(lang, "rag-pat-none")
    };
    let description = c.description.clone().unwrap_or_default();
    // Aggregate source rows mirror the primary's build lifecycle (one unified
    // index), so hand each row the primary to read status/provenance from.
    let primary = refs.iter().find(|r| r.is_primary);
    let aggregate = c.search_mode == rag_db::SearchMode::Aggregate;
    // Aggregate collections have no single repo URL — summarise by source
    // count instead. Versioned ones show their one repo.
    let meta_line = if aggregate {
        t_args(
            lang,
            "rag-meta-aggregate",
            &i18n::args([
                ("count", refs.len().to_string().into()),
                ("hint", pat_hint.clone().into()),
            ]),
        )
    } else {
        t_args(
            lang,
            "rag-meta-versioned",
            &i18n::args([
                ("url", c.git_url.clone().into()),
                ("hint", pat_hint.clone().into()),
            ]),
        )
    };
    html! {
        li(
            id: (dom_id),
            class: "flex flex-col gap-2 py-4"
        ) {
            div(class: "flex items-start gap-4") {
                div(class: "flex-1 min-w-0") {
                    div(class: "flex items-center gap-2") {
                        span(class: "text-base font-medium") { (c.name.clone()) }
                        if aggregate {
                            span(class: "badge badge-sm badge-secondary") { (t(lang, "rag-badge-aggregate")) }
                        }
                        if sync_hook_set {
                            span(class: "badge badge-sm badge-ghost") { (t(lang, "rag-badge-sync-hook")) }
                        }
                    }
                    if !description.is_empty() {
                        p(class: "text-sm text-base-content/70 mt-0.5") { (description) }
                    }
                    p(class: "text-xs text-base-content/60 mt-1 font-mono break-all") {
                        (meta_line)
                    }
                    p(class: "text-xs text-base-content/60 mt-1") {
                        (t(lang, "rag-embed-prefix")) " " (c.embedding_model.clone())
                    }
                }
                div(class: "flex flex-col gap-2 shrink-0") {
                    form(
                        action: (edit_action.clone()),
                        method: "post",
                        class: "m-0",
                        "data-on:submit__prevent": (edit_directive)
                    ) {
                        button(type: "submit", class: "btn btn-sm btn-outline") { (t(lang, "rag-button-edit")) }
                    }
                    form(
                        action: (delete_action.clone()),
                        method: "post",
                        class: "m-0",
                        "data-on:submit__prevent": (delete_directive)
                    ) {
                        button(type: "submit", class: "btn btn-sm btn-outline btn-error") { (t(lang, "rag-button-delete-collection")) }
                    }
                    // The trigger URL a file host's webhook points at. Minted
                    // here because the plaintext is shown once and never
                    // stored — rotating is the only way to get a new one.
                    form(
                        action: (sync_token_action.clone()),
                        method: "post",
                        class: "m-0",
                        "data-on:submit__prevent": (sync_token_directive)
                    ) {
                        button(type: "submit", class: "btn btn-sm btn-ghost") { (sync_token_label.clone()) }
                    }
                    if sync_hook_set {
                        form(
                            action: (sync_clear_action.clone()),
                            method: "post",
                            class: "m-0",
                            "data-on:submit__prevent": (sync_clear_directive)
                        ) {
                            button(type: "submit", class: "btn btn-sm btn-ghost") { (t(lang, "rag-button-sync-token-clear")) }
                        }
                    }
                }
            }
            // Per-ref/source rows: each indexed independently in its own store.
            div(class: "mt-1 pl-3 border-l border-base-300 flex flex-col gap-1.5") {
                for r in refs.iter() {
                    (render_ref(lang, c, r, primary))
                    // Empty container the "Log" button fills in on demand. Kept
                    // OUTSIDE `render_ref` so the status poll (which re-patches
                    // `#rag-ref-{id}`) doesn't wipe an opened log.
                    div(id: (format!("rag-reflog-{}", r.id))) {}
                }
                // Add-source form. Aggregate collections take a repo URL plus
                // an optional ref; versioned ones just a ref of the one repo.
                form(
                    id: (add_src_form_id),
                    action: (add_ref_action),
                    method: "post",
                    class: "flex items-center gap-2 mt-1 flex-wrap",
                    "data-on:submit__prevent": (add_ref_directive)
                ) {
                    if aggregate {
                        input(
                            type: "text",
                            name: "git_url",
                            placeholder: (t(lang, "rag-placeholder-source-git-url")),
                            required: "required",
                            class: "input input-bordered input-xs w-80"
                        );
                        input(
                            type: "text",
                            name: "git_ref",
                            placeholder: (t(lang, "rag-placeholder-ref-default")),
                            value: (c.git_ref.clone()),
                            required: "required",
                            class: "input input-bordered input-xs w-44"
                        );
                        button(type: "submit", class: "btn btn-xs") { (t(lang, "rag-button-add-source")) }
                    } else {
                        input(
                            type: "text",
                            name: "git_ref",
                            placeholder: (t(lang, "rag-placeholder-branch-tag-commit")),
                            required: "required",
                            class: "input input-bordered input-xs w-56"
                        );
                        button(type: "submit", class: "btn btn-xs") { (t(lang, "rag-button-add-ref")) }
                    }
                }
                // Bulk add (aggregate only): one repo per line, optional
                // ` @ref`. The fast path for many-repo corpora like Proxmox.
                if aggregate {
                    form(
                        id: (bulk_form_id),
                        action: (bulk_action),
                        method: "post",
                        class: "flex flex-col gap-1 mt-1",
                        "data-on:submit__prevent": (bulk_directive)
                    ) {
                        textarea(
                            name: "sources",
                            rows: "4",
                            placeholder: (t(lang, "rag-placeholder-bulk-sources")),
                            class: "textarea textarea-bordered textarea-xs w-full font-mono"
                        ) {}
                        div {
                            button(type: "submit", class: "btn btn-xs") { (t(lang, "rag-button-add-bulk")) }
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

/// The ref whose *lifecycle* a row displays. Aggregate collections keep ONE
/// unified index (built by the primary ref, folding in every source), so a
/// re-index of any source rebuilds the whole corpus on the primary — every
/// source row therefore mirrors the primary's status/provenance/error and they
/// all transition together. Versioned refs each carry their own build, so a row
/// reflects itself. Falls back to `r` if no primary is present (shouldn't
/// happen, but keeps rendering total).
fn status_ref<'a>(
    c: &rag_db::Collection,
    r: &'a rag_db::CollectionRef,
    primary: Option<&'a rag_db::CollectionRef>,
) -> &'a rag_db::CollectionRef {
    if c.search_mode == rag_db::SearchMode::Aggregate {
        primary.unwrap_or(r)
    } else {
        r
    }
}

/// One ref/source row inside a collection: its name, primary badge, status,
/// last-indexed provenance, and per-ref actions (re-index / set-primary /
/// delete). For aggregate collections the source repo (e.g. `qemu-server`)
/// is shown as the label, since every source there shares the same `git_ref`.
///
/// `primary` is the collection's primary ref (if any). In aggregate mode the
/// status/provenance/error shown come from it (the shared unified index), while
/// the label, id, and per-row actions still come from `r` — so clicking
/// Re-index on any source row shows *that* row (and all rows) rebuilding, which
/// is what actually happens. See [`status_ref`].
fn render_ref(
    lang: Lang,
    c: &rag_db::Collection,
    r: &rag_db::CollectionRef,
    primary: Option<&rag_db::CollectionRef>,
) -> Html {
    let dom_id = format!("rag-ref-{}", r.id);
    let reindex_action = format!("/rag/refs/{}/reindex", r.id);
    let delete_action = format!("/rag/refs/{}/delete", r.id);
    let primary_action = format!("/rag/refs/{}/primary", r.id);
    let log_action = format!("/rag/refs/{}/log", r.id);
    let edit_action = format!("/rag/refs/{}/edit-form", r.id);
    let reindex_directive = format!("@post('{reindex_action}', {{contentType: 'form'}})");
    let delete_directive = format!("@post('{delete_action}', {{contentType: 'form'}})");
    let primary_directive = format!("@post('{primary_action}', {{contentType: 'form'}})");
    let edit_directive = format!("@post('{edit_action}', {{contentType: 'form'}})");
    // Toggle: if the log container already holds content, clear it (close);
    // otherwise fetch + fill it (open). The container always exists (rendered
    // as a sibling in `render_row`), so the bare `.innerHTML` assignment is
    // safe — no optional chaining needed (and `a?.b = c` is invalid JS anyway).
    let reflog_id = format!("rag-reflog-{}", r.id);
    let log_directive = format!(
        "document.getElementById('{reflog_id}').innerHTML ? \
         document.getElementById('{reflog_id}').innerHTML = '' : @get('{log_action}')"
    );
    let aggregate = c.search_mode == rag_db::SearchMode::Aggregate;
    // Status, provenance, and error come from the ref that actually holds this
    // row's build: itself for versioned collections, the shared primary for
    // aggregate ones (one unified index → all sources rebuild together).
    let s = status_ref(c, r, primary);
    let last_indexed = s
        .last_indexed_at
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| t(lang, "rag-never"));
    let last_commit = s
        .last_indexed_commit
        .as_deref()
        .unwrap_or("—")
        .chars()
        .take(8)
        .collect::<String>();
    // Aggregate: lead with the source repo and show the ref after it.
    // Versioned: the ref is the label (one repo, many refs).
    let label = if aggregate {
        format!("{} @ {}", r.source_label(c), r.git_ref)
    } else {
        r.git_ref.clone()
    };
    let indexed_line = t_args(
        lang,
        "rag-ref-indexed-line",
        &i18n::args([
            ("date", last_indexed.into()),
            ("commit", last_commit.into()),
        ]),
    );
    html! {
        div(id: (dom_id), class: "flex items-center gap-2 text-sm flex-wrap") {
            span(class: "font-mono") { (label) }
            // Primacy is meaningful only for versioned collections (the
            // search default); aggregate search ignores it.
            if r.is_primary && !aggregate {
                span(class: "badge badge-sm") { (t(lang, "rag-badge-primary")) }
            }
            (status_badge(lang, s.status))
            span(class: "text-xs text-base-content/60") {
                (indexed_line)
            }
            if let Some(err) = s.last_error.as_ref() {
                // Headline the most recent error/advisory; the Log button
                // opens the full timeline below.
                span(
                    class: (if s.status == rag_db::CollectionStatus::Error {
                        "text-xs text-error break-all"
                    } else {
                        "text-xs text-warning break-all"
                    })
                ) { (err.clone()) }
            }
            div(class: "flex items-center gap-1 ml-auto") {
                button(
                    type: "button",
                    class: "btn btn-xs btn-ghost",
                    "data-on:click": (log_directive)
                ) { (t(lang, "rag-button-log")) }
                form(action: (edit_action), method: "post", class: "m-0", "data-on:submit__prevent": (edit_directive)) {
                    button(type: "submit", class: "btn btn-xs btn-ghost") { (t(lang, "rag-button-edit")) }
                }
                form(action: (reindex_action), method: "post", class: "m-0", "data-on:submit__prevent": (reindex_directive)) {
                    button(type: "submit", class: "btn btn-xs") { (t(lang, "rag-button-reindex")) }
                }
                if !r.is_primary && !aggregate {
                    form(action: (primary_action), method: "post", class: "m-0", "data-on:submit__prevent": (primary_directive)) {
                        button(type: "submit", class: "btn btn-xs btn-ghost") { (t(lang, "rag-button-set-primary")) }
                    }
                }
                form(action: (delete_action), method: "post", class: "m-0", "data-on:submit__prevent": (delete_directive)) {
                    button(type: "submit", class: "btn btn-xs btn-ghost btn-error") { (t(lang, "rag-button-remove")) }
                }
            }
        }
    }
    .to_html()
}

/// A small coloured badge for a log entry's severity. `shrink-0` +
/// `whitespace-nowrap` keep it on one line — without them the row's
/// `break-all` on the message would wrap the short label letter-by-letter
/// ("e/r/r/o/r" stacked).
fn log_level_badge(lang: Lang, level: rag_db::LogLevel) -> Html {
    let (cls, key) = match level {
        rag_db::LogLevel::Info => ("badge badge-xs badge-ghost shrink-0", "rag-log-info"),
        rag_db::LogLevel::Warn => ("badge badge-xs badge-warning shrink-0", "rag-log-warn"),
        rag_db::LogLevel::Error => ("badge badge-xs badge-error shrink-0", "rag-log-error"),
    };
    let label = t(lang, key);
    html! { span(class: (cls)) { (label) } }.to_html()
}

/// Render a ref's indexing timeline into its `#rag-reflog-{ref_id}` container.
/// Newest first; each row shows time, severity, phase, and message. Closing is
/// the "Log" button's job (it toggles the container), so there's no Hide here.
fn render_ref_log(lang: Lang, _ref_id: i64, entries: &[rag_db::IndexLogEntry]) -> Html {
    html! {
        div(class: "mt-1 mb-1 rounded border border-base-300 bg-base-200/40 p-2 text-xs") {
            div(class: "mb-1") {
                span(class: "font-medium text-base-content/70") { (t(lang, "rag-log-heading")) }
            }
            if entries.is_empty() {
                p(class: "text-base-content/60") {
                    (t(lang, "rag-log-empty"))
                }
            } else {
                ul(class: "flex flex-col gap-1") {
                    for e in entries.iter() {
                        // Only the message wraps (break-words); the fixed-width
                        // columns stay on one line via shrink-0.
                        li(class: "flex items-start gap-2 font-mono") {
                            span(class: "text-base-content/50 shrink-0 whitespace-nowrap") {
                                (e.created_at.strftime("%Y-%m-%d %H:%M:%S").to_string())
                            }
                            (log_level_badge(lang, e.level))
                            span(class: "text-base-content/50 shrink-0 w-14") { (e.phase.clone()) }
                            span(class: "min-w-0 break-words") { (e.message.clone()) }
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

/// Fetch a collection + its refs and render its row. Used by the ref/edit
/// handlers to re-patch a single `#rag-row-{id}`.
async fn row_html(state: &RamaState, lang: Lang, collection_id: i64) -> Option<String> {
    let c = rag_db::find_collection_by_id(&state.db, collection_id)
        .await
        .ok()
        .flatten()?;
    let refs = rag_db::list_refs(&state.db, collection_id)
        .await
        .unwrap_or_default();
    Some(render_row(lang, &c, &refs).to_string())
}

fn render_create_form(
    lang: Lang,
    embedding_models: &[String],
    default_embedding: Option<&str>,
    registry: &ProviderRegistry,
    profiles: &[docs_db::Profile],
) -> Html {
    let profile_picker = profile_field(lang, profiles, None);
    let picker = rag_source::source_picker(lang, registry, rag_source::GIT_KIND);
    let fields = rag_source::provider_fields(lang, registry, None, None);
    let signals = rag_source::source_signals(rag_source::GIT_KIND);
    let git_show = format!("$sourceKind === '{}'", rag_source::GIT_KIND);
    let remote_show = format!("$sourceKind !== '{}'", rag_source::GIT_KIND);
    html! {
        form(
            id: "rag-create-form",
            action: "/rag",
            method: "post",
            class: "card border border-base-300 mb-6",
            "data-on:submit__prevent": "@post('/rag', {contentType: 'form'})"
        ) {
            div(class: "card-body") {
                h2(class: "card-title") { (t(lang, "rag-create-heading")) }
                p(class: "text-base-content/70 text-sm") {
                    (t(lang, "rag-create-description"))
                }
                div(class: "grid grid-cols-1 md:grid-cols-2 gap-4 mt-2") {
                    label(class: "flex flex-col gap-1 w-full") {
                        div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-name")) } }
                        input(
                            name: "name",
                            type: "text",
                            required: "required",
                            placeholder: (t(lang, "rag-placeholder-name")),
                            class: "input input-bordered w-full"
                        );
                    }
                    (embedding_model_field(lang, embedding_models, default_embedding))
                    label(class: "flex flex-col gap-1 w-full md:col-span-2") {
                        div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-description-optional")) } }
                        input(
                            name: "description",
                            type: "text",
                            placeholder: (t(lang, "rag-placeholder-description")),
                            class: "input input-bordered w-full"
                        );
                    }
                    (signals)
                    (picker)
                    div(
                        class: "grid grid-cols-1 md:grid-cols-2 gap-4 md:col-span-2",
                        "data-show": (git_show.clone()),
                        style: "display:none"
                    ) {
                        label(class: "flex flex-col gap-1 w-full") {
                            // Not `required`: aggregate collections leave this empty
                            // (each source brings its own URL). The server enforces
                            // a non-empty URL for versioned git collections.
                            div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-git-url-versioned")) } }
                            input(
                                name: "git_url",
                                type: "text",
                                placeholder: (t(lang, "rag-placeholder-git-url")),
                                class: "input input-bordered w-full"
                            );
                        }
                        label(class: "flex flex-col gap-1 w-full") {
                            div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-branch-tag")) } }
                            input(
                                name: "git_ref",
                                type: "text",
                                value: "main",
                                class: "input input-bordered w-full"
                            );
                        }
                        label(class: "flex flex-col gap-1 w-full md:col-span-2") {
                            div(class: "label") {
                                span(class: "label-text") { (t(lang, "rag-label-pat-optional")) }
                            }
                            input(
                                name: "pat",
                                type: "password",
                                placeholder: (t(lang, "rag-placeholder-pat")),
                                class: "input input-bordered w-full"
                            );
                        }
                    }
                    (fields)
                    (profile_picker)
                    label(class: "flex flex-col gap-1 w-full") {
                        div(class: "label") {
                            span(class: "label-text") { (t(lang, "rag-label-include-globs-full")) }
                        }
                        input(
                            name: "include_globs",
                            type: "text",
                            placeholder: (t(lang, "rag-placeholder-include-globs")),
                            class: "input input-bordered w-full"
                        );
                    }
                    label(class: "flex flex-col gap-1 w-full") {
                        div(class: "label") {
                            span(class: "label-text") { (t(lang, "rag-label-exclude-globs")) }
                        }
                        input(
                            name: "exclude_globs",
                            type: "text",
                            placeholder: (t(lang, "rag-placeholder-exclude-globs")),
                            class: "input input-bordered w-full"
                        );
                    }
                    label(class: "flex flex-col gap-1 w-full") {
                        div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-chunk-size")) } }
                        input(
                            name: "chunk_size",
                            type: "number",
                            value: "800",
                            min: "1",
                            max: "8000",
                            class: "input input-bordered w-full"
                        );
                    }
                    label(class: "flex flex-col gap-1 w-full") {
                        div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-chunk-overlap")) } }
                        input(
                            name: "chunk_overlap",
                            type: "number",
                            value: "100",
                            min: "0",
                            class: "input input-bordered w-full"
                        );
                    }
                    label(class: "flex items-start gap-3 md:col-span-2 cursor-pointer") {
                        input(
                            name: "aggregate",
                            type: "checkbox",
                            class: "checkbox checkbox-sm mt-0.5 shrink-0"
                        );
                        span(class: "label-text min-w-0") {
                            (t(lang, "rag-create-aggregate-help"))
                        }
                    }
                }
                div(class: "card-actions justify-end mt-2") {
                    // Only meaningful for a remote source; hidden for git,
                    // whose repo is checked when indexing actually runs.
                    button(
                        type: "button",
                        class: "btn btn-ghost",
                        "data-show": (remote_show.clone()),
                        style: "display:none",
                        "data-on:click": "@post('/rag/test-source', {contentType: 'form'})"
                    ) {
                        (t(lang, "rag-source-test-button"))
                    }
                    button(type: "submit", class: "btn btn-primary") { (t(lang, "rag-button-queue-indexing")) }
                }
            }
        }
    }
    .to_html()
}

/// The row swapped in by `rag_edit_form`. Same `<li id="rag-row-{id}">`
/// shell so the SSE outer-replace round-trips cleanly between display
/// and edit modes. Fields are pre-filled from the stored row.
fn render_edit_form(
    lang: Lang,
    c: &rag_db::Collection,
    embedding_models: &[String],
    registry: &ProviderRegistry,
    profiles: &[docs_db::Profile],
    // Needed only to answer "has this source been consented?" — the secrets
    // are one sealed blob, so which keys it holds is not visible without it.
    crypto: &Crypto,
) -> Html {
    let profile_picker = profile_field(lang, profiles, c.profile_id);
    let picker = rag_source::source_picker(lang, registry, &c.source.kind);
    let source_fields = rag_source::provider_fields(
        lang,
        registry,
        Some(&c.source),
        Some(rag_source::ConsentState {
            collection_id: c.id,
            connected: rag_source::has_refresh_token(&c.source, crypto),
            account: c.connected_account.as_deref(),
        }),
    );
    let source_signals = rag_source::source_signals(&c.source.kind);
    let git_show = format!("$sourceKind === '{}'", rag_source::GIT_KIND);
    let remote_show = format!("$sourceKind !== '{}'", rag_source::GIT_KIND);
    let collection_id = c.id.to_string();
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
    let pat_placeholder = if pat_present {
        t(lang, "rag-placeholder-pat-keep")
    } else {
        t(lang, "rag-placeholder-pat")
    };
    let editing_heading = t_args(
        lang,
        "rag-edit-heading",
        &i18n::args([("name", c.name.clone().into())]),
    );
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
                            (editing_heading)
                        }
                        (status_badge(lang, c.status))
                    }
                    div(class: "grid grid-cols-1 md:grid-cols-2 gap-4 mt-2") {
                        label(class: "flex flex-col gap-1 w-full md:col-span-2") {
                            div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-description")) } }
                            input(
                                name: "description",
                                type: "text",
                                value: (description),
                                class: "input input-bordered w-full"
                            );
                        }
                        (source_signals)
                        // Lets "Test connection" fall back to the stored
                        // secret instead of demanding it be retyped.
                        input(type: "hidden", name: "collection_id", value: (collection_id));
                        (picker)
                        div(
                            class: "grid grid-cols-1 md:grid-cols-2 gap-4 md:col-span-2",
                            "data-show": (git_show.clone()),
                            style: "display:none"
                        ) {
                            label(class: "flex flex-col gap-1 w-full") {
                                // Not `required`: aggregate collections leave this
                                // empty (sources bring their own URLs). The server
                                // only enforces it for versioned git collections.
                                div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-git-url-versioned")) } }
                                input(
                                    name: "git_url",
                                    type: "text",
                                    value: (c.git_url.clone()),
                                    class: "input input-bordered w-full"
                                );
                            }
                            label(class: "flex flex-col gap-1 w-full") {
                                div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-branch-tag")) } }
                                input(
                                    name: "git_ref",
                                    type: "text",
                                    value: (c.git_ref.clone()),
                                    class: "input input-bordered w-full"
                                );
                            }
                            div(class: "flex flex-col gap-1 w-full md:col-span-2") {
                                div(class: "label") {
                                    span(class: "label-text") {
                                        (t(lang, "rag-label-pat"))
                                        if pat_present {
                                            span(class: "ml-2 badge badge-success badge-outline") {
                                                (t(lang, "rag-badge-pat-set"))
                                            }
                                        } else {
                                            span(class: "ml-2 badge badge-ghost") { (t(lang, "rag-badge-pat-none")) }
                                        }
                                    }
                                }
                                input(
                                    name: "pat",
                                    type: "password",
                                    placeholder: (pat_placeholder),
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
                                            (t(lang, "rag-label-clear-pat"))
                                        }
                                    }
                                }
                            }
                        }
                        (source_fields)
                        (profile_picker)
                        (embedding_model_field(lang, embedding_models, Some(&c.embedding_model)))
                        label(class: "flex flex-col gap-1 w-full") {
                            div(class: "label") {
                                span(class: "label-text") { (t(lang, "rag-label-include-globs")) }
                            }
                            input(
                                name: "include_globs",
                                type: "text",
                                value: (include_csv),
                                placeholder: (t(lang, "rag-placeholder-include-globs")),
                                class: "input input-bordered w-full"
                            );
                        }
                        label(class: "flex flex-col gap-1 w-full") {
                            div(class: "label") {
                                span(class: "label-text") { (t(lang, "rag-label-exclude-globs")) }
                            }
                            input(
                                name: "exclude_globs",
                                type: "text",
                                value: (exclude_csv),
                                placeholder: (t(lang, "rag-placeholder-exclude-globs")),
                                class: "input input-bordered w-full"
                            );
                        }
                        label(class: "flex flex-col gap-1 w-full") {
                            div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-chunk-size")) } }
                            input(
                                name: "chunk_size",
                                type: "number",
                                value: (chunk_size),
                                min: "1",
                                max: "8000",
                                class: "input input-bordered w-full"
                            );
                        }
                        label(class: "flex flex-col gap-1 w-full") {
                            div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-chunk-overlap")) } }
                            input(
                                name: "chunk_overlap",
                                type: "number",
                                value: (chunk_overlap),
                                min: "0",
                                class: "input input-bordered w-full"
                            );
                        }
                    }
                    label(class: "flex flex-col gap-1 w-full") {
                        div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-allowed-groups")) } }
                        input(
                            name: "allowed_groups",
                            type: "text",
                            value: (c.allowed_groups.join(", ")),
                            placeholder: "developers, network_admin",
                            class: "input input-bordered w-full font-mono"
                        );
                        div(class: "label") { span(class: "label-text-alt text-base-content/60") { (t(lang, "rag-hint-allowed-groups")) } }
                    }
                    div(class: "card-actions justify-end mt-2 gap-2") {
                        form(
                            action: (cancel_action.clone()),
                            method: "post",
                            class: "m-0 inline",
                            "data-on:submit__prevent": (cancel_directive)
                        ) {
                            button(type: "submit", class: "btn btn-sm btn-outline") { (t(lang, "rag-button-cancel")) }
                        }
                        button(
                            type: "button",
                            class: "btn btn-sm btn-ghost",
                            "data-show": (remote_show.clone()),
                            style: "display:none",
                            "data-on:click": "@post('/rag/test-source', {contentType: 'form'})"
                        ) {
                            (t(lang, "rag-source-test-button"))
                        }
                        button(type: "submit", class: "btn btn-sm btn-primary") { (t(lang, "rag-button-save-changes")) }
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
/// Extraction-profile picker.
///
/// "None" is first and is the default: extraction costs one model call per
/// document, and a code collection has no fields worth pulling out. The
/// operator opts in for a document corpus.
fn profile_field(lang: Lang, profiles: &[docs_db::Profile], selected: Option<i64>) -> Html {
    let options: Vec<(String, String, bool)> = profiles
        .iter()
        .map(|p| {
            (
                p.id.to_string(),
                match p.description.as_deref() {
                    Some(d) => format!("{} — {d}", p.name),
                    None => p.name.clone(),
                },
                selected == Some(p.id),
            )
        })
        .collect();
    let none_selected = selected.is_none();
    html! {
        label(class: "flex flex-col gap-1 w-full md:col-span-2") {
            div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-profile")) } }
            select(name: "profile_id", class: "select select-bordered w-full") {
                (super::select_option("0", &t(lang, "rag-option-profile-none"), none_selected))
                for (id, label, is_selected) in options.iter() {
                    (super::select_option(id, label, *is_selected))
                }
            }
            p(class: "text-xs opacity-70") { (t(lang, "rag-profile-help")) }
        }
    }
    .to_html()
}

fn embedding_model_field(lang: Lang, models: &[String], selected: Option<&str>) -> Html {
    if models.is_empty() {
        let value = selected.unwrap_or("");
        return html! {
            label(class: "flex flex-col gap-1 w-full") {
                div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-embedding-model")) } }
                input(
                    name: "embedding_model",
                    type: "text",
                    required: "required",
                    value: (value),
                    placeholder: (t(lang, "rag-placeholder-embedding-model-none")),
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
        label(class: "flex flex-col gap-1 w-full") {
            div(class: "label") { span(class: "label-text") { (t(lang, "rag-label-embedding-model")) } }
            select(
                name: "embedding_model",
                required: "required",
                class: "select select-bordered w-full"
            ) {
                if selected.is_none() && stale_selected.is_none() {
                    option(value: "", disabled: "disabled", selected: "selected") {
                        (t(lang, "rag-option-choose-embedding-model"))
                    }
                }
                if let Some(stale) = stale_selected.as_ref() {
                    option(value: (stale.clone()), selected: "selected") {
                        (stale.clone()) " " (t(lang, "rag-suffix-not-advertised"))
                    }
                }
                for (model, is_selected) in options.iter() {
                    (super::select_option(model, model, *is_selected))
                }
            }
        }
    }
    .to_html()
}

fn render_body(
    lang: Lang,
    list: &[(rag_db::Collection, Vec<rag_db::CollectionRef>)],
    embedding_models: &[String],
    default_embedding: Option<&str>,
    registry: &ProviderRegistry,
    profiles: &[docs_db::Profile],
) -> Html {
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
            div(class: "flex items-center gap-2 mb-2") {
                (icons::folder(20))
                h1(class: "text-2xl font-bold m-0") { (t(lang, "rag-heading")) }
            }
            p(class: "text-base-content/60 text-sm mb-6") {
                (t(lang, "rag-description-prefix")) " "
                code(class: "font-mono text-xs") { "rag_search" }
                " " (t(lang, "rag-description-suffix"))
                " "
                a(
                    href: "/rag/profiles",
                    class: "link link-primary",
                    "data-on:click__prevent": "@get('/rag/profiles')"
                ) {
                    (t(lang, "rag-profile-link"))
                }
            }

            (render_create_form(lang, embedding_models, default_embedding, registry, profiles))

            section(class: "card border border-base-300") {
                div(class: "card-body") {
                    h2(class: "card-title") { (t(lang, "rag-collections-heading")) }
                    // Live status: while on the page, poll `/rag/status` and
                    // morph each ref's status row. Catches transitions driven
                    // by the background indexer (cloning → ready/error) without
                    // a manual reload — and indexing failures the operator
                    // would otherwise never see. Only the `#rag-ref-*` status
                    // rows are re-patched, so the add-source inputs and any
                    // opened log are left untouched. Gated on a non-empty list
                    // so an empty page doesn't poll for nothing.
                    ul(
                        id: "rag-list",
                        class: "flex flex-col divide-y divide-base-300",
                        // `on-interval` is its own datastar plugin (key denied),
                        // so the attribute is hyphen-form `data-on-interval`, NOT
                        // `data-on:interval` (which the generic `on` plugin would
                        // read as an event named "interval" that never fires).
                        "data-on-interval__duration.4s": (if list.is_empty() { "" } else { "@get('/rag/status')" })
                    ) {
                        for (c, refs) in list.iter() {
                            (render_row(lang, c, refs))
                        }
                    }
                    if list.is_empty() {
                        p(class: "text-base-content/60 text-sm") {
                            (t(lang, "rag-empty-list"))
                        }
                    }
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

    fn collection() -> rag_db::Collection {
        let now = Timestamp::now();
        rag_db::Collection {
            id: 7,
            data_uuid: Some("u".into()),
            name: "ceph".into(),
            description: None,
            git_url: "https://example.invalid/ceph.git".into(),
            git_ref: "main".into(),
            pat: None,
            source: Default::default(),
            profile_id: None,
            extraction_model: None,
            sync_hook_set: false,
            connected_account: None,
            connected_by: None,
            connected_at: None,
            embedding_model: "embed".into(),
            include_globs: vec!["**/*".into()],
            exclude_globs: vec![],
            chunk_size: 800,
            chunk_overlap: 100,
            search_mode: rag_db::SearchMode::Versioned,
            status: rag_db::CollectionStatus::Ready,
            allowed_groups: Vec::new(),
            last_indexed_at: None,
            last_indexed_commit: None,
            last_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn cref(id: i64, status: rag_db::CollectionStatus, err: Option<&str>) -> rag_db::CollectionRef {
        let now = Timestamp::now();
        rag_db::CollectionRef {
            id,
            collection_id: 7,
            git_ref: "reef".into(),
            git_url: None,
            is_primary: true,
            data_uuid: "u".into(),
            status,
            last_indexed_at: None,
            last_indexed_commit: None,
            last_error: err.map(|s| s.into()),
            dir_versions: Default::default(),
            delta_cursor: None,
            force_full_rebuild: false,
            extractor_fingerprint: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// The "Log" control on a ref must call the matching log endpoint, and the
    /// status row must carry the stable `#rag-ref-{id}` id the poller patches.
    /// A collection whose source is a registered provider rather than git.
    fn webdav_collection() -> rag_db::Collection {
        let mut c = collection();
        c.source = rag_db::SourceSpec {
            kind: "webdav".into(),
            config: [
                (
                    "base_url".to_string(),
                    "https://cloud.example.com".to_string(),
                ),
                ("username".to_string(), "svc".to_string()),
            ]
            .into_iter()
            .collect(),
            secrets: None,
        };
        c
    }

    #[test]
    fn create_form_offers_every_registered_source_and_its_fields() {
        let registry = ProviderRegistry::with_builtins();
        let html =
            render_create_form(Lang::En, &["embed".into()], None, &registry, &[]).to_string();
        assert!(html.contains(r#"name="source_kind""#), "{html}");
        assert!(
            html.contains(r#"value="webdav""#),
            "the picker lists the provider"
        );
        assert!(
            html.contains(r#"name="src_webdav_base_url""#),
            "the provider's declared fields are rendered: {html}"
        );
    }

    #[test]
    fn create_form_hides_the_git_inputs_behind_the_picker() {
        let registry = ProviderRegistry::with_builtins();
        let html =
            render_create_form(Lang::En, &["embed".into()], None, &registry, &[]).to_string();
        // The git block is gated on the same signal the picker writes, so
        // choosing a remote source does not leave a required-looking Git URL
        // on screen.
        assert!(
            html.contains("$sourceKind ===") || html.contains("$sourceKind &#61;&#61;&#61;"),
            "{html}"
        );
        assert!(html.contains(r#"name="git_url""#));
    }

    #[test]
    fn create_form_wires_the_test_button_to_its_endpoint() {
        let registry = ProviderRegistry::with_builtins();
        let html =
            render_create_form(Lang::En, &["embed".into()], None, &registry, &[]).to_string();
        assert!(
            html.contains("/rag/test-source"),
            "the Test connection button must post to the route the router registers: {html}"
        );
    }

    #[test]
    fn edit_form_preselects_the_collections_own_source() {
        let registry = ProviderRegistry::with_builtins();
        let c = webdav_collection();
        let html = render_edit_form(
            Lang::En,
            &c,
            &["embed".into()],
            &registry,
            &[],
            &Crypto::from_key([3u8; 32]),
        )
        .to_string();
        assert!(
            html.contains(r#"value="webdav" selected="selected""#),
            "the stored kind is the selected option: {html}"
        );
        assert!(
            html.contains("sourceKind:") && html.contains("webdav"),
            "and the signal is seeded to match, so the right field set shows on load"
        );
        assert!(
            html.contains("https://cloud.example.com"),
            "stored non-secret settings are prefilled"
        );
    }

    #[test]
    fn edit_form_carries_the_collection_id_for_a_credential_free_test() {
        let registry = ProviderRegistry::with_builtins();
        let c = webdav_collection();
        let html = render_edit_form(
            Lang::En,
            &c,
            &["embed".into()],
            &registry,
            &[],
            &Crypto::from_key([3u8; 32]),
        )
        .to_string();
        assert!(
            html.contains(r#"name="collection_id""#),
            "without this, testing an existing source would demand the password be retyped"
        );
    }

    #[test]
    fn a_remote_source_does_not_require_a_git_url() {
        let form = CreateForm {
            name: "docs".into(),
            description: None,
            git_url: String::new(),
            git_ref: None,
            pat: None,
            profile_id: None,
            extraction_model: None,
            embedding_model: "embed".into(),
            include_globs: None,
            exclude_globs: None,
            chunk_size: None,
            chunk_overlap: None,
            aggregate: None,
        };
        let source = rag_db::SourceSpec {
            kind: "webdav".into(),
            config: Default::default(),
            secrets: None,
        };
        let new = validate(Lang::En, form, source).expect("a remote source has no repo URL");
        assert_eq!(new.source.kind, "webdav");
    }

    #[test]
    fn a_git_collection_still_requires_its_url() {
        let form = CreateForm {
            name: "code".into(),
            description: None,
            git_url: String::new(),
            git_ref: None,
            pat: None,
            profile_id: None,
            extraction_model: None,
            embedding_model: "embed".into(),
            include_globs: None,
            exclude_globs: None,
            chunk_size: None,
            chunk_overlap: None,
            aggregate: None,
        };
        assert!(
            validate(Lang::En, form, rag_db::SourceSpec::default()).is_err(),
            "the git path keeps its existing validation"
        );
    }

    #[test]
    fn render_ref_wires_log_button_to_endpoint() {
        let c = collection();
        let r = cref(
            42,
            rag_db::CollectionStatus::Error,
            Some("Branch 'x' does not exist"),
        );
        let html = render_ref(Lang::En, &c, &r, Some(&r)).to_string();
        // (plait HTML-escapes attribute values, so match escaping-safe
        // substrings — the stable id and the endpoint path.)
        assert!(html.contains("rag-ref-42"), "{html}");
        assert!(html.contains("/rag/refs/42/log"), "{html}");
        // The latest error is headlined on the row itself.
        assert!(html.contains("does not exist"), "{html}");
    }

    /// Regression: an aggregate collection keeps ONE unified index (built by the
    /// primary ref, folding in every source), so re-indexing any source rebuilds
    /// the whole corpus on the primary. Every source row must therefore mirror
    /// the primary's status — otherwise a non-primary row stays "ready" while the
    /// primary alone flips to "pending", which reads as "I clicked Re-index on
    /// cifs-utils but samba started indexing". The clicked row must still target
    /// its OWN ref id (the re-index URL is per-row).
    #[test]
    fn aggregate_source_row_mirrors_primary_status() {
        let mut c = collection();
        c.search_mode = rag_db::SearchMode::Aggregate;
        // Primary source (e.g. `samba`) is mid-rebuild; a non-primary source
        // (e.g. `cifs-utils`) has no build of its own.
        let mut primary = cref(1, rag_db::CollectionStatus::Cloning, None);
        primary.is_primary = true;
        let mut other = cref(2, rag_db::CollectionStatus::Ready, None);
        other.is_primary = false;

        let html = render_ref(Lang::En, &c, &other, Some(&primary)).to_string();
        // The non-primary row shows the primary's live status, not its own.
        assert!(
            html.contains("cloning"),
            "non-primary row should mirror primary status: {html}"
        );
        assert!(
            !html.contains(">ready<"),
            "non-primary row must not show its own stale 'ready': {html}"
        );
        // …but it keeps its own identity + actions (re-index targets ref 2).
        assert!(html.contains("rag-ref-2"), "{html}");
        assert!(html.contains("/rag/refs/2/reindex"), "{html}");
    }

    /// Regression for "individual repos show `indexed never · —`": only the
    /// primary ref carries the unified index's provenance (last-indexed time +
    /// commit); non-primary sources are never built and have none. An aggregate
    /// source row must therefore mirror the *primary's* provenance too — not
    /// just its status — or every source but the first reads "indexed never".
    #[test]
    fn aggregate_source_row_mirrors_primary_provenance() {
        let mut c = collection();
        c.search_mode = rag_db::SearchMode::Aggregate;
        let mut primary = cref(1, rag_db::CollectionStatus::Ready, None);
        primary.is_primary = true;
        primary.last_indexed_at = Some(
            "2026-07-06T11:08:00Z"
                .parse::<Timestamp>()
                .expect("valid ts"),
        );
        primary.last_indexed_commit = Some("8c585019abcdef".into());
        // Non-primary source: never built, so its own provenance is empty.
        let mut other = cref(2, rag_db::CollectionStatus::Ready, None);
        other.is_primary = false;

        let html = render_ref(Lang::En, &c, &other, Some(&primary)).to_string();
        assert!(
            html.contains("2026-07-06"),
            "non-primary row must show the primary's indexed date, not 'never': {html}"
        );
        assert!(
            html.contains("8c585019"),
            "non-primary row must show the primary's commit: {html}"
        );
        assert!(
            !html.contains("never"),
            "non-primary aggregate row must not read 'indexed never': {html}"
        );
    }

    /// Versioned collections build each ref independently, so a row must reflect
    /// its OWN status — never a sibling's. Guards against the aggregate mirroring
    /// leaking into versioned mode.
    #[test]
    fn versioned_row_shows_own_status_not_primary() {
        let c = collection(); // Versioned
        let primary = cref(1, rag_db::CollectionStatus::Cloning, None);
        let mut other = cref(2, rag_db::CollectionStatus::Ready, None);
        other.is_primary = false;
        let html = render_ref(Lang::En, &c, &other, Some(&primary)).to_string();
        assert!(
            html.contains("ready"),
            "versioned row shows its own status: {html}"
        );
        assert!(
            !html.contains("cloning"),
            "versioned row must not borrow primary status: {html}"
        );
    }

    /// Every source row must expose an Edit control wired to its own
    /// `edit-form` endpoint — that's how aggregate per-source repo URLs get
    /// changed (the collection-level Edit only carries the single versioned
    /// URL). Regression for "I can't edit the git URLs".
    #[test]
    fn render_ref_wires_edit_button_to_endpoint() {
        let mut c = collection();
        c.search_mode = rag_db::SearchMode::Aggregate;
        let mut r = cref(7, rag_db::CollectionStatus::Ready, None);
        r.is_primary = false;
        let html = render_ref(Lang::En, &c, &r, Some(&r)).to_string();
        assert!(html.contains("Edit"), "{html}");
        assert!(html.contains("/rag/refs/7/edit-form"), "{html}");
    }

    /// The inline source editor pre-fills the source's URL + ref and submits to
    /// its `update` endpoint, with a Cancel wired to `cancel-edit`.
    #[test]
    fn render_ref_edit_form_prefills_and_wires_update() {
        let mut c = collection();
        c.search_mode = rag_db::SearchMode::Aggregate;
        let mut r = cref(7, rag_db::CollectionStatus::Ready, None);
        r.git_url = Some("https://example.com/org/cifs-utils.git".into());
        r.git_ref = "master".into();
        let html = render_ref_edit_form(Lang::En, &c, &r).to_string();
        assert!(html.contains("/rag/refs/7/update"), "{html}");
        assert!(html.contains("/rag/refs/7/cancel-edit"), "{html}");
        // The current URL + ref are pre-filled so the operator edits, not retypes.
        assert!(html.contains("cifs-utils.git"), "{html}");
        assert!(html.contains("master"), "{html}");
    }

    /// Initial page render (not just an event) must arm the status poll and
    /// emit a log container per ref — otherwise the admin never sees live
    /// transitions or can't open the log. Guards the on-load wiring.
    #[test]
    fn render_body_arms_status_poll_and_log_container() {
        let c = collection();
        let refs = vec![cref(42, rag_db::CollectionStatus::Cloning, None)];
        let registry = ProviderRegistry::with_builtins();
        let html = render_body(
            Lang::En,
            &[(c, refs)],
            &["embed".into()],
            None,
            &registry,
            &[],
        )
        .to_string();
        assert!(html.contains("data-on-interval__duration.4s"), "{html}");
        assert!(html.contains("/rag/status"), "{html}");
        assert!(html.contains("rag-reflog-42"), "{html}");
    }

    /// An empty list must NOT arm the poll (nothing to watch → no traffic).
    #[test]
    fn render_body_empty_list_does_not_poll() {
        let registry = ProviderRegistry::with_builtins();
        let html = render_body(Lang::En, &[], &["embed".into()], None, &registry, &[]).to_string();
        assert!(!html.contains("/rag/status"), "{html}");
    }

    /// The configured default embedding model pre-selects the create form's
    /// `<select>` (so the operator doesn't re-pick it every time), while an
    /// unset default leaves the "choose a model" placeholder in place.
    #[test]
    fn create_form_preselects_configured_default_embedding() {
        let models: Vec<String> = vec!["embed-a".into(), "embed-b".into()];
        // Configured + advertised → that option is selected.
        let registry = ProviderRegistry::with_builtins();
        let with =
            render_create_form(Lang::En, &models, Some("embed-b"), &registry, &[]).to_string();
        assert!(
            with.contains(r#"value="embed-b" selected="selected""#),
            "configured default must be pre-selected: {with}"
        );
        // No default → the disabled placeholder is the selected option.
        let without = render_create_form(Lang::En, &models, None, &registry, &[]).to_string();
        assert!(
            without.contains(r#"disabled="disabled" selected="selected""#),
            "unset default must keep the choose-a-model placeholder: {without}"
        );
    }

    #[test]
    fn render_ref_log_shows_entries_and_empty_state() {
        let now = Timestamp::now();
        let entries = vec![rag_db::IndexLogEntry {
            id: 1,
            ref_id: 42,
            collection_id: 7,
            created_at: now,
            level: rag_db::LogLevel::Error,
            phase: "cloning".into(),
            message: "Branch 'x' does not exist in the repository".into(),
            commit_sha: None,
            files: None,
            chunks: None,
            duration_ms: Some(12),
        }];
        let html = render_ref_log(Lang::En, 42, &entries).to_string();
        assert!(html.contains("does not exist in the repository"), "{html}");
        assert!(html.contains("error"), "{html}");

        let empty = render_ref_log(Lang::En, 42, &[]).to_string();
        assert!(empty.contains("No indexing events recorded yet"), "{empty}");
    }
}
