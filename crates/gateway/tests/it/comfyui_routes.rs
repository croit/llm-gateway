// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Route integration tests for the ComfyUI admin + API endpoints.
//!
//! What this pins:
//!   - GET /admin/comfyui requires admin (anon → /login, non-admin → 403).
//!   - POST /admin/comfyui/reload requires admin and triggers a store
//!     re-scan, redirecting back to the page.
//!   - GET /api/v0/comfyui/catalog requires admin and returns JSON.
//!   - POST /api/v0/comfyui/reload requires admin and returns JSON with
//!     a ReloadReport.
//!   - When [comfyui] is not configured, the catalog endpoint reports
//!     `configured: false` and the admin page renders the "Not configured"
//!     card.

use crate::common;

use common::Service as _;
use gateway::rama_server::RamaState;
use gateway_core::server::db::users;
use jiff::Timestamp;
use rama::http::{Body, Method, Request, StatusCode};

fn req_with_cookie(method: Method, uri: &str, cookie: &str) -> Request {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .body(Body::empty())
        .unwrap()
}

async fn seed_admin(state: &RamaState, user_id: &str) -> String {
    let cookie = common::seed_session(state, user_id, &format!("{user_id}@example.com")).await;
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: user_id.into(),
            email: format!("{user_id}@example.com"),
            name: None,
            roles: vec!["admin".into()],
            created_at: now,
            updated_at: now,
            timezone: None,
        },
    )
    .await
    .unwrap();
    cookie
}

// ----- /admin/comfyui (HTML page) ---------------------------------------

#[tokio::test]
async fn anon_admin_comfyui_redirects_to_login() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/admin/comfyui"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        location.starts_with("/login") && location.contains("return_to="),
        "anon must bounce to /login; got `{location}`"
    );
}

#[tokio::test]
async fn non_admin_admin_comfyui_is_403() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/admin/comfyui", &cookie))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_admin_comfyui_renders_page_without_comfyui() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/admin/comfyui", &cookie))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("ComfyUI workflow catalog"));
    assert!(body.contains("Not configured"));
}

#[tokio::test]
async fn admin_admin_comfyui_renders_page_with_empty_catalog() {
    let state = common::state_with_admin_rbac_and_comfyui("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/admin/comfyui", &cookie))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("ComfyUI workflow catalog"));
    assert!(body.contains("No workflows loaded"));
}

// ----- POST /admin/comfyui/reload (HTML redirect) -----------------------

#[tokio::test]
async fn anon_reload_redirects_to_login() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::POST, "/admin/comfyui/reload"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn non_admin_reload_is_403() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "bob", "bob@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/comfyui/reload",
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_reload_redirects_back() {
    let state = common::state_with_admin_rbac_and_comfyui("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/comfyui/reload",
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(location, "/admin/comfyui");
}

// ----- GET /api/v0/comfyui/catalog (JSON) ------------------------------

#[tokio::test]
async fn anon_catalog_json_is_401() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/api/v0/comfyui/catalog"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn non_admin_catalog_json_is_403() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "carol", "carol@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            "/api/v0/comfyui/catalog",
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_catalog_json_reports_not_configured() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            "/api/v0/comfyui/catalog",
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains(r#""configured":false"#));
}

#[tokio::test]
async fn admin_catalog_json_reports_workflows() {
    let state = common::state_with_admin_rbac_and_comfyui("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            "/api/v0/comfyui/catalog",
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains(r#""configured":true"#));
}

// ----- POST /api/v0/comfyui/reload (JSON) ------------------------------

#[tokio::test]
async fn anon_reload_json_is_401() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::POST, "/api/v0/comfyui/reload"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn non_admin_reload_json_is_403() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "dave", "dave@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/comfyui/reload",
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_reload_json_returns_report() {
    let state = common::state_with_admin_rbac_and_comfyui("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/comfyui/reload",
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains(r#""report""#));
    assert!(body.contains(r#""total":0"#));
}
