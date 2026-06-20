// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Top-level rama Router. Mirrors the axum router shape in
//! `crate::server::api::router` — same paths, same methods — but rebuilt
//! against rama's `web::Router` and handler model.
//!
//! Three groups of routes wired below:
//!   - **Static + pages**: `/assets/*`, `/`, `/login`, `/tokens`,
//!     `/chat`, `/theme/toggle` — server-rendered HTML via plait +
//!     daisyUI, with SSE patches (`datastar-patch-elements`) for
//!     in-page nav and CRUD.
//!   - **OpenAI-compatible proxy**: `/v1/models`,
//!     `/v1/chat/completions`, `/v1/audio/transcriptions`,
//!     `/v1/embeddings` — token-
//!     authenticated, forwarded to the upstream pool selected by
//!     model.
//!   - **Auth + session API**: `/auth/*` (OIDC + CLI device flow) and
//!     `/api/v0/*` (session-scoped token CRUD + transcription used by
//!     the chat composer).

use std::sync::Arc;

use rama::http::layer::error_handling::ErrorHandlerLayer;
use rama::http::server::HttpServer;
use rama::http::service::web::Router;
use rama::http::service::web::response::Json;
use rama::layer::{ArcLayer, Layer};
use rama::net::address::SocketAddress;
use serde_json::json;

use crate::rama_server::RamaState;
use crate::rama_server::{api, cli_handlers, oidc_handlers, pages, proxy, rag_api};
use session_core::assets;

/// Builds the rama router. State is shared via `Arc` since handlers
/// borrow it immutably.
pub fn router(state: Arc<RamaState>) -> Router<Arc<RamaState>> {
    Router::new_with_state(state)
        .with_get("/healthz", async || Json(json!({"status": "ok"})))
        .with_get("/readyz", async || Json(json!({"status": "ok"})))
        // Static asset bundles, baked in via include_bytes.
        .with_get("/assets/app.css", assets::app_css)
        .with_get("/assets/datastar.js", assets::datastar_js)
        .with_get("/assets/app.js", assets::app_js)
        .with_get("/assets/pcm-recorder.js", assets::pcm_recorder_js)
        // Page handlers — server-rendered HTML, plait + daisyUI.
        // `/` is the chat surface: a plain navigation 303s into the
        // latest (or a fresh) `/chat/{id}`; a Datastar nav renders chat
        // in place. There is no separate dashboard landing page — the
        // old identity card moved into /tokens.
        .with_get("/", pages::chat_index)
        .with_get("/login", pages::login)
        .with_get("/tokens", pages::tokens_index)
        .with_post("/tokens", pages::tokens_create)
        .with_post("/tokens/{id}/revoke", pages::tokens_revoke)
        .with_post("/tokens/{id}/delete", pages::tokens_delete)
        .with_post("/tokens/{id}/tools/master", pages::tokens_tools_master)
        .with_post("/tokens/{id}/tools/toggle", pages::tokens_tools_toggle)
        .with_get("/tools", pages::tools_index)
        .with_post("/tools/toggle", pages::tools_toggle)
        .with_get("/memory", pages::memory_index)
        .with_post("/memory", pages::memory_create)
        .with_post("/memory/{id}/edit", pages::memory_edit)
        .with_post("/memory/{id}/delete", pages::memory_delete)
        .with_get("/scheduled", pages::scheduled_index)
        .with_post("/scheduled", pages::scheduled_create)
        .with_post("/scheduled/preview", pages::scheduled_preview)
        .with_get("/scheduled/{id}/edit", pages::scheduled_edit_form)
        .with_post("/scheduled/{id}", pages::scheduled_update)
        .with_post("/scheduled/{id}/toggle", pages::scheduled_toggle)
        .with_post("/scheduled/{id}/delete", pages::scheduled_delete)
        .with_get("/chat", pages::chat_index)
        .with_get("/chat/{id}", pages::chat_session_view)
        .with_post("/chat/sessions", pages::chat_session_create)
        .with_post("/chat/{id}/messages", pages::chat_message_send)
        .with_get("/chat/{id}/tail", pages::chat_tail)
        .with_post("/chat/{id}/cancel", pages::chat_cancel)
        .with_post("/chat/{id}/turns/{turn_id}/retry", pages::chat_retry)
        .with_post("/chat/{id}/turns/{turn_id}/edit", pages::chat_edit)
        .with_post("/chat/{id}/delete", pages::chat_session_delete)
        .with_post("/chat/{id}/share", pages::chat_share_toggle)
        .with_post("/chat/{id}/fork", pages::chat_fork)
        .with_get("/chat/{id}/export.md", pages::chat_export_markdown)
        .with_get("/chat/{id}/export.pdf", pages::chat_export_pdf)
        .with_get(
            "/chat/attachment/{turn_id}/{filename}",
            pages::chat_attachment,
        )
        .with_get("/admin/models", pages::admin_models_index)
        .with_post("/admin/models", pages::admin_models_save)
        .with_get("/admin/backends", pages::admin_backends_index)
        .with_get("/admin/users", pages::admin_users_index)
        // Target id rides in the POST body (not the path) — rama lowercases
        // path segments, which would mangle case-sensitive OIDC subjects.
        .with_post("/admin/users/impersonate", pages::users_impersonate)
        .with_post("/impersonate/stop", pages::impersonate_stop)
        .with_get("/admin/skills", pages::admin_skills_index)
        .with_get("/admin/skills/download", pages::admin_skills_download)
        .with_post("/admin/skills/upload", pages::admin_skills_upload)
        .with_post("/admin/skills/delete", pages::admin_skills_delete)
        .with_get("/rag", pages::rag_index)
        .with_post("/rag", pages::rag_create)
        .with_post("/rag/{id}/reindex", pages::rag_reindex)
        .with_post("/rag/{id}/delete", pages::rag_delete)
        .with_post("/rag/{id}/edit-form", pages::rag_edit_form)
        .with_post("/rag/{id}/cancel-edit", pages::rag_cancel_edit)
        .with_post("/rag/{id}/update", pages::rag_update)
        .with_post("/rag/{id}/refs", pages::rag_add_ref)
        .with_post("/rag/{id}/refs/bulk", pages::rag_add_sources_bulk)
        .with_post("/rag/refs/{ref_id}/reindex", pages::rag_ref_reindex)
        .with_post("/rag/refs/{ref_id}/primary", pages::rag_ref_set_primary)
        .with_post("/rag/refs/{ref_id}/delete", pages::rag_ref_delete)
        .with_post("/theme/toggle", session_core::chrome::theme_toggle)
        .with_get("/v1/models", proxy::list_models)
        // Catch-all param: model ids contain `/` (e.g.
        // `mistralai/Voxtral-Mini-4B-Realtime-2602`).
        .with_get("/v1/models/{*id}", proxy::retrieve_model)
        .with_post("/v1/chat/completions", proxy::chat_completions)
        .with_post("/v1/audio/transcriptions", proxy::transcribe)
        .with_post("/v1/embeddings", proxy::embeddings)
        .with_get("/auth/login", oidc_handlers::login)
        .with_get("/auth/callback", oidc_handlers::callback)
        .with_post("/auth/logout", oidc_handlers::logout)
        .with_post("/auth/cli/start", cli_handlers::start)
        .with_get("/auth/cli/begin", cli_handlers::begin)
        .with_post("/auth/cli/poll", cli_handlers::poll)
        .with_get("/api/v0/me", api::me)
        .with_get("/api/v0/tokens", api::list_tokens)
        .with_post("/api/v0/tokens", api::create_token)
        .with_post("/api/v0/tokens/{id}/revoke", api::revoke_token)
        .with_put("/api/v0/tokens/{id}/tools", api::update_token_tools)
        .with_delete("/api/v0/tokens/{id}", api::delete_token)
        .with_post("/api/v0/transcriptions", proxy::transcribe_session)
        .with_get("/api/v0/transcription_models", api::transcription_models)
        .with_post("/api/v0/me/timezone", api::set_timezone)
        .with_post("/api/v0/me/location", api::set_location)
        .with_delete("/api/v0/me/location", api::clear_location)
        .with_post(
            "/api/v0/me/location/feedback/{turn_id}",
            api::location_feedback,
        )
        .with_get("/api/v0/rag/collections", rag_api::list_collections)
        .with_post("/api/v0/rag/collections", rag_api::create_collection)
        .with_get("/api/v0/rag/collections/{id}", rag_api::get_collection)
        .with_patch("/api/v0/rag/collections/{id}", rag_api::update_collection)
        .with_delete("/api/v0/rag/collections/{id}", rag_api::delete_collection)
        .with_post(
            "/api/v0/rag/collections/{id}/reindex",
            rag_api::reindex_collection,
        )
}

/// The complete HTTP service: the router plus the layers that make it
/// servable. rc1's `Router` is not `Clone` and surfaces `RouterError`,
/// while `HttpServer::listen` wants a `Clone` service whose error is
/// `Infallible` — `ArcLayer` makes the router shareable/cloneable and
/// `ErrorHandlerLayer` renders any `RouterError` (e.g. an unmatched path)
/// into a `Response`. Both `serve` and the tests build the service through
/// here so they exercise the same stack (notably the 404 handling).
pub fn service(
    state: Arc<RamaState>,
) -> impl rama::Service<
    rama::http::Request,
    Output = rama::http::Response,
    Error = std::convert::Infallible,
> + Clone {
    let router = router(state);
    (ArcLayer::new(), ErrorHandlerLayer::default()).into_layer(router)
}

/// Convenience: build the service and start serving on `addr`.
pub async fn serve(state: Arc<RamaState>, addr: SocketAddress) -> anyhow::Result<()> {
    HttpServer::default()
        .listen(addr, service(state))
        .await
        .map_err(|e| anyhow::anyhow!("rama listen: {e}"))?;
    Ok(())
}
