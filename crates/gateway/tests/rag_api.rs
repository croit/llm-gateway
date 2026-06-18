// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! /api/v0/rag/* — session-authed RAG admin endpoints.
//!
//! Exercises the CRUD + reindex surface against the in-memory test
//! state. The indexer worker itself is `None` in this state (no
//! upstreams pool is wired here), but the API doesn't depend on it —
//! everything it does runs through the rag DB tables.

mod common;

use common::Service as _;
use rama::http::{Body, Method, Request, StatusCode};
use serde_json::{Value, json};

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

fn create_body() -> &'static str {
    r#"{
        "name": "gateway-repo",
        "description": "the gateway codebase",
        "git_url": "https://example.invalid/gateway.git",
        "git_ref": "main",
        "embedding_model": "embed-1",
        "include_globs": ["*.rs"],
        "exclude_globs": ["target/"],
        "chunk_size": 512,
        "chunk_overlap": 64
    }"#
}

#[tokio::test]
async fn list_collections_anonymous_is_401() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/api/v0/rag/collections"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn full_create_get_list_update_reindex_delete_round_trip() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let db = state.db.clone();
    let app = common::app(state);

    // Empty list to start.
    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            "/api/v0/rag/collections",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["data"].as_array().unwrap().len(), 0);

    // Create.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/rag/collections",
            &cookie,
            Some(create_body()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(created["name"], "gateway-repo");
    assert_eq!(created["pat_set"], false);
    assert_eq!(created["status"], "pending");
    let id = created["id"].as_i64().unwrap();

    // Get one.
    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            &format!("/api/v0/rag/collections/{id}"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let fetched: Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(fetched["id"], id);
    assert_eq!(fetched["description"], "the gateway codebase");
    assert_eq!(fetched["include_globs"], json!(["*.rs"]));

    // Update: set a PAT + tweak embedding_model.
    let resp = app
        .serve(req_with_cookie(
            Method::PATCH,
            &format!("/api/v0/rag/collections/{id}"),
            &cookie,
            Some(r#"{"pat": "ghp_secretvalue", "embedding_model": "embed-2"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let updated: Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(updated["pat_set"], true, "PAT must read as set");
    assert!(
        updated.get("pat").is_none(),
        "raw PAT must NEVER appear in the response shape"
    );
    assert_eq!(updated["embedding_model"], "embed-2");

    // Reindex: the row was 'pending', set it to error first, then bump back.
    {
        // Move status off 'pending' by writing the DB directly so we
        // can prove reindex flips it back.
        use gateway::server::db::rag as rag_db;
        rag_db::mark_failed(&db, id, "manual force").await.unwrap();
    }
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/api/v0/rag/collections/{id}/reindex"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let after: Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(after["status"], "pending");
    assert!(after["last_error"].is_null());

    // List should now have one entry.
    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            "/api/v0/rag/collections",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    let listed: Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);

    // Delete.
    let resp = app
        .serve(req_with_cookie(
            Method::DELETE,
            &format!("/api/v0/rag/collections/{id}"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Get on deleted id → 404.
    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            &format!("/api/v0/rag/collections/{id}"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_rejects_duplicate_name_with_a_helpful_400() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);

    let _ = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/rag/collections",
            &cookie,
            Some(create_body()),
        ))
        .await
        .unwrap();
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/rag/collections",
            &cookie,
            Some(create_body()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let parsed: Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    let msg = parsed["error"]["message"].as_str().unwrap();
    assert!(msg.contains("already exists"), "{msg}");
}

#[tokio::test]
async fn create_validates_inputs() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);

    // chunk_overlap >= chunk_size
    let body = r#"{
        "name": "bad", "git_url": "u", "embedding_model": "m",
        "chunk_size": 100, "chunk_overlap": 100
    }"#;
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/rag/collections",
            &cookie,
            Some(body),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn update_with_empty_body_returns_current_state() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/rag/collections",
            &cookie,
            Some(create_body()),
        ))
        .await
        .unwrap();
    let created: Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    let id = created["id"].as_i64().unwrap();

    let resp = app
        .serve(req_with_cookie(
            Method::PATCH,
            &format!("/api/v0/rag/collections/{id}"),
            &cookie,
            Some("{}"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(v["id"], id);
    assert_eq!(v["embedding_model"], "embed-1");
}

#[tokio::test]
async fn update_can_clear_pat() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);
    // Create with a PAT.
    let body = r#"{
        "name": "with-pat",
        "git_url": "https://example.invalid/private.git",
        "embedding_model": "embed-1",
        "pat": "ghp_token"
    }"#;
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/rag/collections",
            &cookie,
            Some(body),
        ))
        .await
        .unwrap();
    let created: Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(created["pat_set"], true);
    let id = created["id"].as_i64().unwrap();

    // PATCH with `"pat": null` → cleared.
    let resp = app
        .serve(req_with_cookie(
            Method::PATCH,
            &format!("/api/v0/rag/collections/{id}"),
            &cookie,
            Some(r#"{"pat": null}"#),
        ))
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(v["pat_set"], false);
}
