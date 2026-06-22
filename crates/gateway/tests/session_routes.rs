// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! /api/v0/* — session-authed JSON endpoints (token CRUD, /me).

mod common;

use common::Service as _;
use rama::http::{Body, Method, Request, StatusCode};

fn req_with_cookie(method: Method, uri: &str, cookie: &str, body: Option<&str>) -> Request {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("id={cookie}"));
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    b.body(Body::from(body.unwrap_or("").to_string())).unwrap()
}

#[tokio::test]
async fn me_anonymous_is_401() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/api/v0/me"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_with_session_returns_user() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/api/v0/me", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["id"], "alice");
    assert_eq!(parsed["email"], "alice@example.com");
}

#[tokio::test]
async fn create_then_list_then_revoke_then_delete() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "bob", "bob@example.com").await;
    let app = common::app(state);

    // List → empty
    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            "/api/v0/tokens",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 0);

    // Create
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/tokens",
            &cookie,
            Some(r#"{"name":"laptop","ttl_days":30}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert!(parsed["plaintext"].as_str().unwrap().starts_with("gwk_"));
    let token_id = parsed["token"]["id"].as_str().unwrap().to_string();

    // Revoke
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/api/v0/tokens/{token_id}/revoke"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["revoked"], true);

    // Delete (hard-removes the revoked row)
    let resp = app
        .serve(req_with_cookie(
            Method::DELETE,
            &format!("/api/v0/tokens/{token_id}"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["deleted"], true);

    // List → empty again
    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            "/api/v0/tokens",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    let list: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert!(list.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn rotate_swaps_secret_then_refuses_revoked() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "dave", "dave@example.com").await;
    let app = common::app(state);

    // Create a token, capture its first plaintext + id.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/tokens",
            &cookie,
            Some(r#"{"name":"ci","ttl_days":30}"#),
        ))
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    let token_id = parsed["token"]["id"].as_str().unwrap().to_string();
    let first_plaintext = parsed["plaintext"].as_str().unwrap().to_string();

    // Rotate → 200 with a fresh plaintext, same id + name.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/api/v0/tokens/{token_id}/rotate"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    let second_plaintext = parsed["plaintext"].as_str().unwrap().to_string();
    assert!(second_plaintext.starts_with("gwk_"));
    assert_ne!(
        first_plaintext, second_plaintext,
        "rotation must issue a new secret"
    );
    assert_eq!(parsed["token"]["id"], token_id);
    assert_eq!(parsed["token"]["name"], "ci");
    assert_eq!(parsed["token"]["revoked"], false);

    // Revoke, then rotate again → 404 (a revoked token can't be resurrected).
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/api/v0/tokens/{token_id}/revoke"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/api/v0/tokens/{token_id}/rotate"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cannot_delete_active_token() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "carol", "carol@example.com").await;
    let app = common::app(state);

    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/tokens",
            &cookie,
            Some(r#"{"name":"active","ttl_days":30}"#),
        ))
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    let token_id = parsed["token"]["id"].as_str().unwrap().to_string();

    let resp = app
        .serve(req_with_cookie(
            Method::DELETE,
            &format!("/api/v0/tokens/{token_id}"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    // Hard-delete refuses non-revoked tokens — keeps the audit trail
    // honest if a token gets stolen.
    assert_eq!(parsed["deleted"], false);
}
