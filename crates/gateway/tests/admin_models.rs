// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/models` — admin-only model defaults UI + save endpoint.
//!
//! What this pins:
//!   - GET requires the `admin` role (anon → /login redirect,
//!     logged-in-but-not-admin → 403).
//!   - POST is gated the same way.
//!   - A valid TOML save round-trips into the DB so subsequent
//!     /v1/chat/completions calls see the merged defaults.

mod common;

use common::Service as _;
use gateway::server::db::{model_defaults as db_defaults, users};
use jiff::Timestamp;
use rama::http::{Body, Method, Request, StatusCode};

fn req_with_cookie(method: Method, uri: &str, cookie: &str, body: Option<&str>) -> Request {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("id={cookie}"));
    if body.is_some() {
        b = b.header("content-type", "application/x-www-form-urlencoded");
    }
    b.body(Body::from(body.unwrap_or("").to_string())).unwrap()
}

/// Seed a session + flip the user's `roles` to include `"admin"`.
/// Mirrors `common::seed_session` + an extra `users::upsert` so the
/// admin-gate test can drive an actually-admin user.
async fn seed_admin(state: &gateway::rama_server::RamaState, user_id: &str) -> String {
    let cookie = common::seed_session(state, user_id, &format!("{user_id}@example.com")).await;
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: user_id.into(),
            email: format!("{user_id}@example.com"),
            name: None,
            roles: vec!["admin".into()],
            created_at: now,
            updated_at: now,
            timezone: None,
        },
    )
    .await
    .unwrap();
    cookie
}

#[tokio::test]
async fn anon_get_redirects_to_login() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/admin/models"))
        .await
        .unwrap();
    // Anonymous → /login redirect (same shape as other authed pages),
    // preserving the requested page as ?return_to so the deep link survives.
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        location.starts_with("/login") && location.contains("return_to="),
        "anon must bounce to /login carrying return_to; got `{location}`"
    );
}

#[tokio::test]
async fn non_admin_get_is_403() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/admin/models", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_get_renders_page() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/admin/models", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("Model defaults"), "page heading missing");
    // The seeded pool advertises `model-a`, so we expect a card.
    assert!(body.contains("model-a"), "model-a not listed: {body}");
}

#[tokio::test]
async fn non_admin_save_is_403() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "bob", "bob@example.com").await;
    let db = state.db.clone();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/models",
            &cookie,
            Some("model_name=model-a&defaults_toml=temperature%20%3D%200.7"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // And nothing got written.
    assert!(db_defaults::get(&db, "model-a").await.unwrap().is_none());
}

#[tokio::test]
async fn rendered_page_shows_stored_toml_in_textarea() {
    // Seed an admin + a stored row, then GET the page and assert
    // the saved TOML actually lands inside the textarea body so the
    // operator can see / edit it on reload.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    db_defaults::upsert(&state.db, "model-a", "temperature = 0.7\ntop_p = 0.95")
        .await
        .unwrap();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/admin/models", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("temperature = 0.7"),
        "stored TOML not in textarea body: {body}"
    );
    // Pin it to the textarea content slot specifically — not just
    // anywhere on the page (e.g. accidentally inside the placeholder
    // string would also pass `body.contains`).
    let textarea_open = body
        .find("<textarea")
        .expect("no textarea tag in rendered page");
    let close = body[textarea_open..]
        .find("</textarea>")
        .expect("unclosed textarea");
    let tag = &body[textarea_open..textarea_open + close];
    let inner_start = tag.find('>').expect("malformed textarea open tag") + 1;
    let inner = &tag[inner_start..];
    assert!(
        inner.contains("temperature = 0.7"),
        "stored TOML missing from textarea inner content; got:\n{inner}"
    );
}

#[tokio::test]
async fn save_then_get_renders_saved_toml_for_slashed_model() {
    // End-to-end version of the reported "save says success but
    // reload is empty" case. Seeds a HuggingFace-style slashed
    // model name (the slash must survive URL encoding on the save
    // path *and* match the registry's listing key on the GET path),
    // posts a TOML body, fetches the page, asserts the textarea
    // has the saved content.
    let upstream = "http://unused.invalid";
    let mut state = common::state_with_admin_rbac(upstream).await;
    // Seed a model name that contains `/`. The default scaffold
    // advertises `model-a`; we replace it with the realistic one.
    common::seed_pool_models(&state.upstreams, "pool", 0, &["Qwen/Qwen3.6-27B-FP8"]);
    // (state_with_admin_rbac doesn't return Result for mut, just be
    // explicit we're mutating the upstream snapshot via the helper.)
    let _ = &mut state;
    let cookie = seed_admin(&state, "root").await;
    let db = state.db.clone();
    let app = common::app(state);

    // POST the form using the URL-encoded model name in the path.
    let post_uri = "/admin/models";
    let post_body =
        "model_name=Qwen%2FQwen3.6-27B-FP8&defaults_toml=temperature+%3D+0.7%0Atop_p+%3D+0.95";
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            post_uri,
            &cookie,
            Some(post_body),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let toast_body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    // DB lookup uses the decoded key.
    let stored = db_defaults::get(&db, "Qwen/Qwen3.6-27B-FP8")
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("row should exist after POST; toast was:\n{toast_body}"));
    assert!(stored.defaults_toml.contains("temperature = 0.7"));

    // Now GET the page and confirm the textarea carries the saved TOML.
    let resp = app
        .serve(req_with_cookie(Method::GET, "/admin/models", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    let textarea_open = body.find("<textarea").expect("no textarea on page");
    let close = body[textarea_open..].find("</textarea>").unwrap();
    let inner_start = body[textarea_open..textarea_open + close]
        .find('>')
        .unwrap()
        + 1;
    let inner = &body[textarea_open + inner_start..textarea_open + close];
    assert!(
        inner.contains("temperature = 0.7"),
        "saved TOML missing from textarea after GET; inner was:\n{inner}"
    );
}

#[tokio::test]
async fn admin_save_round_trips_to_db() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let db = state.db.clone();
    let app = common::app(state);
    let body = "model_name=model-a&defaults_toml=temperature+%3D+0.7%0Atop_p+%3D+0.95";
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/models",
            &cookie,
            Some(body),
        ))
        .await
        .unwrap();
    // Save returns an SSE toast — 200 with text/event-stream.
    assert_eq!(resp.status(), StatusCode::OK);
    let row = db_defaults::get(&db, "model-a")
        .await
        .unwrap()
        .expect("row written");
    assert!(row.defaults_toml.contains("temperature"));
    assert!(row.defaults_toml.contains("top_p"));
}

#[tokio::test]
async fn admin_save_with_broken_toml_doesnt_persist() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let db = state.db.clone();
    let app = common::app(state);
    // Valid-syntax TOML but nested table is rejected by the merge
    // helper at save time (sampling params must be flat).
    let body = "model_name=model-a&defaults_toml=%5Bsampling%5D%0Atemperature+%3D+0.7";
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/models",
            &cookie,
            Some(body),
        ))
        .await
        .unwrap();
    // Toast response, but nothing got persisted.
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(db_defaults::get(&db, "model-a").await.unwrap().is_none());
}

#[tokio::test]
async fn admin_save_empty_toml_clears_row() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    db_defaults::upsert(&state.db, "model-a", "temperature = 0.7")
        .await
        .unwrap();
    let db = state.db.clone();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/models",
            &cookie,
            Some("model_name=model-a&defaults_toml="),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(db_defaults::get(&db, "model-a").await.unwrap().is_none());
}
