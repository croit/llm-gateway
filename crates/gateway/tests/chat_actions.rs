// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Retry/edit on chat messages: both truncate the conversation at the
//! target turn and spawn a fresh assistant turn. We assert the
//! synchronous DB effects (truncation + edited text + a new assistant
//! turn); the regenerated content itself streams async and isn't under
//! test here.

mod common;

use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::router::router;
use rama::http::{Body, Method, Request, StatusCode};
use session_core::db::{self as chat, TurnRole, TurnStatus};

fn post_form(uri: &str, cookie: &str, body: &str) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Seed a session with the given (role, id) turns. Assistant turns are
/// finalized as completed so they look like real past replies.
async fn seed_conversation(state: &gateway::rama_server::RamaState, user: &str) -> String {
    let session = chat::create_session(&state.db, user).await.unwrap();
    chat::create_user_turn(&state.db, &session.id, "u0", "first question")
        .await
        .unwrap();
    chat::create_assistant_turn_in_progress(&state.db, &session.id, "a1", "model-a")
        .await
        .unwrap();
    chat::finalize_turn(&state.db, "a1", TurnStatus::Completed, None)
        .await
        .unwrap();
    chat::create_user_turn(&state.db, &session.id, "u2", "second question")
        .await
        .unwrap();
    chat::create_assistant_turn_in_progress(&state.db, &session.id, "a3", "model-a")
        .await
        .unwrap();
    chat::finalize_turn(&state.db, "a3", TurnStatus::Completed, None)
        .await
        .unwrap();
    session.id
}

#[tokio::test]
async fn retry_drops_the_reply_and_everything_below_then_regenerates() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session_id = seed_conversation(&state, "alice").await;
    let app = router(state.clone());

    // Retry the FIRST assistant reply (a1): drops a1, u2, a3.
    let resp = app
        .serve(post_form(
            &format!("/chat/{session_id}/turns/a1/retry"),
            &cookie,
            "model=model-a",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Synchronous effect: only the leading user turn survives, plus a
    // fresh assistant turn created for regeneration.
    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    assert_eq!(turns.len(), 2, "expected u0 + new assistant, got {turns:?}");
    assert_eq!(turns[0].turn.id, "u0");
    assert_eq!(turns[1].turn.role, TurnRole::Assistant);
    assert_ne!(
        turns[1].turn.id, "a1",
        "should be a brand-new assistant turn"
    );
    // The dropped turns are gone.
    assert!(
        chat::get_turn(&state.db, &session_id, "a1")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        chat::get_turn(&state.db, &session_id, "u2")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn edit_rewrites_the_message_drops_below_then_regenerates() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session_id = seed_conversation(&state, "alice").await;
    let app = router(state.clone());

    // Edit the FIRST user turn (u0): rewrite text, drop a1/u2/a3.
    let resp = app
        .serve(post_form(
            &format!("/chat/{session_id}/turns/u0/edit"),
            &cookie,
            "model=model-a&message=rewritten+question",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    assert_eq!(
        turns.len(),
        2,
        "expected edited u0 + new assistant, got {turns:?}"
    );
    assert_eq!(turns[0].turn.id, "u0");
    assert_eq!(
        turns[0].turn.user_content.as_deref(),
        Some("rewritten question")
    );
    assert_eq!(turns[1].turn.role, TurnRole::Assistant);
}

#[tokio::test]
async fn retry_on_a_user_turn_is_rejected() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session_id = seed_conversation(&state, "alice").await;
    let app = router(state.clone());

    let resp = app
        .serve(post_form(
            &format!("/chat/{session_id}/turns/u0/retry"),
            &cookie,
            "model=model-a",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Nothing truncated — the conversation is intact.
    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    assert_eq!(turns.len(), 4);
}

#[tokio::test]
async fn edit_on_an_assistant_turn_is_rejected() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session_id = seed_conversation(&state, "alice").await;
    let app = router(state.clone());

    let resp = app
        .serve(post_form(
            &format!("/chat/{session_id}/turns/a1/edit"),
            &cookie,
            "model=model-a&message=hack",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    assert_eq!(turns.len(), 4);
    // a1 still an assistant turn with no injected user_content.
    let a1 = chat::get_turn(&state.db, &session_id, "a1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a1.role, TurnRole::Assistant);
    assert!(a1.user_content.is_none());
}

#[tokio::test]
async fn cannot_retry_in_someone_elses_session() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let _alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    let session_id = seed_conversation(&state, "alice").await;
    let app = router(state.clone());

    // Bob (authenticated) tries to retry inside Alice's session.
    let resp = app
        .serve(post_form(
            &format!("/chat/{session_id}/turns/a1/retry"),
            &bob,
            "model=model-a",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Alice's conversation is untouched.
    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    assert_eq!(turns.len(), 4);
}
