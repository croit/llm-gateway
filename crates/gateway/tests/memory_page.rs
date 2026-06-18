// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The per-user `/memory` page: inspect memories grouped by kind, add,
//! edit, and delete them — all scoped to the session user.

mod common;

use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::router::router;
use gateway::server::db::user_memories::{self, MemoryKind};
use rama::http::{Body, Method, Request, StatusCode};

fn post_form(uri: &str, cookie: &str, body: &str) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str, cookie: &str) -> Request {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn memory_anonymous_redirects_to_login() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/memory"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get(rama::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        location.starts_with("/login") && location.contains("return_to="),
        "anon must bounce to /login carrying return_to; got `{location}`"
    );
}

#[tokio::test]
async fn page_lists_memories_grouped_by_kind() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    user_memories::insert(
        &state.db,
        "alice",
        MemoryKind::Preference,
        "metric units please",
    )
    .await
    .unwrap();
    user_memories::insert(
        &state.db,
        "alice",
        MemoryKind::Project,
        "building the llm gateway",
    )
    .await
    .unwrap();
    let app = router(Arc::new(state));

    let resp = app.serve(get("/memory", &cookie)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();

    assert!(body.contains("Preferences"), "expected Preferences heading");
    assert!(
        body.contains("Project context"),
        "expected Project context heading"
    );
    assert!(body.contains("Facts"), "expected Facts heading");
    assert!(body.contains("metric units please"));
    assert!(body.contains("building the llm gateway"));
}

#[tokio::test]
async fn create_appends_and_persists() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = router(state.clone());

    let resp = app
        .serve(post_form(
            "/memory",
            &cookie,
            "kind=preference&content=likes+concise+answers",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("event: datastar-patch-elements"),
        "expected an SSE patch, got:\n{body}"
    );
    assert!(
        body.contains("#mem-list-preference"),
        "expected append into the preference list, got:\n{body}"
    );

    let rows = user_memories::list_for_user(&state.db, "alice", 50)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, MemoryKind::Preference);
    assert_eq!(rows[0].content, "likes concise answers");
}

#[tokio::test]
async fn edit_updates_content() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let m = user_memories::insert(&state.db, "alice", MemoryKind::Fact, "old text")
        .await
        .unwrap();
    let app = router(state.clone());

    let resp = app
        .serve(post_form(
            &format!("/memory/{}/edit", m.id),
            &cookie,
            "content=new+text",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains(&format!("#mem-row-{}", m.id)));

    let rows = user_memories::list_for_user(&state.db, "alice", 50)
        .await
        .unwrap();
    assert_eq!(rows[0].content, "new text");
    // Kind is preserved across an edit.
    assert_eq!(rows[0].kind, MemoryKind::Fact);
}

#[tokio::test]
async fn delete_removes_the_row() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let m = user_memories::insert(&state.db, "alice", MemoryKind::Fact, "to be removed")
        .await
        .unwrap();
    let app = router(state.clone());

    let resp = app
        .serve(post_form(&format!("/memory/{}/delete", m.id), &cookie, ""))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("data: mode remove"),
        "expected a remove patch"
    );

    assert!(
        user_memories::list_for_user(&state.db, "alice", 50)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn cannot_edit_another_users_memory() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let _alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let bob_cookie = common::seed_session(&state, "bob", "bob@example.com").await;
    let m = user_memories::insert(&state.db, "alice", MemoryKind::Fact, "alice only")
        .await
        .unwrap();
    let app = router(state.clone());

    // Bob tries to edit Alice's memory → rejected, Alice's row untouched.
    let resp = app
        .serve(post_form(
            &format!("/memory/{}/edit", m.id),
            &bob_cookie,
            "content=pwned",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("not found"), "expected a not-found toast");

    let rows = user_memories::list_for_user(&state.db, "alice", 50)
        .await
        .unwrap();
    assert_eq!(rows[0].content, "alice only");
}
