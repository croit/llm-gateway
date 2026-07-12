// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! CORS for the OpenAI-compatible `/v1/*` API surface.
//!
//! Browsers block a cross-origin `fetch()` to `/v1/*` unless the gateway
//! (a) answers the preflight `OPTIONS /v1/…` without auth and (b) sends
//! `Access-Control-*` headers on every response. These tests pin that
//! wiring end-to-end through the real service stack (`common::app` =
//! `router::service`), so a regression — e.g. dropping the layer or the
//! error-response decoration — fails CI.

mod common;

use common::Service as _;
use rama::http::{Body, Method, Request, StatusCode, header};

/// A browser preflight: `OPTIONS` + `Origin` + the two `Access-Control-
/// Request-*` headers a fetch with a bearer token and JSON body triggers.
fn preflight(uri: &str, origin: &str) -> Request {
    Request::builder()
        .method(Method::OPTIONS)
        .uri(uri)
        .header(header::ORIGIN, origin)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(
            header::ACCESS_CONTROL_REQUEST_HEADERS,
            "authorization,content-type",
        )
        .body(Body::empty())
        .unwrap()
}

fn get_with_origin(uri: &str, origin: &str) -> Request {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(header::ORIGIN, origin)
        .body(Body::empty())
        .unwrap()
}

/// Acceptance criterion #1/#3: `OPTIONS /v1/chat/completions` with an
/// `Origin` returns 2xx and the four CORS headers, with the allow-origin
/// reflecting the sent `Origin` — and it does so *without* auth (no
/// `Authorization` header on this request).
#[tokio::test]
async fn preflight_on_chat_completions_is_unauthenticated_2xx_with_cors() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);

    let resp = app
        .serve(preflight("/v1/chat/completions", "https://app.example.com"))
        .await
        .unwrap();

    // 2xx (we answer 204 No Content), never the pre-CORS 405.
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let h = resp.headers();
    assert_eq!(
        h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
        "https://app.example.com",
        "allow-origin must reflect the sent Origin",
    );
    assert_eq!(
        h.get(header::ACCESS_CONTROL_ALLOW_METHODS).unwrap(),
        "GET, POST, OPTIONS",
    );
    assert_eq!(
        h.get(header::ACCESS_CONTROL_ALLOW_HEADERS).unwrap(),
        "authorization, content-type",
    );
    assert_eq!(h.get(header::ACCESS_CONTROL_MAX_AGE).unwrap(), "86400");
    // Bearer auth ⇒ credentials mode stays off.
    assert!(
        h.get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none(),
        "must not set allow-credentials",
    );
}

/// Preflight works on every `/v1` endpoint, including the multipart upload
/// (`audio/transcriptions`) — requirement #4.
#[tokio::test]
async fn preflight_on_transcriptions_and_embeddings() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);

    for path in ["/v1/audio/transcriptions", "/v1/embeddings", "/v1/models"] {
        let resp = app.serve(preflight(path, "https://x.test")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "preflight {path}");
        assert_eq!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "https://x.test",
            "allow-origin on preflight {path}",
        );
    }
}

/// Requirement #3: the CORS header rides on error responses too. An
/// unauthenticated `GET /v1/models` is a 401, and it must still carry the
/// reflected allow-origin so the browser surfaces the 401 to the app
/// instead of a bare CORS failure.
#[tokio::test]
async fn cors_header_present_on_401_error_response() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);

    let resp = app
        .serve(get_with_origin("/v1/models", "https://spa.example.org"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "https://spa.example.org",
        "error responses must still carry the CORS allow-origin header",
    );
}

/// No `Origin` header (a non-browser client, e.g. curl/SDK) ⇒ allow-origin
/// falls back to `*`, so the endpoint stays usable everywhere.
#[tokio::test]
async fn missing_origin_falls_back_to_wildcard() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);

    let resp = app
        .serve(common::req(Method::OPTIONS, "/v1/chat/completions"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "*",
    );
}

/// Scope guard: CORS is `/v1`-only. A same-origin surface (`/` and the
/// session `/api/v0`) must NOT gain an allow-origin header, and its
/// `OPTIONS` must NOT be short-circuited to 204 by the CORS layer — it
/// falls through to the router (405 Method Not Allowed, as before).
#[tokio::test]
async fn non_v1_paths_are_untouched() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);

    // A real GET route: no CORS header added.
    let resp = app
        .serve(get_with_origin("/healthz", "https://evil.example"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "non-/v1 responses must not gain CORS headers",
    );

    // OPTIONS on a non-/v1 path is not intercepted by the CORS layer.
    let resp = app
        .serve(preflight("/tokens", "https://evil.example"))
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "CORS layer must not answer OPTIONS outside /v1",
    );
    assert!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
    );
}
