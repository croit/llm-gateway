// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! /healthz / /readyz are unauthenticated and always 200. The simplest
//! test in the suite — also doubles as a smoke check that the test
//! scaffolding (state, router, serve) hangs together.

mod common;

use common::Service as _;
use rama::http::{Method, StatusCode};

#[tokio::test]
async fn healthz_returns_ok() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/healthz"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::read_body(resp).await;
    assert!(
        body.starts_with(b"{\"status\":\"ok\""),
        "unexpected body: {body:?}"
    );
}

#[tokio::test]
async fn readyz_returns_ok() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/readyz"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_path_is_404() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/no-such-route"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
