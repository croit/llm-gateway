// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/api/v0/comfyui/*` — session-authenticated admin API for the
//! hot-reloadable ComfyUI workflow catalog.
//!
//! Admin-only because the catalog is an operator-global resource (a
//! reload affects every chat user). Anonymous → 401 JSON; an
//! authenticated non-admin → 403 JSON.

use std::sync::Arc;

use rama::http::service::web::extract::State;
use rama::http::service::web::response::IntoResponse;
use rama::http::{Request, Response, StatusCode, header};
use serde::Serialize;
use serde_json::json;

use gateway_core::rama_server::state::RamaState;
use gateway_core::server::comfyui::ReloadReport;
use gateway_core::server::db::users;

/// `POST /api/v0/comfyui/reload` — re-scan `[comfyui] content_dir` and
/// atomically swap in the new catalog. In-flight tool calls keep their
/// old snapshot; the next `schema()`/`run()` sees the new one. Returns a
/// [`ReloadReport`] describing what landed and what was skipped so the
/// admin UI can surface the result without grepping logs.
pub async fn reload(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_admin(&state, &req).await {
        return resp;
    }
    let Some(handle) = state.comfyui.as_ref() else {
        return error_envelope(
            StatusCode::CONFLICT,
            "not_configured",
            "[comfyui] is not configured on this gateway",
        );
    };
    // The rescan walks the content dir and parses every manifest.toml +
    // workflow.json synchronously — keep that blocking I/O off the async
    // worker thread.
    let store = handle.store.clone();
    let report: ReloadReport = match tokio::task::spawn_blocking(move || store.reload()).await {
        Ok(report) => report,
        Err(err) => {
            tracing::warn!(error = %err, "comfyui reload task panicked");
            return internal_error("comfyui catalog reload failed");
        }
    };
    tracing::info!(
        loaded = report.total,
        skipped = report.skipped.len(),
        "comfyui catalog reloaded",
    );
    json_ok(&ReloadResponse {
        report,
        base_url: handle.client.base_url().to_string(),
        content_dir: handle.store.dir().display().to_string(),
    })
}

/// `GET /api/v0/comfyui/catalog` — the current snapshot's tool list, for
/// the admin UI. Refreshing this endpoint after `POST …/reload` reflects
/// the new catalog without a gateway restart.
pub async fn catalog(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_admin(&state, &req).await {
        return resp;
    }
    let Some(handle) = state.comfyui.as_ref() else {
        return json_ok(&CatalogResponse {
            configured: false,
            base_url: None,
            content_dir: None,
            workflows: Vec::new(),
        });
    };
    let snapshot = handle.store.current();
    let workflows = snapshot
        .workflows()
        .into_iter()
        .map(|m| CatalogEntry {
            id: m.id.clone(),
            tool_id: format!("comfyui_{}", m.id),
            title: m.title.clone(),
            description: m.description.clone(),
            output_kind: m.output_kind.to_string(),
            params: m
                .params
                .iter()
                .map(|p| CatalogParam {
                    key: p.key.clone(),
                    description: p.description.clone(),
                    required: p.required,
                })
                .collect(),
        })
        .collect();
    json_ok(&CatalogResponse {
        configured: true,
        base_url: Some(handle.client.base_url().to_string()),
        content_dir: Some(handle.store.dir().display().to_string()),
        workflows,
    })
}

#[derive(Serialize)]
struct ReloadResponse {
    report: ReloadReport,
    base_url: String,
    content_dir: String,
}

#[derive(Serialize)]
struct CatalogResponse {
    configured: bool,
    base_url: Option<String>,
    content_dir: Option<String>,
    workflows: Vec<CatalogEntry>,
}

#[derive(Serialize)]
struct CatalogEntry {
    id: String,
    tool_id: String,
    title: String,
    description: String,
    output_kind: String,
    params: Vec<CatalogParam>,
}

#[derive(Serialize)]
struct CatalogParam {
    key: String,
    description: String,
    required: bool,
}

// ----- helpers (mirrors `rag_api`) ---------------------------------------

async fn require_admin(state: &RamaState, req: &Request) -> Result<(), Response> {
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
    Ok(())
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
