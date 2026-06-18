// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Drift guard for the README's **HTTP endpoints** table.
//!
//! The table in `README.md` is hand-written (a curated, grouped summary —
//! not a 1:1 dump), and rama's `Router` exposes no way to enumerate its
//! registered routes at runtime. So instead of generating the table, this
//! test pins the contract between the two sources of truth:
//!
//!   1. the real routes declared in `rama_server/router.rs`
//!      (`.with_get("…")`, `.with_post("…")`, …), and
//!   2. the paths documented in the README endpoint table.
//!
//! It fails CI when they drift, in either direction:
//!   - a route exists in `router.rs` but nothing in the README covers it
//!     (a new endpoint was added without documenting it), or
//!   - the README documents a path that no route serves (a stale/typo'd
//!     entry left behind after a rename/removal).
//!
//! Routes that are intentionally absent from the public table (static
//! assets, the OIDC/CLI auth dance, the theme toggle) are listed in
//! `UNDOCUMENTED` below — so adding a new internal route still forces a
//! conscious choice: document it, or allow-list it here.

use std::path::Path;

/// HTTP verbs the router registers (`with_get` → `GET`, …) and that the
/// README may prefix a path with (`GET /healthz`).
const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];

/// Path prefixes for routes deliberately not listed in the README's
/// HTTP endpoints table. Matched with the same coverage rule as the
/// documented patterns (a `/*` suffix is a prefix glob).
const UNDOCUMENTED: &[&str] = &[
    "/assets/*",     // static bundles baked in via include_bytes
    "/auth/*",       // OIDC browser flow + CLI device-flow handoff (covered in prose)
    "/theme/toggle", // UI affordance, not an API surface
];

/// `pat` covers concrete path `actual`. A trailing `/*` is a prefix glob;
/// a plain prefix also covers its sub-paths (so `/chat` covers
/// `/chat/{id}/messages`), except `/` which matches only itself.
fn path_covers(pat: &str, actual: &str) -> bool {
    if let Some(prefix) = pat.strip_suffix("/*") {
        return actual == prefix || actual.starts_with(&format!("{prefix}/"));
    }
    if pat == "/" {
        return actual == "/";
    }
    actual == pat || actual.starts_with(&format!("{pat}/"))
}

/// A documented `(method?, path)` covers an actual `(method, path)` route.
/// A README entry without a method (UI-nav rows, `/api/v0/*`) matches any
/// method.
fn doc_covers(doc: &(Option<String>, String), route: &(String, String)) -> bool {
    if let Some(m) = &doc.0
        && m != &route.0
    {
        return false;
    }
    path_covers(&doc.1, &route.1)
}

/// Extract every `(METHOD, path)` route registered in `router.rs` by
/// scanning for `.with_<verb>("<path>"` builder calls. The path is always
/// the first string literal after the opening paren (handles the
/// multi-line registrations too).
fn actual_routes(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for verb in METHODS {
        let needle = format!(".with_{}(", verb.to_lowercase());
        let mut from = 0;
        while let Some(pos) = src[from..].find(&needle) {
            let after = from + pos + needle.len();
            from = after;
            let Some(q1) = src[after..].find('"') else {
                continue;
            };
            let s = after + q1 + 1;
            let Some(q2) = src[s..].find('"') else {
                continue;
            };
            out.push((verb.to_string(), src[s..s + q2].to_string()));
        }
    }
    out
}

/// Parse the README's "HTTP endpoints" table into documented
/// `(method?, path)` entries — every backtick token in each row's first
/// column.
fn documented_routes(readme: &str) -> Vec<(Option<String>, String)> {
    let start = readme
        .find("### HTTP endpoints")
        .expect("README is missing the `### HTTP endpoints` section");
    let mut out = Vec::new();
    for line in readme[start..].lines().skip(1) {
        let t = line.trim();
        if t.starts_with("## ") || t.starts_with("### ") {
            break; // next section
        }
        if !t.starts_with('|') {
            continue;
        }
        // First column: text between the first and second pipe.
        let cell = t.trim_matches('|').split('|').next().unwrap_or("");
        if cell.chars().all(|c| matches!(c, '-' | ':' | ' ')) {
            continue; // header separator row
        }
        for (i, part) in cell.split('`').enumerate() {
            if i % 2 == 0 {
                continue; // outside backticks
            }
            let tok = part.trim();
            if tok.is_empty() {
                continue;
            }
            match tok.split_once(' ') {
                Some((m, p)) if METHODS.contains(&m) => {
                    out.push((Some(m.to_string()), p.trim().to_string()))
                }
                _ => out.push((None, tok.to_string())),
            }
        }
    }
    out
}

#[test]
fn readme_http_endpoints_match_router() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let router_src = std::fs::read_to_string(manifest.join("src/rama_server/router.rs"))
        .expect("read router.rs");
    let readme = std::fs::read_to_string(manifest.join("../../README.md")).expect("read README.md");

    let actual = actual_routes(&router_src);
    let docs = documented_routes(&readme);

    assert!(!actual.is_empty(), "parsed zero routes from router.rs");
    assert!(
        !docs.is_empty(),
        "parsed zero entries from the README table"
    );

    let mut errors = Vec::new();

    // 1. Every real route is either documented or explicitly allow-listed.
    for route in &actual {
        if UNDOCUMENTED.iter().any(|p| path_covers(p, &route.1)) {
            continue;
        }
        if !docs.iter().any(|d| doc_covers(d, route)) {
            errors.push(format!(
                "  route `{} {}` is not in the README HTTP endpoints table \
                 (document it, or add a prefix to UNDOCUMENTED)",
                route.0, route.1
            ));
        }
    }

    // 2. Every documented path is served by at least one real route.
    for doc in &docs {
        if !actual.iter().any(|r| doc_covers(doc, r)) {
            let m = doc.0.as_deref().unwrap_or("(any)");
            errors.push(format!(
                "  README documents `{} {}` but no such route exists in router.rs",
                m, doc.1
            ));
        }
    }

    assert!(
        errors.is_empty(),
        "README HTTP endpoints table is out of sync with router.rs:\n{}",
        errors.join("\n")
    );
}
