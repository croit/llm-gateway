// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Static-asset handlers — the Tailwind/daisyUI CSS bundle, the
//! Datastar JS runtime, and our own JS glue, baked into the binary via
//! `include_bytes!`. The gateway depends on `session-core` and pulls
//! these from here so the bundle, the cache key, and the served URL
//! stay consistent.
//!
//! The CSS is produced by `npm run build` in `ui/`; the JS is the
//! upstream Datastar release pulled in at branch-bootstrap time. The
//! `ui/src/main.css` `@source` globs scan the gateway and session-core
//! crates so utility classes used anywhere in the workspace survive
//! Tailwind's tree-shake.
//!
//! ## Cache busting
//!
//! Each bundle's URL carries a `?v=<8-byte-sha256-prefix>` of the bundle
//! bytes. The hash is computed once at startup via `LazyLock`. With the
//! query string acting as a per-content cache key, we can serve the
//! files as `Cache-Control: public, max-age=31536000, immutable` — the
//! browser keeps them indefinitely and only re-fetches when the
//! template-emitted URL changes after a deploy.

use std::sync::LazyLock;

use rama::http::service::web::response::IntoResponse;
use rama::http::{Response, StatusCode, header};
use sha2::{Digest, Sha256};

const APP_CSS: &[u8] = include_bytes!("../assets/app.css");
const DATASTAR_JS: &[u8] = include_bytes!("../assets/datastar.js");
const APP_JS: &[u8] = include_bytes!("../assets/app.js");
const PCM_RECORDER_JS: &[u8] = include_bytes!("../assets/pcm-recorder.js");

/// Long-lived caching tag for content-hashed asset URLs. `immutable`
/// is what tells modern browsers to skip the revalidation round-trip
/// entirely — without it they still issue a conditional GET on every
/// reload despite the year-long max-age.
const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";

/// 8-byte (16 hex chars) prefix of the asset's sha256 — enough entropy
/// to avoid collisions across our ~handful of bundles while keeping
/// URLs short.
fn version_query(path: &str, bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for b in &digest[..8] {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    format!("{path}?v={hex}")
}

static APP_CSS_URL: LazyLock<String> = LazyLock::new(|| version_query("/assets/app.css", APP_CSS));
static DATASTAR_JS_URL: LazyLock<String> =
    LazyLock::new(|| version_query("/assets/datastar.js", DATASTAR_JS));
static APP_JS_URL: LazyLock<String> = LazyLock::new(|| version_query("/assets/app.js", APP_JS));
static PCM_RECORDER_JS_URL: LazyLock<String> =
    LazyLock::new(|| version_query("/assets/pcm-recorder.js", PCM_RECORDER_JS));

/// Versioned URL for each baked asset. Page handlers call these to
/// emit `<link href=...>` / `<script src=...>` so the browser cache
/// busts automatically when the underlying bytes change.
pub fn app_css_url() -> &'static str {
    APP_CSS_URL.as_str()
}
pub fn datastar_js_url() -> &'static str {
    DATASTAR_JS_URL.as_str()
}
pub fn app_js_url() -> &'static str {
    APP_JS_URL.as_str()
}
pub fn pcm_recorder_js_url() -> &'static str {
    PCM_RECORDER_JS_URL.as_str()
}

pub async fn app_css() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, IMMUTABLE_CACHE),
        ],
        APP_CSS,
    )
        .into_response()
}

pub async fn datastar_js() -> Response {
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, IMMUTABLE_CACHE),
        ],
        DATASTAR_JS,
    )
        .into_response()
}

pub async fn app_js() -> Response {
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, IMMUTABLE_CACHE),
        ],
        APP_JS,
    )
        .into_response()
}

pub async fn pcm_recorder_js() -> Response {
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, IMMUTABLE_CACHE),
        ],
        PCM_RECORDER_JS,
    )
        .into_response()
}
