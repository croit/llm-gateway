// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `POST /api/v0/me/ask/feedback/{turn_id}` — the browser's answer to an
//! in-flight `ask_user` question.
//!
//! What this pins:
//!   - only the turn's **own** user may answer it (a session cookie alone is
//!     not authorisation — otherwise any logged-in user who learned a turn id
//!     could inject text into someone else's model context),
//!   - an unknown turn and someone else's turn are indistinguishable, so the
//!     endpoint can't be used to probe for live turns,
//!   - answers, empty answers and skips resolve the parked tool with the right
//!     reply, and
//!   - answering a turn nobody is parked on is a no-op, not an error (the tool
//!     may have timed out).

use crate::common;

use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::router::router;
use gateway_runtime::server::tools::feedback::AskReply;
use rama::http::{Body, Method, Request, StatusCode};
use session_core::db as chat;

fn post_json(uri: &str, cookie: &str, body: &str) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Seed a chat session for `user` with one in-progress assistant turn, and
/// return that turn's id — the unit `ask_user` and this endpoint key on.
async fn seed_turn(state: &gateway::rama_server::RamaState, user: &str, turn_id: &str) -> String {
    let session = chat::create_session(&state.db, user).await.unwrap();
    chat::create_user_turn(&state.db, &session.id, &format!("{turn_id}-u"), "question")
        .await
        .unwrap();
    chat::create_assistant_turn_in_progress(&state.db, &session.id, turn_id, "model-a")
        .await
        .unwrap();
    turn_id.to_string()
}

fn url(turn_id: &str) -> String {
    format!("/api/v0/me/ask/feedback/{turn_id}")
}

#[tokio::test]
async fn anon_cannot_answer() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    // Seed alice (a chat session needs a real user row) but send no cookie.
    common::seed_session(&state, "alice", "alice@example.com").await;
    seed_turn(&state, "alice", "a1").await;
    let mut rx = state.ask_feedback.register("a1");

    let app = router(state.clone());
    let resp = app
        .serve(
            Request::builder()
                .method(Method::POST)
                .uri(url("a1"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"injected"}"#.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::OK, "must not accept an answer");
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "nothing may reach the parked tool"
    );
}

#[tokio::test]
async fn the_turns_owner_can_answer_and_the_parked_tool_gets_it() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    seed_turn(&state, "alice", "a1").await;
    // Park like the tool does.
    let rx = state.ask_feedback.register("a1");

    let app = router(state.clone());
    let resp = app
        .serve(post_json(
            &url("a1"),
            &cookie,
            r#"{"choices":["Postgres"],"text":"and please keep the schema"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        rx.await.unwrap(),
        AskReply::Answered {
            choices: vec!["Postgres".into()],
            text: Some("and please keep the schema".into()),
        }
    );
}

#[tokio::test]
async fn another_user_cannot_answer_someone_elses_question() {
    // The core of this endpoint's authorisation: bob is a perfectly valid
    // logged-in user, and still must not be able to answer alice's turn.
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    seed_turn(&state, "alice", "a1").await;
    let mut rx = state.ask_feedback.register("a1");

    let app = router(state.clone());
    let resp = app
        .serve(post_json(
            &url("a1"),
            &bob,
            r#"{"text":"do the wrong thing"}"#,
        ))
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::OK, "bob must be refused");

    // Alice's tool is still parked — nothing was delivered.
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "the parked tool must not have received bob's answer"
    );
}

#[tokio::test]
async fn an_unknown_turn_looks_the_same_as_someone_elses() {
    // Same status + same body for both, so the endpoint can't be used to
    // discover which turn ids exist.
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    seed_turn(&state, "alice", "a1").await;
    let app = router(state.clone());

    let others = app
        .serve(post_json(&url("a1"), &bob, r#"{"text":"x"}"#))
        .await
        .unwrap();
    let others_status = others.status();
    let others_body = String::from_utf8(common::read_body(others).await.to_vec()).unwrap();

    let unknown = app
        .serve(post_json(&url("does-not-exist"), &bob, r#"{"text":"x"}"#))
        .await
        .unwrap();
    let unknown_status = unknown.status();
    let unknown_body = String::from_utf8(common::read_body(unknown).await.to_vec()).unwrap();

    assert_eq!(others_status, unknown_status);
    assert_eq!(others_body, unknown_body);
}

#[tokio::test]
async fn skip_resolves_as_dismissed() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    seed_turn(&state, "alice", "a1").await;
    let rx = state.ask_feedback.register("a1");

    let app = router(state.clone());
    let resp = app
        .serve(post_json(&url("a1"), &cookie, r#"{"dismissed":true}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(rx.await.unwrap(), AskReply::Dismissed);
}

#[tokio::test]
async fn an_empty_answer_is_treated_as_a_skip() {
    // Pressing "Send" with nothing entered must not hand the model
    // `answered: true` with no content in it.
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    seed_turn(&state, "alice", "a1").await;
    let rx = state.ask_feedback.register("a1");

    let app = router(state.clone());
    let resp = app
        .serve(post_json(
            &url("a1"),
            &cookie,
            r#"{"choices":["  "],"text":"   "}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(rx.await.unwrap(), AskReply::Dismissed);
}

#[tokio::test]
async fn answering_a_turn_nobody_waits_on_is_not_an_error() {
    // The tool times out after a few minutes; a late click must not 500.
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    seed_turn(&state, "alice", "a1").await;
    let app = router(state.clone());
    let resp = app
        .serve(post_json(&url("a1"), &cookie, r#"{"text":"late"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_malformed_body_is_rejected() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    seed_turn(&state, "alice", "a1").await;
    let app = router(state.clone());
    let resp = app
        .serve(post_json(&url("a1"), &cookie, "not json at all"))
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// The sibling endpoint, which had the same gap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn location_feedback_also_refuses_another_users_turn() {
    // `POST /me/location/feedback/{turn}` shipped without the ownership check.
    // It matters more there than here: an accepted answer also persists a
    // position onto the turn owner's user row.
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    seed_turn(&state, "alice", "a1").await;
    let mut rx = state.location_feedback.register("a1");

    let app = router(state.clone());
    let resp = app
        .serve(post_json(
            "/api/v0/me/location/feedback/a1",
            &bob,
            r#"{"lat":1.0,"lon":2.0}"#,
        ))
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::OK, "bob must be refused");
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "the parked location tool must not have received bob's answer"
    );

    // And no position was written onto alice.
    assert!(
        gateway_core::server::db::users::find_location(&state.db, "alice")
            .await
            .unwrap()
            .is_none(),
        "bob must not be able to set alice's location"
    );
}

#[tokio::test]
async fn location_feedback_still_works_for_the_owner() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    seed_turn(&state, "alice", "a1").await;
    let rx = state.location_feedback.register("a1");

    let app = router(state.clone());
    let resp = app
        .serve(post_json(
            "/api/v0/me/location/feedback/a1",
            &cookie,
            r#"{"lat":52.5,"lon":13.4}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(matches!(
        rx.await.unwrap(),
        gateway_runtime::server::tools::feedback::BrowserFix::Position { .. }
    ));
}
