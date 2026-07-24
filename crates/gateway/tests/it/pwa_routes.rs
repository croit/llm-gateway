// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! PWA route wiring tests: the manifest, service worker, favicon, and
//! icons must all return 200 **without** authentication, with the
//! correct `Content-Type`, and with appropriate cache headers.
//!
//! None of the PWA assets are content-hashed, so none may use the
//! `immutable` cache directive — otherwise a manifest/icon/favicon
//! change served at the same URL would be pinned in browsers for a
//! year. They all use a short, revalidating `max-age` instead.

use crate::common;

use common::Service as _;
use rama::http::{Method, StatusCode, header};

/// Build an app, fire a GET, return (status, content-type,
/// cache-control) from the response.
async fn pwa_get(uri: &str) -> (StatusCode, String, String) {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app.serve(common::req(Method::GET, uri)).await.unwrap();
    let status = resp.status();
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap_or_default().to_string())
        .unwrap_or_default();
    let cc = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .map(|v| v.to_str().unwrap_or_default().to_string())
        .unwrap_or_default();
    let _ = common::read_body(resp).await;
    (status, ct, cc)
}

#[tokio::test]
async fn manifest_returns_ok_without_auth() {
    let (status, ct, cc) = pwa_get("/manifest.webmanifest").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ct.starts_with("application/manifest+json"),
        "expected manifest content-type, got {ct}"
    );
    // Manifest must NOT be immutable-cached — updates must roll out.
    assert!(
        !cc.contains("immutable"),
        "manifest must not use immutable cache, got: {cc}"
    );
}

#[tokio::test]
async fn sw_js_returns_ok_without_auth() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app.serve(common::req(Method::GET, "/sw.js")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap_or_default().to_string())
        .unwrap_or_default();
    assert!(
        ct.starts_with("application/javascript"),
        "expected JS content-type, got {ct}"
    );
    let cc = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .map(|v| v.to_str().unwrap_or_default().to_string())
        .unwrap_or_default();
    assert!(
        !cc.contains("immutable"),
        "service worker must not use immutable cache, got: {cc}"
    );
    // Service-Worker-Allowed header must be present for root scope.
    let swa = resp
        .headers()
        .get("service-worker-allowed")
        .map(|v| v.to_str().unwrap_or_default().to_string())
        .unwrap_or_default();
    assert_eq!(swa, "/", "expected Service-Worker-Allowed: /, got: {swa}");
    let _ = common::read_body(resp).await;
}

#[tokio::test]
async fn favicon_returns_ok_without_auth() {
    let (status, ct, _cc) = pwa_get("/favicon.ico").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ct.starts_with("image/x-icon") || ct.starts_with("image/vnd"),
        "expected icon content-type, got {ct}"
    );
}

#[tokio::test]
async fn icons_return_ok_without_auth() {
    for name in &[
        "icon-192.png",
        "icon-512.png",
        "icon-maskable-512.png",
        "apple-touch-icon.png",
    ] {
        let uri = format!("/icons/{name}");
        let (status, ct, cc) = pwa_get(&uri).await;
        assert_eq!(status, StatusCode::OK, "failed for {name}");
        assert!(
            ct.starts_with("image/png"),
            "expected image/png for {name}, got {ct}"
        );
        // Icons are not content-hashed, so they must NOT be immutable —
        // a rebrand reusing the same filenames must be able to roll out.
        assert!(
            !cc.contains("immutable"),
            "icons must not use immutable cache for {name}, got {cc}"
        );
        assert!(
            cc.contains("max-age"),
            "expected a max-age cache header for {name}, got {cc}"
        );
    }
}

#[tokio::test]
async fn unknown_icon_returns_404() {
    let (status, _ct, _cc) = pwa_get("/icons/nope.png").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn manifest_contains_required_pwa_fields() {
    let (status, _ct, _cc) = pwa_get("/manifest.webmanifest").await;
    assert_eq!(status, StatusCode::OK);
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/manifest.webmanifest"))
        .await
        .unwrap();
    let body = common::read_body(resp).await;
    let json: serde_json::Value = serde_json::from_slice(&body).expect("manifest is valid JSON");
    assert_eq!(json["start_url"], "/");
    assert_eq!(json["scope"], "/");
    assert_eq!(json["display"], "standalone");
    let icons = json["icons"].as_array().expect("icons array");
    assert!(
        icons
            .iter()
            .any(|i| { i["sizes"] == "192x192" && i["type"] == "image/png" }),
        "manifest must include a 192x192 icon"
    );
    assert!(
        icons
            .iter()
            .any(|i| { i["sizes"] == "512x512" && i["purpose"] == "maskable" }),
        "manifest must include a maskable 512x512 icon"
    );
}
