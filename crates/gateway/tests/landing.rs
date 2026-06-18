// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Landing-page behaviour: `/` is the chat surface (not a dashboard),
//! and the identity info that used to live on the dashboard now sits in
//! a low-key "Account" section on the /tokens page.

mod common;

use common::Service as _;
use rama::http::{Method, StatusCode};

/// A signed-in browser hitting `/` lands in chat — a plain navigation
/// (no Datastar header) gets a 303 to a concrete `/chat/{id}` URL.
#[tokio::test]
async fn root_redirects_browser_to_chat() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);

    let req = rama::http::Request::builder()
        .method(Method::GET)
        .uri("/")
        .header("cookie", format!("id={cookie}"))
        .body(rama::http::Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get(rama::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        location.starts_with("/chat/"),
        "expected redirect into a chat session, got `{location}`"
    );
}

/// Anonymous on `/` still bounces to /login — `/` is an authed surface.
#[tokio::test]
async fn root_anonymous_redirects_to_login() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);

    let resp = app.serve(common::req(Method::GET, "/")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get(rama::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        location.starts_with("/login"),
        "anon on `/` must bounce to /login; got `{location}`"
    );
}

/// The identity info (email, user id, role summary) now renders inside
/// the /tokens page rather than on a prominent landing dashboard.
#[tokio::test]
async fn tokens_page_includes_account_section() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);

    let req = rama::http::Request::builder()
        .method(Method::GET)
        .uri("/tokens")
        .header("cookie", format!("id={cookie}"))
        .body(rama::http::Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();

    assert!(body.contains("Account"), "expected an Account section");
    assert!(
        body.contains("alice@example.com"),
        "expected the signed-in email"
    );
    assert!(body.contains("User ID"), "expected the User ID label");
    assert!(body.contains("alice"), "expected the user id value");
}
