// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Chat sharing: a `shared` session is read-only-readable by any signed-in
//! user who knows its UUID; mutations (incl. re-sharing) stay owner-only.

mod common;

use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::router::router;
use rama::http::{Body, Method, Request, StatusCode};
use session_core::db as chat;

fn get(uri: &str, cookie: &str) -> Request {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .body(Body::empty())
        .unwrap()
}

fn post_form(uri: &str, cookie: &str, body: &str) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A datastar-issued POST (the `datastar-request: true` header is what
/// `is_datastar_request` keys on) — exercises the SSE branch rather than the
/// no-JS redirect fallback.
fn datastar_post(uri: &str, cookie: &str) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("datastar-request", "true")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn non_owner_cannot_view_unshared_session() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let _alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    let app = router(state.clone());

    let resp = app.serve(get(&format!("/chat/{sid}"), &bob)).await.unwrap();
    assert!(
        resp.status().is_redirection(),
        "non-owner must not read a private chat; got {}",
        resp.status()
    );
}

#[tokio::test]
async fn shared_session_is_readable_read_only_by_other_user() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    let app = router(state.clone());

    // Owner shares.
    let resp = app
        .serve(post_form(&format!("/chat/{sid}/share"), &alice, ""))
        .await
        .unwrap();
    assert!(resp.status().is_redirection());
    assert!(
        chat::get_session(&state.db, "alice", &sid)
            .await
            .unwrap()
            .unwrap()
            .shared
    );

    // Bob can now read it — and it's read-only (banner present, no composer
    // message-POST URL rendered).
    let resp = app.serve(get(&format!("/chat/{sid}"), &bob)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "shared chat must be viewable by another signed-in user"
    );
    let body = String::from_utf8_lossy(&common::read_body(resp).await).into_owned();
    assert!(body.contains("read-only"), "expected the read-only banner");
    assert!(
        !body.contains(&format!("/chat/{sid}/messages")),
        "composer must not render for a read-only viewer"
    );
}

#[tokio::test]
async fn non_owner_share_toggle_is_a_noop() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    let app = router(state.clone());

    app.serve(post_form(&format!("/chat/{sid}/share"), &alice, ""))
        .await
        .unwrap();
    // Bob's toggle would unshare it — must have no effect.
    let resp = app
        .serve(post_form(&format!("/chat/{sid}/share"), &bob, ""))
        .await
        .unwrap();
    assert!(resp.status().is_redirection());
    assert!(
        chat::get_session(&state.db, "alice", &sid)
            .await
            .unwrap()
            .unwrap()
            .shared,
        "a non-owner must not be able to change a session's shared flag"
    );
}

#[tokio::test]
async fn non_owner_view_does_not_error_owners_in_progress_turn() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let _alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    // Alice shares the chat and has a live (in_progress) assistant turn. No
    // worker is registered in this test, so to *her* it's an orphan the render
    // sweep would clear — but a non-owner's read must never run that sweep.
    chat::set_shared(&state.db, "alice", &sid, true)
        .await
        .unwrap();
    chat::create_user_turn(&state.db, &sid, "u-turn", "hi")
        .await
        .unwrap();
    chat::create_assistant_turn_in_progress(&state.db, &sid, "a-turn", "model-a")
        .await
        .unwrap();
    let app = router(state.clone());

    // Bob (non-owner) reads the shared chat...
    let resp = app.serve(get(&format!("/chat/{sid}"), &bob)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // ...and the owner's in-progress turn is untouched.
    let turn = chat::get_turn(&state.db, &sid, "a-turn")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        turn.status,
        chat::TurnStatus::InProgress,
        "a non-owner's read must not flip the owner's live turn to errored",
    );

    // The owner's own view still sweeps a genuine orphan (proving the gate is
    // what differentiates the two readers, not a disabled sweep).
    let alice = _alice;
    let resp = app
        .serve(get(&format!("/chat/{sid}"), &alice))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let turn = chat::get_turn(&state.db, &sid, "a-turn")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        turn.status,
        chat::TurnStatus::Errored,
        "the owner's render sweep should still clear a real orphan",
    );
}

#[tokio::test]
async fn share_toggle_datastar_toast_reflects_actual_state() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    let app = router(state.clone());

    // Enable: 200 SSE that re-patches the toggle and toasts the *share*
    // message. (The label flip + toast come from the server, not a stale
    // client guess.)
    let resp = app
        .serve(datastar_post(&format!("/chat/{sid}/share"), &alice))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8_lossy(&common::read_body(resp).await).into_owned();
    assert!(
        body.contains("#share-toggle"),
        "enable must re-patch the toggle in place; got {body}"
    );
    assert!(
        body.contains("Shared"),
        "enable must flip the label to shared; got {body}"
    );
    assert!(
        body.contains("read along"),
        "enable toast must state others can now read; got {body}"
    );
    assert!(
        chat::get_session(&state.db, "alice", &sid)
            .await
            .unwrap()
            .unwrap()
            .shared
    );

    // Disable: the toast must reflect the *new* state ("Sharing stopped") and
    // never claim others can read — this is the stale-view bug the fix closes.
    let resp = app
        .serve(datastar_post(&format!("/chat/{sid}/share"), &alice))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8_lossy(&common::read_body(resp).await).into_owned();
    assert!(
        body.contains("Sharing stopped"),
        "disable toast must say sharing ended; got {body}"
    );
    assert!(
        !body.contains("read along"),
        "disable must never claim others can still read; got {body}"
    );
    assert!(
        !chat::get_session(&state.db, "alice", &sid)
            .await
            .unwrap()
            .unwrap()
            .shared
    );
}

#[tokio::test]
async fn unauthenticated_deep_link_preserves_return_to() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    // The unauthenticated bounce fires before any DB lookup, so the session
    // need not exist — a literal id keeps the test about the redirect alone.
    let sid = "deadbeef-0000-0000-0000-000000000000";
    let app = router(state.clone());

    // No cookie → bounced to /login, but the requested chat URL must survive as
    // `?return_to=` so the post-login callback lands back on the shared chat.
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/chat/{sid}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert!(
        resp.status().is_redirection(),
        "unauthenticated deep link must redirect to login; got {}",
        resp.status()
    );
    let loc = resp
        .headers()
        .get(rama::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        loc.starts_with("/login?"),
        "login bounce must carry a query; got {loc}"
    );
    assert!(loc.contains("return_to="), "missing return_to; got {loc}");
    assert!(
        loc.contains(sid),
        "return_to must point back at the deep link; got {loc}"
    );
}

#[tokio::test]
async fn login_page_forwards_return_to_into_the_form() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let app = router(state.clone());

    let req = Request::builder()
        .method(Method::GET)
        .uri("/login?return_to=%2Fchat%2Fabc-123")
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8_lossy(&common::read_body(resp).await).into_owned();
    assert!(
        body.contains(r#"name="return_to""#),
        "login form must forward return_to as a hidden field"
    );
    assert!(
        body.contains("/chat/abc-123"),
        "the forwarded value must be the requested path"
    );
}

#[tokio::test]
async fn non_owner_cannot_post_messages_even_when_shared() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    let app = router(state.clone());

    app.serve(post_form(&format!("/chat/{sid}/share"), &alice, ""))
        .await
        .unwrap();
    // Bob tries to post into the shared (read-only-for-him) chat.
    let _ = app
        .serve(post_form(
            &format!("/chat/{sid}/messages"),
            &bob,
            "model=model-a&message=hi",
        ))
        .await
        .unwrap();
    // The mutation is owner-only, so no turn was created.
    let turns = chat::list_turns(&state.db, &sid).await.unwrap();
    assert!(
        turns.is_empty(),
        "non-owner message must not create a turn; got {}",
        turns.len()
    );
}
