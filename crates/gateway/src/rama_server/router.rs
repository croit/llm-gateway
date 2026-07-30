// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Top-level rama Router. Mirrors the axum router shape in
//! `gateway_core::server::api::router` — same paths, same methods — but rebuilt
//! against rama's `web::Router` and handler model.
//!
//! Three groups of routes wired below:
//!   - **Static + pages**: `/assets/*`, `/`, `/login`, `/tokens`,
//!     `/chat`, `/theme/toggle` — server-rendered HTML via plait +
//!     daisyUI, with SSE patches (`datastar-patch-elements`) for
//!     in-page nav and CRUD.
//!   - **OpenAI-compatible proxy**: `/v1/models`,
//!     `/v1/chat/completions`, `/v1/audio/transcriptions`,
//!     `/v1/embeddings`, `/v1/images/generations` — token-
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
use crate::rama_server::{api, comfyui_api, oidc_handlers, pages, proxy, rag_api, sandbox_api};
use gateway_core::rama_server::cors::V1CorsLayer;
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
        // PWA installability: manifest, service worker, favicon, icons.
        // All public (no session check) — the SW needs root-scope
        // access and the manifest/icons are referenced from `<head>`
        // before any auth redirect.
        .with_get("/manifest.webmanifest", assets::manifest_webmanifest)
        .with_get("/sw.js", assets::sw_js)
        .with_get("/favicon.ico", assets::favicon)
        .with_get("/icons/{*name}", assets::icon)
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
        .with_post("/tokens/{id}/rotate", pages::tokens_rotate)
        .with_post("/tokens/{id}/delete", pages::tokens_delete)
        .with_post("/tokens/{id}/tools/master", pages::tokens_tools_master)
        .with_post("/tokens/{id}/tools/toggle", pages::tokens_tools_toggle)
        .with_post("/tokens/{id}/mcp-policy", pages::tokens_mcp_policy)
        .with_get("/tools", pages::tools_index)
        .with_post("/tools/toggle", pages::tools_toggle)
        .with_get("/memory", pages::memory_index)
        .with_post("/memory", pages::memory_create)
        .with_post("/memory/{id}/edit", pages::memory_edit)
        .with_post("/memory/{id}/delete", pages::memory_delete)
        .with_get("/usage", pages::usage_index)
        // Feedback widget (JSON endpoints; the FAB + dialog are chrome).
        .with_get("/feedback/config", pages::feedback_config)
        .with_post("/feedback/extract", pages::feedback_extract)
        .with_post("/feedback", pages::feedback_submit)
        .with_get("/scheduled", pages::scheduled_index)
        .with_post("/scheduled", pages::scheduled_create)
        .with_post("/scheduled/preview", pages::scheduled_preview)
        .with_get("/scheduled/{id}/edit", pages::scheduled_edit_form)
        .with_post("/scheduled/{id}", pages::scheduled_update)
        .with_post("/scheduled/{id}/toggle", pages::scheduled_toggle)
        .with_post("/scheduled/{id}/delete", pages::scheduled_delete)
        // Webhooks: per-user prompts fired by an inbound HTTP call. The
        // management pages are session-gated; the public trigger
        // `/hooks/{secret}` authenticates by the secret in the URL.
        .with_get("/webhooks", pages::webhooks_index)
        .with_post("/webhooks", pages::webhooks_create)
        .with_get("/webhooks/{id}/edit", pages::webhooks_edit_form)
        .with_get("/webhooks/{id}/runs", pages::webhooks_runs)
        .with_get("/webhooks/{id}/rerun", pages::webhooks_rerun_form)
        .with_post("/webhooks/{id}/rerun", pages::webhooks_rerun)
        .with_post("/webhooks/{id}", pages::webhooks_update)
        .with_post("/webhooks/{id}/toggle", pages::webhooks_toggle)
        .with_post("/webhooks/{id}/rotate", pages::webhooks_rotate)
        .with_post("/webhooks/{id}/delete", pages::webhooks_delete)
        // Public trigger (no session; the secret is the credential). Accepts
        // GET and POST so simple senders and JSON POSTers both work.
        .with_get("/hooks/{secret}", pages::webhook_trigger)
        .with_post("/hooks/{secret}", pages::webhook_trigger)
        .with_get("/integrations", pages::integrations_index)
        .with_get("/integrations/callback", pages::integrations_callback)
        .with_post("/integrations/{key}/connect", pages::integrations_connect)
        .with_post(
            "/integrations/{key}/token",
            pages::integrations_connect_token,
        )
        .with_post("/integrations/{key}/retry", pages::integrations_retry)
        .with_post(
            "/integrations/{key}/disconnect",
            pages::integrations_disconnect,
        )
        .with_post(
            "/integrations/{key}/tools/mode",
            pages::integrations_tool_mode,
        )
        .with_post(
            "/integrations/{key}/tools/all",
            pages::integrations_tools_all,
        )
        .with_get("/chat", pages::chat_index)
        // `/chat/search` MUST precede `/chat/{id}` — rama matches routes in
        // registration order, so the `{id}` param would otherwise capture
        // "search" and hand it to `chat_session_view` (a 303 to /chat).
        .with_get("/chat/search", pages::chat_search)
        .with_get("/chat/{id}", pages::chat_session_view)
        .with_post("/chat/sessions", pages::chat_session_create)
        .with_post("/chat/{id}/messages", pages::chat_message_send)
        .with_get("/chat/{id}/tail", pages::chat_tail)
        .with_get("/chat/{id}/document/{doc_id}", pages::chat_document_view)
        .with_post(
            "/chat/{id}/document/{doc_id}/edit",
            pages::chat_document_edit,
        )
        .with_post("/chat/{id}/cancel", pages::chat_cancel)
        .with_post("/chat/{id}/turns/{turn_id}/retry", pages::chat_retry)
        .with_post("/chat/{id}/turns/{turn_id}/edit", pages::chat_edit)
        .with_post(
            "/chat/{id}/turns/{turn_id}/attachment/{filename}/remove",
            pages::chat_attachment_remove,
        )
        .with_post("/chat/{id}/delete", pages::chat_session_delete)
        .with_post("/chat/{id}/share", pages::chat_share_toggle)
        .with_post("/chat/{id}/pin", pages::chat_session_pin)
        .with_post("/chat/{id}/capabilities", pages::chat_capabilities_toggle)
        .with_post("/chat/{id}/effort", pages::chat_effort_set)
        .with_post("/chat/{id}/fork", pages::chat_fork)
        .with_get("/chat/{id}/export.md", pages::chat_export_markdown)
        .with_get("/chat/{id}/export.pdf", pages::chat_export_pdf)
        .with_get(
            "/chat/attachment/{turn_id}/{filename}",
            pages::chat_attachment,
        )
        // `/admin/models/save` + `/admin/models/defaults` + `/admin/models/clear`
        // MUST precede the `/admin/models` GET only in that they don't overlap;
        // registration order is fine since these are distinct static paths.
        .with_get("/admin/models", pages::admin_models_index)
        .with_post("/admin/models/save", pages::admin_models_save)
        .with_post("/admin/models/clear", pages::admin_models_clear)
        .with_post("/admin/models/defaults", pages::admin_models_defaults_save)
        .with_post("/admin/models/search", pages::admin_models_search_save)
        .with_post("/admin/upstreams/reload", pages::admin_upstreams_reload)
        .with_get("/admin/limits", pages::admin_limits_index)
        .with_post("/admin/limits", pages::admin_limits_save)
        .with_post("/admin/limits/delete", pages::admin_limits_delete)
        // Merged pools + backends page. The old `/admin/backends` and
        // `/admin/pools` GET routes 302-redirect here; the CRUD POST endpoints
        // keep their paths (the ids ride in the body, not the URL).
        .with_get("/admin/upstreams", pages::admin_upstreams_index)
        .with_get("/admin/backends", pages::admin_backends_redirect)
        .with_post("/admin/backends/save", pages::admin_backends_save)
        .with_post("/admin/backends/delete", pages::admin_backends_delete)
        .with_get("/admin/pools", pages::admin_pools_redirect)
        .with_post("/admin/pools/save", pages::admin_pools_save)
        .with_post("/admin/pools/delete", pages::admin_pools_delete)
        .with_post("/admin/pools/fallback", pages::admin_pools_fallback_save)
        .with_get("/admin/users", pages::admin_users_index)
        // Target id rides in the POST body (not the path) — rama lowercases
        // path segments, which would mangle case-sensitive OIDC subjects.
        .with_post("/admin/users/impersonate", pages::users_impersonate)
        .with_post("/impersonate/stop", pages::impersonate_stop)
        .with_get("/admin/groups", pages::admin_groups_index)
        .with_post("/admin/groups/save", pages::admin_groups_save)
        .with_post("/admin/groups/delete", pages::admin_groups_delete)
        .with_get("/admin/connectors", pages::admin_connectors_index)
        .with_post("/admin/connectors", pages::admin_connectors_save)
        .with_post(
            "/admin/connectors/restore-defaults",
            pages::admin_connectors_restore,
        )
        .with_post(
            "/admin/connectors/{key}/toggle",
            pages::admin_connectors_toggle,
        )
        .with_post(
            "/admin/connectors/{key}/delete",
            pages::admin_connectors_delete,
        )
        .with_get(
            "/admin/connectors/{key}/audit",
            pages::admin_connectors_audit,
        )
        .with_get("/admin/skills", pages::admin_skills_index)
        .with_get("/admin/skills/download", pages::admin_skills_download)
        .with_post("/admin/skills/upload", pages::admin_skills_upload)
        .with_post("/admin/skills/delete", pages::admin_skills_delete)
        .with_post("/admin/skills/grants", pages::admin_skills_grants_save)
        .with_get("/admin/comfyui", pages::admin_comfyui_index)
        .with_post("/admin/comfyui/reload", pages::admin_comfyui_reload)
        // Per-user private skills (signed-in-user gate, not admin). Distinct
        // from the /admin/skills operator surface above.
        .with_get("/skills", pages::user_skills_index)
        .with_get("/skills/download", pages::user_skills_download)
        .with_post("/skills/upload", pages::user_skills_upload)
        .with_post("/skills/save", pages::user_skills_save)
        .with_post("/skills/delete", pages::user_skills_delete)
        .with_get("/rag", pages::rag_index)
        .with_get("/rag/status", pages::rag_status)
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
        .with_post("/rag/refs/{ref_id}/edit-form", pages::rag_ref_edit_form)
        .with_post("/rag/refs/{ref_id}/cancel-edit", pages::rag_ref_cancel_edit)
        .with_post("/rag/refs/{ref_id}/update", pages::rag_ref_update)
        .with_get("/rag/refs/{ref_id}/log", pages::rag_ref_log)
        .with_post("/theme/toggle", session_core::chrome::theme_toggle)
        .with_post(
            "/nav/toggle/{section}",
            session_core::chrome::nav_sections_toggle,
        )
        .with_post("/lang", session_core::chrome::lang_set)
        .with_get("/v1/models", proxy::list_models)
        // Catch-all param: model ids contain `/` (e.g.
        // `mistralai/Voxtral-Mini-4B-Realtime-2602`).
        .with_get("/v1/models/{*id}", proxy::retrieve_model)
        .with_post("/v1/chat/completions", proxy::chat_completions)
        .with_post("/v1/audio/transcriptions", proxy::transcribe)
        .with_post("/v1/audio/speech", proxy::speech)
        .with_post("/v1/embeddings", proxy::embeddings)
        .with_post("/v1/images/generations", proxy::images_generations)
        .with_post("/v1/images/edits", proxy::images_edits)
        // Bearer-authed download of a file a sandbox run produced for an
        // API caller (scoped to the caller's user; see `sandbox_api`).
        .with_get("/v1/sandbox/files/{run}/{filename}", sandbox_api::download)
        .with_get("/auth/login", oidc_handlers::login)
        .with_get("/auth/callback", oidc_handlers::callback)
        .with_post("/auth/logout", oidc_handlers::logout)
        .with_get("/api/v0/me", api::me)
        .with_get("/api/v0/tokens", api::list_tokens)
        .with_post("/api/v0/tokens", api::create_token)
        .with_post("/api/v0/tokens/{id}/revoke", api::revoke_token)
        .with_post("/api/v0/tokens/{id}/rotate", api::rotate_token)
        .with_put("/api/v0/tokens/{id}/tools", api::update_token_tools)
        .with_delete("/api/v0/tokens/{id}", api::delete_token)
        .with_post("/api/v0/transcriptions", proxy::transcribe_session)
        .with_get("/api/v0/transcription_models", api::transcription_models)
        .with_post("/api/v0/speech", proxy::speech_session)
        .with_get("/api/v0/push/config", api::push_config)
        .with_post("/api/v0/push/subscribe", api::push_subscribe)
        .with_post("/api/v0/push/unsubscribe", api::push_unsubscribe)
        .with_post("/api/v0/me/timezone", api::set_timezone)
        .with_post("/api/v0/me/speech_voice", api::set_speech_voice)
        .with_post("/api/v0/me/location", api::set_location)
        .with_delete("/api/v0/me/location", api::clear_location)
        .with_post(
            "/api/v0/me/location/feedback/{turn_id}",
            api::location_feedback,
        )
        .with_post("/api/v0/me/ask/feedback/{turn_id}", api::ask_feedback)
        .with_get("/api/v0/rag/collections", rag_api::list_collections)
        .with_post("/api/v0/rag/collections", rag_api::create_collection)
        .with_get("/api/v0/rag/collections/{id}", rag_api::get_collection)
        .with_patch("/api/v0/rag/collections/{id}", rag_api::update_collection)
        .with_delete("/api/v0/rag/collections/{id}", rag_api::delete_collection)
        .with_post(
            "/api/v0/rag/collections/{id}/reindex",
            rag_api::reindex_collection,
        )
        .with_post("/api/v0/comfyui/reload", comfyui_api::reload)
        .with_get("/api/v0/comfyui/catalog", comfyui_api::catalog)
}

/// The complete HTTP service: the router plus the layers that make it
/// servable. rc1's `Router` is not `Clone` and surfaces `RouterError`,
/// while `HttpServer::listen` wants a `Clone` service whose error is
/// `Infallible` — `ArcLayer` makes the router shareable/cloneable and
/// `ErrorHandlerLayer` renders any `RouterError` (e.g. an unmatched path)
/// into a `Response`. Both `serve` and the tests build the service through
/// here so they exercise the same stack (notably the 404 handling).
///
/// `V1CorsLayer` sits *outside* the error handler so it decorates every
/// `/v1` response — including the 404/405 a `RouterError` renders (a
/// browser preflight `OPTIONS /v1/…` hits no registered route) and the
/// bearer-auth 401 — with the CORS headers browser SPAs require. It is
/// scoped to `/v1`, so the same-origin HTML UI and `/api/v0` are untouched.
pub fn service(
    state: Arc<RamaState>,
) -> impl rama::Service<
    rama::http::Request,
    Output = rama::http::Response,
    Error = std::convert::Infallible,
> + Clone {
    let router = router(state);
    (V1CorsLayer, ArcLayer::new(), ErrorHandlerLayer::default()).into_layer(router)
}

/// Convenience: build the service and start serving on `addr`.
pub async fn serve(state: Arc<RamaState>, addr: SocketAddress) -> anyhow::Result<()> {
    HttpServer::default()
        .listen(addr, service(state))
        .await
        .map_err(|e| anyhow::anyhow!("rama listen: {e}"))?;
    Ok(())
}
