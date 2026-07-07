// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Conversation search (sidebar magnifier → `GET /chat/search`).
//!
//! What this pins:
//!   - The authed layout renders the search form wired to a Datastar
//!     `@get('/chat/search…')` on submit — NOT a bare native GET. Without
//!     the directive the browser does a full-page navigation that carries
//!     no `Datastar-Request` header, the handler takes its non-datastar
//!     branch, and the in-place results patch never happens. This is the
//!     regression guard for exactly that wiring.
//!   - A datastar GET returns an SSE patch of `#session-list` containing
//!     the matching conversation.
//!   - A stored XSS payload in conversation text comes back escaped in the
//!     snippet (no live `<img>`/`<script>`).
//!   - The no-JS GET (no datastar header) returns a full HTML results page,
//!     not a redirect that silently drops the query.

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

/// A datastar-issued GET — the `datastar-request: true` header is what
/// `is_datastar_request` keys on, selecting the SSE branch.
fn datastar_get(uri: &str, cookie: &str) -> Request {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .header("datastar-request", "true")
        .body(Body::empty())
        .unwrap()
}

/// The rendered sidebar must wire the search form to a Datastar `@get`, so
/// a submit issues an SSE-patch request rather than a native navigation.
#[tokio::test]
async fn sidebar_search_form_is_wired_to_datastar_get() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    // `GET /chat` 303-redirects to a concrete session; render that session
    // page (200) so we can inspect the sidebar it carries.
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    let app = router(state.clone());

    let resp = app
        .serve(get(&format!("/chat/{sid}"), &alice))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();

    assert!(
        body.contains("action=\"/chat/search\""),
        "search form should target /chat/search"
    );
    // The load-bearing assertion: submit must fire a Datastar @get, not a
    // plain form navigation. `__prevent` stops the native GET. Single quotes
    // in the attribute value are HTML-escaped to `&#39;` by the renderer.
    assert!(
        body.contains("data-on:submit__prevent=\"@get(&#39;/chat/search?q=&#39; + encodeURIComponent($searchQuery))\""),
        "search form must submit via Datastar @get, got:\n{body}"
    );
}

/// A datastar GET returns an SSE patch of #session-list with the match.
#[tokio::test]
async fn datastar_search_patches_session_list_with_hits() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    chat::create_user_turn(&state.db, &sid, "t0", "ceph osd timeout tuning")
        .await
        .unwrap();
    let app = router(state.clone());

    let resp = app
        .serve(datastar_get("/chat/search?q=ceph%20osd", &alice))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();

    assert!(
        body.contains("event: datastar-patch-elements"),
        "expected an SSE patch, got:\n{body}"
    );
    assert!(
        body.contains("selector #session-list"),
        "search should patch the session list, got:\n{body}"
    );
    assert!(
        body.contains(&format!("/chat/{sid}")),
        "the matching conversation should appear, got:\n{body}"
    );
    assert!(
        body.contains("<b>"),
        "the match should be highlighted, got:\n{body}"
    );
    // Regression: search-result rows must keep the full sidebar affordances
    // (pin + delete forms, per-row id), not a stripped-down row — otherwise
    // running a search removes the ability to pin/delete from the sidebar.
    assert!(
        body.contains(&format!("#session-row-{sid}"))
            || body.contains(&format!("session-row-{sid}")),
        "search row must carry its per-row id, got:\n{body}"
    );
    assert!(
        body.contains(&format!("/chat/{sid}/pin")),
        "search row must keep the pin form, got:\n{body}"
    );
    assert!(
        body.contains(&format!("/chat/{sid}/delete")),
        "search row must keep the delete form, got:\n{body}"
    );
}

/// A stored XSS payload in conversation text must be escaped in the snippet.
#[tokio::test]
async fn search_snippet_escapes_stored_xss() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    chat::create_user_turn(
        &state.db,
        &sid,
        "t0",
        "danger <img src=x onerror=alert(1)> danger",
    )
    .await
    .unwrap();
    let app = router(state.clone());

    let resp = app
        .serve(datastar_get("/chat/search?q=danger", &alice))
        .await
        .unwrap();
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();

    assert!(
        !body.contains("<img src=x onerror"),
        "raw XSS payload must not appear unescaped in the patch, got:\n{body}"
    );
    assert!(
        body.contains("&lt;img"),
        "the payload should be HTML-escaped, got:\n{body}"
    );
}

/// The no-JS path (no datastar header) returns a full HTML results page,
/// not a redirect that discards the query.
#[tokio::test]
async fn no_js_search_returns_full_results_page() {
    let state = Arc::new(common::state_with_chat_pool("http://unused.invalid").await);
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let sid = chat::create_session(&state.db, "alice").await.unwrap().id;
    chat::create_user_turn(&state.db, &sid, "t0", "unique_marker content")
        .await
        .unwrap();
    let app = router(state.clone());

    let resp = app
        .serve(get("/chat/search?q=unique_marker", &alice))
        .await
        .unwrap();
    // Full page, not a redirect.
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("<!doctype html>") || body.contains("<html"));
    assert!(
        body.contains("Search results for"),
        "expected a results heading, got a page without it"
    );
    assert!(
        body.contains(&format!("/chat/{sid}")),
        "the matching conversation should be linked on the results page"
    );
}
