// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Collapsible sidebar nav-groups (Workspace / Account / Admin).
//!
//! What this pins:
//!   - `POST /nav/toggle/{section}` flips the `nav_sections` cookie and
//!     returns an SSE patch that sets `<html data-nav-{section}>` in place
//!     (the same server-cookie pattern as `/theme/toggle`). An unknown
//!     section is a no-op (no cookie, no script).
//!   - The authed layout renders the three groups with the default fold
//!     state on `<html>` (Workspace open, Account + Admin collapsed) and a
//!     toggle directive per group header.
//!   - A non-admin never gets the Admin *group* (its `<html>` attribute is
//!     still present, but the group markup is gated on the admin role).
//!   - The initial full-page render honours an existing `nav_sections`
//!     cookie — not just the toggle event — so the fold state survives a
//!     reload.

mod common;

use common::Service as _;
use gateway::server::db::users;
use jiff::Timestamp;
use rama::http::{Body, Method, Request, StatusCode};

fn set_cookie_values(resp: &rama::http::Response) -> Vec<String> {
    resp.headers()
        .get_all(rama::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(String::from))
        .collect()
}

/// POST /nav/toggle/{section} with an optional pre-existing `nav_sections`
/// cookie value. No session needed — the toggle is an unauthenticated UI
/// affordance, exactly like `/theme/toggle`.
fn toggle_req(section: &str, nav_cookie: Option<&str>) -> Request {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri(format!("/nav/toggle/{section}"));
    if let Some(c) = nav_cookie {
        b = b.header("cookie", format!("nav_sections={c}"));
    }
    b.body(Body::empty()).unwrap()
}

/// Authed full-page GET with the session cookie + an optional extra
/// `nav_sections` cookie. No `datastar-request` header → full HTML page.
fn page_req(uri: &str, session: &str, nav_cookie: Option<&str>) -> Request {
    let cookie = match nav_cookie {
        Some(c) => format!("id={session}; nav_sections={c}"),
        None => format!("id={session}"),
    };
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap()
}

async fn make_admin(state: &gateway::rama_server::RamaState, user_id: &str) {
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
}

#[tokio::test]
async fn nav_toggle_flips_cookie_and_patches_html() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let app = common::app(state);

    // No cookie → defaults (workspace open, account closed). Toggling
    // `account` opens it; the persisted open-set becomes {workspace, account}.
    let resp = app.serve(toggle_req("account", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = set_cookie_values(&resp)
        .into_iter()
        .find(|c| c.starts_with("nav_sections="))
        .expect("nav_sections cookie set");
    assert!(
        cookie.starts_with("nav_sections=workspace,account;"),
        "expected open-set workspace,account, got `{cookie}`"
    );
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("event: datastar-patch-elements"),
        "expected an SSE patch, got:\n{body}"
    );
    assert!(
        body.contains("setAttribute('data-nav-account', 'open')"),
        "expected the inline script to open the account group, got:\n{body}"
    );

    // Starting from {workspace, account} open, toggling `workspace` closes
    // it → open-set is just {account}, and the script sets it closed.
    let resp = app
        .serve(toggle_req("workspace", Some("workspace,account")))
        .await
        .unwrap();
    let cookie = set_cookie_values(&resp)
        .into_iter()
        .find(|c| c.starts_with("nav_sections="))
        .expect("nav_sections cookie set");
    assert!(
        cookie.starts_with("nav_sections=account;"),
        "expected open-set account, got `{cookie}`"
    );
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("setAttribute('data-nav-workspace', 'closed')"),
        "expected the inline script to close the workspace group, got:\n{body}"
    );
}

#[tokio::test]
async fn nav_toggle_unknown_section_is_a_noop() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let app = common::app(state);

    let resp = app.serve(toggle_req("bogus", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        set_cookie_values(&resp).is_empty(),
        "an unknown section must not set a cookie"
    );
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        !body.contains("setAttribute"),
        "an unknown section must not emit a script, got:\n{body}"
    );
}

#[tokio::test]
async fn sidebar_renders_groups_with_default_fold_state() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    // Plain user (no admin role) — the Admin *group* must not render.
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);

    let resp = app.serve(page_req("/tokens", &cookie, None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();

    // Default fold state lives on <html>: workspace open, the rest closed.
    assert!(
        body.contains("data-nav-workspace=\"open\""),
        "workspace should default open"
    );
    assert!(
        body.contains("data-nav-account=\"closed\""),
        "account should default closed"
    );
    assert!(
        body.contains("data-nav-admin=\"closed\""),
        "admin should default closed"
    );

    // The two user-facing groups render, each with a toggle directive.
    assert!(body.contains("data-group=\"workspace\""));
    assert!(body.contains("data-group=\"account\""));
    assert!(body.contains("/nav/toggle/workspace"));
    assert!(body.contains("/nav/toggle/account"));

    // A non-admin never sees the Admin group markup.
    assert!(
        !body.contains("data-group=\"admin\""),
        "non-admin must not get the Admin group"
    );
    assert!(!body.contains("/nav/toggle/admin"));
}

#[tokio::test]
async fn sidebar_admin_group_renders_for_admins() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "root", "root@example.com").await;
    make_admin(&state, "root").await;
    let app = common::app(state);

    let resp = app.serve(page_req("/tokens", &cookie, None)).await.unwrap();
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();

    assert!(
        body.contains("data-group=\"admin\""),
        "admin must get the Admin group"
    );
    assert!(body.contains("/nav/toggle/admin"));
    // Skills is an admin-only entry and lives in the Admin group.
    assert!(body.contains("Skills"));
}

#[tokio::test]
async fn initial_render_honours_existing_nav_cookie() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);

    // A returning user whose cookie has only `account` open: the full-page
    // render must reflect it (account open, workspace closed) — the state
    // survives a reload, it isn't only applied on the toggle event.
    let resp = app
        .serve(page_req("/tokens", &cookie, Some("account")))
        .await
        .unwrap();
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("data-nav-account=\"open\""));
    assert!(body.contains("data-nav-workspace=\"closed\""));
    assert!(body.contains("data-nav-admin=\"closed\""));
}
