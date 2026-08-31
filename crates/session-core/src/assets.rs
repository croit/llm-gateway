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
//! Content-hashed bundles (CSS, JS) carry a `?v=<8-byte-sha256-prefix>`
//! of the bundle bytes. The hash is computed once at startup via
//! `LazyLock`. With the query string acting as a per-content cache key,
//! we serve those as `Cache-Control: public, max-age=31536000,
//! immutable` — the browser keeps them indefinitely and only re-fetches
//! when the template-emitted URL changes after a deploy.
//!
//! PWA assets (manifest, service worker) are **not** content-hashed and
//! must **not** use `immutable` — otherwise manifest/SW updates never
//! roll out. They get a short `max-age` instead.

use std::sync::LazyLock;

use rama::http::service::web::response::IntoResponse;
use rama::http::{HeaderName, Request, Response, StatusCode, header};
use sha2::{Digest, Sha256};

const APP_CSS: &[u8] = include_bytes!("../assets/app.css");
const DATASTAR_JS: &[u8] = include_bytes!("../assets/datastar.js");
const APP_JS: &[u8] = include_bytes!("../assets/app.js");
const PCM_RECORDER_JS: &[u8] = include_bytes!("../assets/pcm-recorder.js");

const MANIFEST_WEBMANIFEST: &[u8] = include_bytes!("../assets/manifest.webmanifest");
const SW_JS: &[u8] = include_bytes!("../assets/sw.js");
const FAVICON_ICO: &[u8] = include_bytes!("../assets/favicon.ico");

const ICON_192: &[u8] = include_bytes!("../assets/icons/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../assets/icons/icon-512.png");
const ICON_MASKABLE_512: &[u8] = include_bytes!("../assets/icons/icon-maskable-512.png");
const APPLE_TOUCH_ICON: &[u8] = include_bytes!("../assets/icons/apple-touch-icon.png");

/// Long-lived caching tag for content-hashed asset URLs. `immutable`
/// is what tells modern browsers to skip the revalidation round-trip
/// entirely — without it they still issue a conditional GET on every
/// reload despite the year-long max-age.
const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";

/// Short cache for PWA assets that must stay updateable — the manifest,
/// service worker, favicon, and icons. None of these are content-hashed
/// (no `?v=<hash>` URL), so they must NOT be `immutable`, or a rebrand /
/// manifest change served at the same URL would be pinned in browsers
/// for a year. A 5-minute max-age means the browser checks for updates
/// on a reasonable cadence without hammering the server.
const SHORT_CACHE: &str = "public, max-age=300";

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

/// Absolute URL for the apple-touch-icon, emitted in `<head>`.
pub fn apple_touch_icon_url() -> &'static str {
    "/icons/apple-touch-icon.png"
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

// ---- PWA assets ---------------------------------------------------------
//
// The web app manifest, service worker, favicon, and icons. Unlike the
// hashed bundles above, the manifest and SW are served with a short
// max-age (never `immutable`) so updates reach clients without a cache
// purge. The SW additionally gets `Service-Worker-Allowed: /` so its
// root-level scope is explicit.

/// `GET /manifest.webmanifest` — the web app manifest for PWA
/// installability.
pub async fn manifest_webmanifest() -> Response {
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/manifest+json; charset=utf-8",
            ),
            (header::CACHE_CONTROL, SHORT_CACHE),
        ],
        MANIFEST_WEBMANIFEST,
    )
        .into_response()
}

/// `GET /sw.js` — the service worker script. Served at root scope so
/// it controls the entire origin. `Service-Worker-Allowed` makes the
/// root scope explicit to the browser.
pub async fn sw_js() -> Response {
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, SHORT_CACHE),
            (HeaderName::from_static("service-worker-allowed"), "/"),
        ],
        SW_JS,
    )
        .into_response()
}

/// `GET /favicon.ico` — the multi-resolution ICO baked into the binary.
pub async fn favicon() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/x-icon"),
            (header::CACHE_CONTROL, SHORT_CACHE),
        ],
        FAVICON_ICO,
    )
        .into_response()
}

/// `GET /icons/{*name}` — serves a named PWA icon from the baked set.
/// Unknown names get a 404. Reads the icon name from the raw URI (not
/// a `Path` extractor) because rama's router lowercases matched path
/// segments — harmless here (all icon names are already lowercase) but
/// consistent with the `retrieve_model` pattern.
///
/// Served with `SHORT_CACHE`, not `IMMUTABLE_CACHE`: the icon URLs are
/// stable and not content-hashed, so `immutable` would pin a stale
/// logo for a year after a rebrand that reuses the same filenames.
pub async fn icon(req: Request) -> Response {
    let name = req.uri().path().strip_prefix("/icons/").unwrap_or_default();
    let bytes = match name {
        "icon-192.png" => Some(ICON_192),
        "icon-512.png" => Some(ICON_512),
        "icon-maskable-512.png" => Some(ICON_MASKABLE_512),
        "apple-touch-icon.png" => Some(APPLE_TOUCH_ICON),
        _ => None,
    };
    match bytes {
        Some(data) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, SHORT_CACHE),
            ],
            data,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "unknown icon").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::APP_CSS;

    /// The declaration block of the first rule whose selector list mentions
    /// `sel` (the compiled bundle merges identical blocks into one selector
    /// list, so an exact-selector match would miss them).
    fn rule_for(sel: &str) -> String {
        let css = std::str::from_utf8(APP_CSS).expect("bundle is utf-8");
        let mut from = 0;
        while let Some(hit) = css[from..].find(sel) {
            let at = from + hit;
            // Selector list = back to the previous `}` / `{`.
            let start = css[..at].rfind(['}', '{']).map_or(0, |i| i + 1);
            let open = match css[at..].find('{') {
                Some(i) => at + i,
                None => break,
            };
            let close = match css[open..].find('}') {
                Some(i) => open + i,
                None => break,
            };
            // Only a real selector-list hit, not a substring of a longer class
            // (`.document-canvas` inside `.document-canvas__body`).
            let selectors = &css[start..open];
            if selectors
                .split(',')
                .any(|s| s.trim().trim_end_matches("::-webkit-scrollbar") == sel)
            {
                return css[open + 1..close].to_string();
            }
            from = open;
        }
        panic!("no rule for `{sel}` in the compiled CSS bundle");
    }

    /// The document canvas scrolls only if EVERY link from the column down to
    /// the scroll body has a bounded height. The bug this pins: the panel was
    /// `height: 100%` inside an auto-height slot, so the percentage never
    /// resolved, `.document-canvas__body` grew to the full length of the
    /// document, and `.canvas-col`'s `overflow: hidden` clipped everything
    /// past the fold — a long report simply could not be scrolled.
    ///
    /// A flex item defaults to `min-height: auto` (refuses to shrink below its
    /// content), so each link needs an explicit `min-height: 0` as well.
    #[test]
    fn the_canvas_scroll_chain_is_height_bounded() {
        let col = rule_for(".canvas-col");
        assert!(
            col.contains("display:flex") && col.contains("flex-direction:column"),
            "the canvas column must be a flex column so the tab bodies below \
             the tab strip get a bounded height; got: {col}"
        );
        assert!(col.contains("min-height:0"), "{col}");

        for sel in [".canvas-slot", ".document-canvas", ".document-canvas__body"] {
            let rule = rule_for(sel);
            assert!(
                rule.contains("min-height:0"),
                "`{sel}` is a flex item in the canvas scroll chain and must \
                 declare min-height:0, or it refuses to shrink below its \
                 content and the panel body never becomes scrollable; got: {rule}"
            );
            assert!(
                !rule.contains("height:100%"),
                "`{sel}` must not size itself off a percentage — its ancestors \
                 are auto-height until flex resolves them, so `height: 100%` \
                 silently computes to `auto`; got: {rule}"
            );
        }

        let body = rule_for(".document-canvas__body");
        assert!(
            body.contains("overflow:auto"),
            "the panel body is the scroll container; got: {body}"
        );
    }
}
