// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Chat forking: the recipient of a shared conversation can copy it into
//! their own account (a fresh, private session) and keep chatting. Forking
//! is recipient-only — the owner gets no clone action, and a non-shared
//! chat can't be forked by anyone but its owner-less viewers (i.e. nobody).

mod common;

use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::router::router;
use rama::http::{Body, Method, Request};
use session_core::db as chat;

fn post_form(uri: &str, cookie: &str, body: &str) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Seed a small two-turn conversation owned by `owner` and mark it shared.
async fn shared_convo(state: &gateway::rama_server::RamaState, owner: &str) -> String {
    let sid = chat::create_session(&state.db, owner).await.unwrap().id;
    chat::set_session_title(&state.db, &sid, "Roadmap")
        .await
        .unwrap();
    chat::create_user_turn(&state.db, &sid, "u-0", "what's the plan?")
        .await
        .unwrap();
    let a = chat::create_assistant_turn_in_progress(&state.db, &sid, "a-0", "model-a")
        .await
        .unwrap();
    chat::append_content(&state.db, &a.id, "ship it")
        .await
        .unwrap();
    chat::finalize_turn(&state.db, &a.id, chat::TurnStatus::Completed, None)
        .await
        .unwrap();
    chat::set_shared(&state.db, owner, &sid, true)
        .await
        .unwrap();
    sid
}

#[tokio::test]
async fn recipient_forks_shared_chat_into_own_account() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let _alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    let sid = shared_convo(&state, "alice").await;
    let app = router(state.clone());

    // Bob (the recipient) forks it.
    let resp = app
        .serve(post_form(&format!("/chat/{sid}/fork"), &bob, ""))
        .await
        .unwrap();
    assert!(
        resp.status().is_redirection(),
        "plain fork POST should redirect into the new copy; got {}",
        resp.status()
    );

    // Bob now owns exactly one session — a copy of the conversation.
    let bobs = chat::list_sessions(&state.db, "bob").await.unwrap();
    assert_eq!(bobs.len(), 1, "fork must create one session for bob");
    let fork = &bobs[0];
    assert_ne!(fork.id, sid, "fork must be a distinct session");
    assert!(
        !fork.shared,
        "fork starts private — re-sharing is bob's call"
    );
    assert_eq!(
        fork.title.as_deref(),
        Some("Roadmap"),
        "title copied 1-to-1"
    );

    let turns = chat::list_turns(&state.db, &fork.id).await.unwrap();
    assert_eq!(turns.len(), 2, "both turns copied");
    assert_eq!(
        turns[0].turn.user_content.as_deref(),
        Some("what's the plan?")
    );
    assert_eq!(turns[1].turn.content.as_deref(), Some("ship it"));

    // The original is untouched and still alice's.
    assert!(
        chat::get_session(&state.db, "alice", &sid)
            .await
            .unwrap()
            .is_some()
    );
    assert!(chat::list_sessions(&state.db, "alice").await.unwrap().len() == 1);
}

#[tokio::test]
async fn owner_fork_is_a_noop() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let sid = shared_convo(&state, "alice").await;
    let app = router(state.clone());

    // Alice forks her own chat — recipient-only, so nothing is copied.
    let resp = app
        .serve(post_form(&format!("/chat/{sid}/fork"), &alice, ""))
        .await
        .unwrap();
    assert!(resp.status().is_redirection());
    assert_eq!(
        chat::list_sessions(&state.db, "alice").await.unwrap().len(),
        1,
        "forking your own chat must not duplicate it"
    );
}

#[tokio::test]
async fn non_owner_cannot_fork_unshared_chat() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let _alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    // Private (un-shared) conversation.
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    chat::create_user_turn(&state.db, &sid, "u-0", "secret")
        .await
        .unwrap();
    let app = router(state.clone());

    let resp = app
        .serve(post_form(&format!("/chat/{sid}/fork"), &bob, ""))
        .await
        .unwrap();
    assert!(
        resp.status().is_redirection(),
        "fork of a chat bob can't read just bounces him to /chat"
    );
    assert!(
        chat::list_sessions(&state.db, "bob")
            .await
            .unwrap()
            .is_empty(),
        "a non-readable chat must not be forkable"
    );
}
