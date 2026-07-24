// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-user `/skills` page: a signed-in user creates a private skill via the
//! inline editor, sees it listed, and — crucially — a *different* user never
//! does. End-to-end through the real router (auth gate, form POST, re-render),
//! the wiring the unit tests can't cover.

use crate::common;

use common::Service as _;
use rama::http::{Body, Method, Request, StatusCode};

const SKILL_MD: &str = "---\nname: mine\ntitle: My Test Skill\ndescription: A private test skill.\n---\n\nDo the private thing.\n";

/// Build a `POST /skills/save` request carrying the session cookie and an
/// urlencoded `name`/`content` form.
fn save_req(cookie: &str, name: &str, content: &str) -> Request {
    let body = serde_urlencoded::to_string([("name", name), ("content", content)]).unwrap();
    Request::builder()
        .method(Method::POST)
        .uri("/skills/save")
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
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

#[tokio::test]
async fn unauthenticated_skills_page_redirects_to_login() {
    let dir = tempfile::tempdir().unwrap();
    let state = common::state_with_user_skills(dir.path().to_path_buf()).await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/skills"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(loc.starts_with("/login"), "redirect target: {loc}");
}

#[tokio::test]
async fn save_via_inline_editor_then_list_shows_the_skill() {
    let dir = tempfile::tempdir().unwrap();
    let state = common::state_with_user_skills(dir.path().to_path_buf()).await;
    let cookie = common::seed_session(&state, "u1", "u1@example.com").await;
    let app = common::app(state);

    // Create a private skill (empty name → slug derived from the frontmatter).
    let resp = app.serve(save_req(&cookie, "", SKILL_MD)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        loc, "/skills?skill=mine",
        "should redirect to the new skill"
    );

    // The list + detail now show it (by its human title).
    let resp = app
        .serve(get_with_cookie("/skills", &cookie))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("My Test Skill"),
        "skill title should be listed"
    );
    assert!(
        body.contains("Do the private thing."),
        "selected skill's rendered body should show"
    );
}

#[tokio::test]
async fn skills_nav_shows_when_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let state = common::state_with_user_skills(dir.path().to_path_buf()).await;
    let cookie = common::seed_session(&state, "u1", "u1@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(get_with_cookie("/tokens", &cookie))
        .await
        .unwrap();
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("href=\"/skills\""),
        "the /skills nav entry should show when private skills are enabled"
    );
}

#[tokio::test]
async fn skills_nav_hidden_when_not_configured() {
    let state = common::state_no_skills().await;
    let cookie = common::seed_session(&state, "u1", "u1@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(get_with_cookie("/tokens", &cookie))
        .await
        .unwrap();
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        !body.contains("href=\"/skills\""),
        "the /skills nav entry must be hidden when skills aren't configured"
    );
}

#[tokio::test]
async fn skills_nav_hidden_when_dir_inaccessible() {
    // Point the skills root at a *file* — its `.users` subdir can't exist, so
    // the directory is inaccessible and the nav entry hides itself.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, "x").unwrap();
    let state = common::state_with_user_skills(file).await;
    let cookie = common::seed_session(&state, "u1", "u1@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(get_with_cookie("/tokens", &cookie))
        .await
        .unwrap();
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        !body.contains("href=\"/skills\""),
        "the /skills nav entry must be hidden when the directory is inaccessible"
    );
}

#[tokio::test]
async fn a_private_skill_is_invisible_to_another_user() {
    let dir = tempfile::tempdir().unwrap();
    let state = common::state_with_user_skills(dir.path().to_path_buf()).await;
    let alice = common::seed_session(&state, "alice", "alice@example.com").await;
    let bob = common::seed_session(&state, "bob", "bob@example.com").await;
    let app = common::app(state);

    // Alice creates a private skill.
    let resp = app.serve(save_req(&alice, "", SKILL_MD)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    // Alice sees it…
    let resp = app.serve(get_with_cookie("/skills", &alice)).await.unwrap();
    let alice_body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(alice_body.contains("My Test Skill"));

    // …Bob does not.
    let resp = app.serve(get_with_cookie("/skills", &bob)).await.unwrap();
    let bob_body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        !bob_body.contains("My Test Skill"),
        "another user's private skill must not appear"
    );
}
