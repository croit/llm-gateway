// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Chat pinning (GitHub issue #5): the owner can pin/unpin a conversation;
//! pinned conversations float to the top of the sidebar list. Pinning is a
//! pure UI affordance — owner-only, never affects readability.

mod common;

use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::router::router;
use rama::http::{Body, Method, Request, StatusCode};
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

/// A datastar-issued POST (the `datastar-request: true` header is what
/// `is_datastar_request` keys on) — exercises the SSE branch.
fn datastar_post(uri: &str, cookie: &str, body: &str) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("datastar-request", "true")
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
async fn owner_pin_toggle_flips_the_flag() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    let app = router(state.clone());

    // Pin.
    let resp = app
        .serve(post_form(&format!("/chat/{sid}/pin"), &alice, ""))
        .await
        .unwrap();
    assert!(resp.status().is_redirection());
    assert!(
        chat::get_session(&state.db, "alice", &sid)
            .await
            .unwrap()
            .unwrap()
            .pinned
    );

    // Unpin (toggle back).
    app.serve(post_form(&format!("/chat/{sid}/pin"), &alice, ""))
        .await
        .unwrap();
    assert!(
        !chat::get_session(&state.db, "alice", &sid)
            .await
            .unwrap()
            .unwrap()
            .pinned
    );
}

#[tokio::test]
async fn non_owner_pin_toggle_is_a_noop() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let _alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    let app = router(state.clone());

    let resp = app
        .serve(post_form(&format!("/chat/{sid}/pin"), &bob, ""))
        .await
        .unwrap();
    assert!(resp.status().is_redirection());
    assert!(
        !chat::get_session(&state.db, "alice", &sid)
            .await
            .unwrap()
            .unwrap()
            .pinned,
        "a non-owner must not be able to change a session's pinned flag"
    );
}

#[tokio::test]
async fn pin_datastar_repatches_the_session_list_and_toasts() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    let app = router(state.clone());

    // Pin via the datastar path: 200 SSE that re-patches the whole list and
    // toasts the pinned message.
    let resp = app
        .serve(datastar_post(
            &format!("/chat/{sid}/pin"),
            &alice,
            &format!("active={sid}"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8_lossy(&common::read_body(resp).await).into_owned();
    assert!(
        body.contains("#session-list"),
        "pin must re-patch the whole session list (it re-sorts); got {body}"
    );
    assert!(
        body.contains("Pinned"),
        "pin toast must confirm the pin; got {body}"
    );
    // The active row carried in the form survives as the highlighted row.
    assert!(
        body.contains("session-row--active"),
        "the active conversation must stay highlighted after a pin; got {body}"
    );

    // Unpin: the toast reflects the new state.
    let resp = app
        .serve(datastar_post(
            &format!("/chat/{sid}/pin"),
            &alice,
            &format!("active={sid}"),
        ))
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&common::read_body(resp).await).into_owned();
    assert!(
        body.contains("Unpinned"),
        "unpin toast must say it was unpinned; got {body}"
    );
}

#[tokio::test]
async fn pinned_conversation_floats_to_top_of_rendered_sidebar() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    // `old` is created first; `recent` second, so by recency `recent` leads.
    let old = chat::create_session(&state.db, "alice").await.unwrap();
    chat::set_session_title(&state.db, &old.id, "OLDER-PINNED")
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let recent = chat::create_session(&state.db, "alice").await.unwrap();
    chat::set_session_title(&state.db, &recent.id, "NEWER-UNPINNED")
        .await
        .unwrap();
    let app = router(state.clone());

    // Pin the older one, then render the chat page and assert the pinned
    // (older) row appears before the newer one in the sidebar markup.
    chat::set_pinned(&state.db, "alice", &old.id, true)
        .await
        .unwrap();
    let resp = app
        .serve(get(&format!("/chat/{}", recent.id), &alice))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8_lossy(&common::read_body(resp).await).into_owned();
    // Scope to the sidebar list: the active title also appears in <title> /
    // the page header, which sits above the sidebar in the markup.
    let list_start = body
        .find(r#"id="session-list""#)
        .expect("session list missing");
    let list = &body[list_start..];
    let older_pos = list.find("OLDER-PINNED").expect("pinned title missing");
    let newer_pos = list.find("NEWER-UNPINNED").expect("unpinned title missing");
    assert!(
        older_pos < newer_pos,
        "pinned conversation must render above the more-recent unpinned one"
    );
    // And the pinned row carries the filled-star state class.
    assert!(
        body.contains("session-row__pin--active"),
        "pinned row must render the lit-star state; got {body}"
    );
}
