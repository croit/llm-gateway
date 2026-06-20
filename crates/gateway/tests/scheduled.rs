// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Route-level wiring for the scheduled-actions page: the create / preview
//! / toggle / delete endpoints behind the session gate, and the
//! directive↔endpoint contract the builder UI relies on (the schedule
//! builder posts to `/scheduled/preview` and renders the server's summary).
//!
//! The cron evaluator and the data-layer CRUD are unit-tested in
//! `server::scheduled`; this file pins the HTTP surface.

mod common;

use common::Service as _;
use gateway::server::scheduled;
use rama::http::{Body, Method, Request, StatusCode};

/// Build a urlencoded, cookie-authed form request (what datastar's
/// `@post(url, {contentType:'form'})` sends).
fn form_req(method: Method, uri: &str, cookie: &str, body: &str) -> Request {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get_with_cookie(uri: &str, cookie: &str) -> Request {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .body(Body::empty())
        .unwrap()
}

const DAILY_FORM: &str = "name=Daily+digest&prompt=Summarize+the+news&model=qwen&timezone=Europe%2FBerlin\
     &mode=daily&hour=9&minute=0&tools=on";

#[tokio::test]
async fn index_anonymous_redirects_to_login() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/scheduled"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(loc.starts_with("/login"), "redirect target was {loc}");
}

#[tokio::test]
async fn create_then_list_then_toggle_then_delete() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let pool = state.db.clone();
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);

    // Create.
    let resp = app
        .serve(form_req(Method::POST, "/scheduled", &cookie, DAILY_FORM))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("Daily digest"),
        "row html missing name: {body}"
    );

    // The row exists in the DB, scoped to the owner, with a computed fire time.
    let actions = scheduled::list_for_user(&pool, "alice").await.unwrap();
    assert_eq!(actions.len(), 1);
    let action = &actions[0];
    assert_eq!(action.cron, "0 9 * * *");
    assert!(action.enabled);
    assert!(action.tools_enabled);
    assert!(action.next_run_at.is_some());

    // The list page renders it.
    let resp = app
        .serve(get_with_cookie("/scheduled", &cookie))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("Daily digest"));
    assert!(body.contains("At 09:00, every day."));

    // Toggle → paused (next_run_at cleared).
    let resp = app
        .serve(form_req(
            Method::POST,
            &format!("/scheduled/{}/toggle", action.id),
            &cookie,
            "",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let paused = scheduled::get(&pool, "alice", &action.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!paused.enabled);
    assert!(paused.next_run_at.is_none());

    // Delete.
    let resp = app
        .serve(form_req(
            Method::POST,
            &format!("/scheduled/{}/delete", action.id),
            &cookie,
            "",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        scheduled::list_for_user(&pool, "alice")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn preview_endpoint_returns_summary_and_next_runs() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "bob", "bob@example.com").await;
    let app = common::app(state);

    let resp = app
        .serve(form_req(
            Method::POST,
            "/scheduled/preview",
            &cookie,
            "mode=daily&hour=9&minute=0&timezone=Europe%2FBerlin",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    // The patch targets the preview element and carries the human summary.
    assert!(
        body.contains("#schedule-preview"),
        "not a preview patch: {body}"
    );
    assert!(body.contains("At 09:00, every day."));
    assert!(body.contains("Next runs:"));
}

#[tokio::test]
async fn preview_endpoint_surfaces_validation_errors() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "carol", "carol@example.com").await;
    let app = common::app(state);

    // Weekly with no weekday selected is the classic builder mistake.
    let resp = app
        .serve(form_req(
            Method::POST,
            "/scheduled/preview",
            &cookie,
            "mode=weekly&hour=8&minute=30&timezone=Europe%2FBerlin",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("Pick at least one weekday."), "body: {body}");
}

#[tokio::test]
async fn actions_are_scoped_per_user_over_http() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let pool = state.db.clone();
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    let app = common::app(state);

    // Alice creates one.
    app.serve(form_req(Method::POST, "/scheduled", &alice, DAILY_FORM))
        .await
        .unwrap();
    let action_id = scheduled::list_for_user(&pool, "alice").await.unwrap()[0]
        .id
        .clone();

    // Bob's list doesn't include it, and Bob can't delete it.
    let resp = app
        .serve(get_with_cookie("/scheduled", &bob))
        .await
        .unwrap();
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(!body.contains("Daily digest"));

    app.serve(form_req(
        Method::POST,
        &format!("/scheduled/{action_id}/delete"),
        &bob,
        "",
    ))
    .await
    .unwrap();
    // Still there — Bob's delete was scoped out.
    assert_eq!(
        scheduled::list_for_user(&pool, "alice")
            .await
            .unwrap()
            .len(),
        1
    );
}

/// Every mode panel stays in the DOM (toggled with `data-show`), so the
/// whole form is serialized on each preview/submit. If two panels each
/// rendered an input with the same `name`, the posted body would carry a
/// duplicate field and `serde_urlencoded` would reject it ("malformed
/// form: duplicate field"). Pin that the builder renders exactly one of
/// each schedule field.
#[tokio::test]
async fn builder_has_no_duplicate_field_names() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "dave", "dave@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(get_with_cookie("/scheduled", &cookie))
        .await
        .unwrap();
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    for field in ["name=\"hour\"", "name=\"minute\"", "name=\"dom\""] {
        let count = body.matches(field).count();
        assert_eq!(count, 1, "expected exactly one `{field}`, found {count}");
    }
}

/// The whole form (every panel's fields at once, as the browser posts it)
/// must create cleanly — a regression guard for the duplicate-field bug
/// above, exercised through the real create handler rather than the DOM.
#[tokio::test]
async fn create_accepts_full_form_with_all_panel_fields() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let pool = state.db.clone();
    let cookie = common::seed_session(&state, "erin", "erin@example.com").await;
    let app = common::app(state);
    // Mirrors a real submit: name/prompt/model + a single hour/minute/dom +
    // weekday + advanced, with mode=weekly selected.
    let full = "name=Weekly+report&prompt=Draft+it&model=qwen&timezone=Europe%2FBerlin\
                &mode=weekly&hour=8&minute=30&dom=1&advanced=0+9+*+*+*&wd1=on&wd3=on&tools=on";
    let resp = app
        .serve(form_req(Method::POST, "/scheduled", &cookie, full))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(!body.to_lowercase().contains("malformed"), "got: {body}");
    let actions = scheduled::list_for_user(&pool, "erin").await.unwrap();
    assert_eq!(actions.len(), 1);
    // mode=weekly with Mon+Wed at 08:30.
    assert_eq!(actions[0].cron, "30 8 * * 1,3");
}
