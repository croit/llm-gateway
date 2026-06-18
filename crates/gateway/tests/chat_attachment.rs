// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Auth gates for `GET /chat/attachment/{turn_id}/{filename}`.
//!
//! We don't drive the happy path through here because that would
//! require a working S3 mock for the `rust-s3` client. The
//! security-critical bits we DO want pinned are: the route refuses
//! anonymous callers, refuses cross-user callers (without leaking
//! the turn's existence), and refuses when chat attachments
//! weren't configured. Those branches all return before any S3
//! call, so the test harness can exercise them with a default
//! `Config` (no `[chat.s3]`).

mod common;

use common::Service as _;
use rama::http::{Body, Method, Request, StatusCode};
use session_core::db as chat;

fn req_with_cookie(uri: &str, cookie: Option<&str>) -> Request {
    let mut b = Request::builder().method(Method::GET).uri(uri);
    if let Some(c) = cookie {
        b = b.header("cookie", format!("id={c}"));
    }
    b.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn anonymous_caller_is_401() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie("/chat/attachment/t-anon/x.png", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn nonexistent_turn_is_404() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            "/chat/attachment/no-such-turn/x.png",
            Some(&cookie),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cross_user_access_is_404_not_403() {
    // Seed alice with a turn, then attempt access as bob. The
    // route must return 404 (same as "no such turn") so a probing
    // caller can't enumerate other users' turn ids by comparing
    // 403 vs 404. The actual S3 fetch is never reached.
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    common::seed_session(&state, "alice", "alice@example.com").await;
    let alice_session = chat::create_session(&state.db, "alice").await.unwrap();
    let _alice_turn = chat::create_user_turn(&state.db, &alice_session.id, "t-alice", "hi")
        .await
        .unwrap();
    let bob_cookie = common::seed_session(&state, "bob", "bob@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            "/chat/attachment/t-alice/x.png",
            Some(&bob_cookie),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn owner_with_no_s3_config_gets_503() {
    // Owner of the turn, but the test scaffolding doesn't wire up
    // [chat.s3] — we expect a clean 503 rather than a 500/panic.
    // Confirms the order of the auth checks (cookie + ownership
    // pass before config lookup) and that the config-missing path
    // returns the documented status code.
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session = chat::create_session(&state.db, "alice").await.unwrap();
    let _turn = chat::create_user_turn(&state.db, &session.id, "t-alice", "hi")
        .await
        .unwrap();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            "/chat/attachment/t-alice/x.png",
            Some(&cookie),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}
