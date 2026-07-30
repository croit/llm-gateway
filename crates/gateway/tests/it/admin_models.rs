// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/models` — admin-only model defaults UI + save endpoint.
//!
//! What this pins:
//!   - GET requires the `admin` role (anon → /login redirect,
//!     logged-in-but-not-admin → 403).
//!   - The consolidated `POST /admin/models/save` is gated the same way.
//!   - A valid TOML save round-trips into the DB so subsequent
//!     /v1/chat/completions calls see the merged defaults.
//!   - `POST /admin/models/clear` deletes the stored overrides row.

use crate::common;

use common::Service as _;
use gateway_core::server::db::{model_defaults as db_defaults, users};
use gateway_features::server::search_settings;
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
            speech_voice: None,
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
    assert!(body.contains("Models"), "page heading missing");
    // The seeded pool advertises `model-a`, so we expect a row.
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
            "/admin/models/save",
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
    let post_uri = "/admin/models/save";
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
            "/admin/models/save",
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
            "/admin/models/save",
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
async fn admin_clear_deletes_row() {
    // "Clear all overrides" (`POST /admin/models/clear`) removes the stored
    // row entirely — the model returns to the backend's built-in behaviour.
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
            "/admin/models/clear",
            &cookie,
            Some("model_name=model-a"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(db_defaults::get(&db, "model-a").await.unwrap().is_none());
}

#[tokio::test]
async fn admin_save_empty_toml_keeps_row_with_prices() {
    // Unlike "Clear", saving with an empty TOML but a price set persists a
    // row (empty sampling defaults, priced) — the consolidated save writes all
    // facets, it doesn't treat a blank textarea as "delete".
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let db = state.db.clone();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/models/save",
            &cookie,
            Some("model_name=model-a&defaults_toml=&input_price=1.5"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let row = db_defaults::get(&db, "model-a")
        .await
        .unwrap()
        .expect("row persisted");
    assert_eq!(row.input_price, Some(1.5));
    assert_eq!(row.defaults_toml, "");
}

#[tokio::test]
async fn price_only_save_preserves_other_model_settings() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    db_defaults::set_all(
        &state.db,
        "model-a",
        &db_defaults::AllFields {
            defaults_toml: "temperature = 0.7".into(),
            context_window: Some(32_768),
            capabilities: db_defaults::ModelCapabilities {
                vision: Some(true),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let db = state.db.clone();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/models/save",
            &cookie,
            Some("model_name=model-a&input_price=2&pricing_unit=images&price_only=1"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let row = db_defaults::get(&db, "model-a").await.unwrap().unwrap();
    assert_eq!(row.input_price, Some(2.0));
    assert_eq!(row.pricing_unit, db_defaults::PricingUnit::Images);
    assert_eq!(row.context_window, Some(32_768));
    assert_eq!(row.capabilities.vision, Some(true));
    assert!(row.defaults_toml.contains("temperature"));
}

// ---------------------------------------------------------------------------
// Web-search settings (`POST /admin/models/search`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_admin_search_save_is_403_and_writes_nothing() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "bob", "bob@example.com").await;
    let db = state.db.clone();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/models/search",
            &cookie,
            Some("provider=brave&brave_api_key=leak-me"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        gateway_core::server::db::app_settings::get(&db, search_settings::BRAVE_KEY_KEY)
            .await
            .unwrap()
            .is_none(),
        "a non-admin must not be able to write the search key"
    );
}

#[tokio::test]
async fn admin_saves_provider_and_url() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let db = state.db.clone();
    let crypto = state.crypto.clone();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/models/search",
            &cookie,
            Some("provider=searxng&searxng_url=https%3A%2F%2Fsearx.example.com%2F"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let s = search_settings::load(&db, &crypto).await.unwrap();
    assert_eq!(s.provider, search_settings::SearchProvider::Searxng);
    assert_eq!(s.searxng_url.as_deref(), Some("https://searx.example.com"));
}

#[tokio::test]
async fn admin_rejects_an_unknown_provider() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let db = state.db.clone();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/models/search",
            &cookie,
            Some("provider=altavista"),
        ))
        .await
        .unwrap();
    // Handled as a flash, not a hard error — but nothing may be persisted.
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        gateway_core::server::db::app_settings::get(&db, search_settings::PROVIDER_KEY)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn brave_key_is_stored_sealed_and_a_blank_resave_keeps_it() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let db = state.db.clone();
    let crypto = state.crypto.clone();
    let app = common::app(state);

    // First save writes the key.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/models/search",
            &cookie,
            Some("provider=brave&brave_api_key=bsk-secret"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Sealed at rest: the row must not contain the plaintext.
    let raw = gateway_core::server::db::app_settings::get(&db, search_settings::BRAVE_KEY_KEY)
        .await
        .unwrap()
        .expect("key row");
    assert!(!raw.contains("bsk-secret"), "stored in plaintext: {raw}");
    assert_eq!(
        search_settings::load(&db, &crypto)
            .await
            .unwrap()
            .brave_api_key
            .as_deref(),
        Some("bsk-secret")
    );

    // Re-saving the form with a blank key field must NOT wipe it — the
    // operator can't read the value back, so a blank means "unchanged".
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/models/search",
            &cookie,
            Some("provider=brave&searxng_url=&brave_api_key="),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        search_settings::load(&db, &crypto)
            .await
            .unwrap()
            .brave_api_key
            .as_deref(),
        Some("bsk-secret"),
        "a blank key field must keep the stored key"
    );
}

#[tokio::test]
async fn clear_checkbox_removes_the_brave_key() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let db = state.db.clone();
    let crypto = state.crypto.clone();
    search_settings::set_brave_key(&db, &crypto, "bsk-secret")
        .await
        .unwrap();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/models/search",
            &cookie,
            Some("provider=brave&brave_api_key=&clear_brave_key=1"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        search_settings::load(&db, &crypto)
            .await
            .unwrap()
            .brave_api_key
            .is_none()
    );
}

#[tokio::test]
async fn rendered_page_shows_the_search_card_without_the_key() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    search_settings::set_provider(&state.db, search_settings::SearchProvider::Brave)
        .await
        .unwrap();
    search_settings::set_brave_key(&state.db, &state.crypto, "bsk-secret")
        .await
        .unwrap();
    search_settings::set_searxng_url(&state.db, "https://searx.example.com")
        .await
        .unwrap();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/admin/models", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("Web search"), "search card missing");
    // Initial-load state, not just the POST path: the stored provider and URL
    // must come back pre-selected on a plain page load.
    assert!(
        body.contains(r#"value="brave" selected="selected""#),
        "stored provider not preselected"
    );
    assert!(body.contains("https://searx.example.com"), "url not shown");
    // The secret itself must never reach the page.
    assert!(!body.contains("bsk-secret"), "the key leaked into the HTML");
}
