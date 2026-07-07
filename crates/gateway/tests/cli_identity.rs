// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Bearer-authed identity endpoints the `gw` CLI depends on: `GET /v1/me`
//! (`gw auth whoami` / `gw auth tools`) and `POST /v1/auth/logout`
//! (`gw auth logout`). These pin the CLI↔server contract — the CLI hard-codes
//! these exact `/v1/…` paths and authenticates with a bearer token, so a
//! missing route or a session-only gate silently breaks those commands.

mod common;

use common::Service as _;
use rama::http::{Body, Method, Request, StatusCode};

fn bearer_get(uri: &str, bearer: &str) -> Request {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn v1_me_without_bearer_is_401() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app.serve(common::req(Method::GET, "/v1/me")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn v1_me_returns_identity_for_bearer() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let resp = app.serve(bearer_get("/v1/me", &bearer)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::read_body(resp).await;
    let me: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(me["id"], "alice");
    assert_eq!(me["email"], "alice@example.com");
    // Shape the CLI's `Me` deserializes / `gw auth tools` iterates.
    assert!(me["roles"].is_array());
    assert!(me["allowed_tools"].is_array());
}

#[tokio::test]
async fn v1_auth_logout_revokes_the_calling_token() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    // Works before logout.
    let resp = app.serve(bearer_get("/v1/me", &bearer)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Self-revoke via the CLI's logout path.
    let logout = Request::builder()
        .method(Method::POST)
        .uri("/v1/auth/logout")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(logout).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The same token no longer authenticates.
    let resp = app.serve(bearer_get("/v1/me", &bearer)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
