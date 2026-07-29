// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Auth gates + key derivation for `GET /chat/attachment/{turn_id}/{filename}`.
//!
//! The security-critical bits we want pinned are: the route refuses
//! anonymous callers, refuses cross-user callers (without leaking
//! the turn's existence), and refuses when chat attachments
//! weren't configured. Those branches all return before any S3
//! call, so the test harness can exercise them with a default
//! `Config` (no `[chat.s3]`).
//!
//! The happy path runs against a wiremock server standing in for the
//! object store, which is what lets us assert the *exact* S3 key the
//! handler derives from the URL — the filename's case and its
//! percent-encoded characters have to survive the trip.

use crate::common;

use common::Service as _;
use rama::http::{Body, Method, Request, StatusCode};
use session_core::db as chat;
use wiremock::matchers::method as wm_method;
use wiremock::{Mock, MockServer, ResponseTemplate};

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

/// Drive the happy path for one URL filename segment against a mock object
/// store owned by `alice`, and report `(status, body, paths the store was
/// asked for)`.
async fn fetch_as_owner(url_filename: &str) -> (StatusCode, Vec<u8>, Vec<String>) {
    let s3 = MockServer::start().await;
    Mock::given(wm_method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/markdown")
                .set_body_bytes(b"# Bericht".to_vec()),
        )
        .mount(&s3)
        .await;
    let state = common::state_with_s3(&s3.uri()).await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session = chat::create_session(&state.db, "alice").await.unwrap();
    chat::create_user_turn(&state.db, &session.id, "t-alice", "hi")
        .await
        .unwrap();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            &format!("/chat/attachment/t-alice/{url_filename}"),
            Some(&cookie),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = common::read_body(resp).await.to_vec();
    let asked = s3
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    (status, body, asked)
}

/// A file is stored under the name it was uploaded with — `Bericht.md`, not
/// `bericht.md`. rama's router lowercases the path it matched on, and the
/// `Path` extractor reads its params from *that* string, so a handler that
/// trusts the extractor asks the bucket for a key that was never written and
/// the browser reports "file wasn't available on site". The filename has to
/// come off the untouched URI.
#[tokio::test]
async fn uppercase_filename_survives_into_the_object_key() {
    let (status, body, asked) = fetch_as_owner("Bericht.md").await;
    assert_eq!(asked, ["/test-bucket/chat-attachments/t-alice/Bericht.md"]);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"# Bericht");
}

/// The chip's href percent-encodes everything outside the unreserved set, so
/// a filename with a space arrives as `Bericht%20Q3.md`. The object key holds
/// the *decoded* name — leaving the escape in place would look for a file
/// literally named `Bericht%20Q3.md`.
#[tokio::test]
async fn percent_encoded_filename_is_decoded_for_the_object_key() {
    let (status, _body, asked) = fetch_as_owner("Bericht%20Q3.md").await;
    // rust-s3 re-encodes the space when it builds the request URL, so the
    // wire form comes back as `%20` — but only one level of escaping.
    assert_eq!(
        asked,
        ["/test-bucket/chat-attachments/t-alice/Bericht%20Q3.md"]
    );
    assert_eq!(status, StatusCode::OK);
}

/// The × control on a chip POSTs the same filename back. It has to reach the
/// handler verbatim there too: the marker is matched by exact filename, so a
/// lowercased one silently removes nothing and the chip stays put.
/// No `[chat.s3]` here — the bucket delete is best-effort and skipped, which
/// leaves the marker rewrite as the observable half.
#[tokio::test]
async fn remove_matches_a_mixed_case_filename() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session = chat::create_session(&state.db, "alice").await.unwrap();
    let marker = session_core::attachments::marker_line(
        "Bericht Q3.md",
        "text/markdown",
        "/chat/attachment/t-alice/Bericht%20Q3.md",
        12,
    );
    chat::create_user_turn(
        &state.db,
        &session.id,
        "t-alice",
        &format!("hi\n\n{marker}\n"),
    )
    .await
    .unwrap();
    let db = state.db.clone();
    let session_id = session.id.clone();
    let app = common::app(state);
    let resp = app
        .serve(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/chat/{session_id}/turns/t-alice/attachment/Bericht%20Q3.md/remove"
                ))
                .header("cookie", format!("id={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let turn = chat::get_turn(&db, &session_id, "t-alice")
        .await
        .unwrap()
        .unwrap();
    let content = turn.user_content.unwrap_or_default();
    assert!(
        !content.contains("gw-attachment"),
        "marker should be gone, got: {content}"
    );
    assert!(content.contains("hi"), "prose should survive: {content}");
}
