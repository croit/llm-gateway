// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/users` — the registered-user roster + admin impersonation.
//!
//! What this pins:
//!   - GET roster requires the `admin` role (anon → /login, non-admin → 403).
//!   - The roster lists registered users + offers an Impersonate control.
//!   - Starting an impersonation is admin-only, mints a session that *acts
//!     as* the target while remembering the admin, refuses self- and
//!     nested-impersonation, and writes an audit row.
//!   - Stopping restores the admin and writes an audit row; the
//!     impersonation banner shows on authed pages while impersonating and
//!     is absent for an ordinary session.

mod common;

use common::Service as _;
use gateway::server::db::{audit, users};
use jiff::Timestamp;
use rama::http::{Body, Method, Request, Response, StatusCode};

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
async fn seed_admin(state: &gateway::rama_server::RamaState, user_id: &str) -> String {
    let cookie = common::seed_session(state, user_id, &format!("{user_id}@example.com")).await;
    upsert_user(state, user_id, &["admin"]).await;
    cookie
}

/// Upsert a bare user row (no session) with the given roles.
async fn upsert_user(state: &gateway::rama_server::RamaState, user_id: &str, roles: &[&str]) {
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: user_id.into(),
            email: format!("{user_id}@example.com"),
            name: None,
            roles: roles.iter().map(|s| (*s).to_string()).collect(),
            created_at: now,
            updated_at: now,
            timezone: None,
        },
    )
    .await
    .unwrap();
}

/// Pull the `id=<value>` session cookie out of a response's Set-Cookie.
fn set_cookie_value(resp: &Response) -> Option<String> {
    let raw = resp.headers().get("set-cookie")?.to_str().ok()?;
    raw.strip_prefix("id=")
        .map(|rest| rest.split(';').next().unwrap_or("").to_string())
        .filter(|v| !v.is_empty())
}

async fn read_body_string(resp: Response) -> String {
    String::from_utf8(common::read_body(resp).await.to_vec()).unwrap()
}

// ---------------------------------------------------------------------------
// Roster gate + content

#[tokio::test]
async fn anon_get_redirects_to_login() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/admin/users"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        loc.starts_with("/login"),
        "anon must bounce to /login; got `{loc}`"
    );
}

#[tokio::test]
async fn non_admin_get_is_403() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/admin/users", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_get_lists_users_with_impersonate() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    upsert_user(&state, "victim", &[]).await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/admin/users", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body_string(resp).await;
    assert!(
        body.contains("victim@example.com"),
        "target user not listed: {body}"
    );
    assert!(body.contains("Impersonate"), "no impersonate control");
    // The admin's own row is marked, not impersonatable.
    assert!(body.contains("root@example.com"));
}

// ---------------------------------------------------------------------------
// Start impersonation

#[tokio::test]
async fn non_admin_impersonate_is_403() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "bob", "bob@example.com").await;
    upsert_user(&state, "victim", &[]).await;
    let db = state.db.clone();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/users/impersonate",
            &cookie,
            Some("user_id=victim"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // And nothing was recorded.
    assert!(audit::recent(&db, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn admin_impersonate_mints_acting_session_and_audits() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    upsert_user(&state, "victim", &[]).await;
    let sessions = state.sessions.clone();
    let db = state.db.clone();
    let app = common::app(state);

    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/users/impersonate",
            &cookie,
            Some("user_id=victim"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let signed = set_cookie_value(&resp).expect("impersonation should set a session cookie");

    // The new cookie resolves to a session acting as the target while
    // remembering the admin.
    let id = sessions.verify(&signed).expect("cookie HMAC valid");
    let sess = sessions
        .lookup(id)
        .await
        .unwrap()
        .expect("session row exists");
    assert_eq!(sess.user_id, "victim");
    assert_eq!(sess.impersonator_id.as_deref(), Some("root"));

    // A 'start' event is on the audit trail.
    let events = audit::recent(&db, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].action, "start");
    assert_eq!(events[0].actor_id, "root");
    assert_eq!(events[0].target_id, "victim");
}

#[tokio::test]
async fn admin_cannot_impersonate_self() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    let db = state.db.clone();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/users/impersonate",
            &cookie,
            Some("user_id=root"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        set_cookie_value(&resp).is_none(),
        "no session swap on self-impersonation"
    );
    assert!(audit::recent(&db, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn nested_impersonation_refused() {
    // root (admin) impersonates root2 (also admin). Acting as an admin,
    // the explicit guard must still refuse starting a second impersonation.
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    upsert_user(&state, "root2", &["admin"]).await;
    upsert_user(&state, "victim", &[]).await;
    let app = common::app(state);

    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/users/impersonate",
            &cookie,
            Some("user_id=root2"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let imp_cookie = set_cookie_value(&resp).expect("impersonation cookie");

    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/users/impersonate",
            &imp_cookie,
            Some("user_id=victim"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Stop impersonation

#[tokio::test]
async fn stop_restores_admin_and_audits() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let _admin_cookie = seed_admin(&state, "root").await;
    upsert_user(&state, "victim", &[]).await;
    let sessions = state.sessions.clone();
    let db = state.db.clone();
    let app = common::app(state);

    // Start, capture the impersonation cookie.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/users/impersonate",
            &_admin_cookie,
            Some("user_id=victim"),
        ))
        .await
        .unwrap();
    let imp_cookie = set_cookie_value(&resp).expect("impersonation cookie");

    // Stop, using the impersonation session.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/impersonate/stop",
            &imp_cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let restored = set_cookie_value(&resp).expect("stop should set a fresh admin cookie");

    // The fresh cookie is an ordinary admin session again.
    let id = sessions.verify(&restored).expect("cookie HMAC valid");
    let sess = sessions
        .lookup(id)
        .await
        .unwrap()
        .expect("session row exists");
    assert_eq!(sess.user_id, "root");
    assert_eq!(sess.impersonator_id, None);

    // Trail has both a start and a stop.
    let events = audit::recent(&db, 10).await.unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.iter().any(|e| e.action == "start"));
    assert!(events.iter().any(|e| e.action == "stop"));
}

#[tokio::test]
async fn stop_on_ordinary_session_is_noop_redirect() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let db = state.db.clone();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/impersonate/stop",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    // No impersonation was happening, so nothing is recorded.
    assert!(audit::recent(&db, 10).await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Kill switch: [gateway].allow_impersonation = false

#[tokio::test]
async fn disabled_hides_button_and_rejects_impersonate() {
    let state = common::state_with_admin_rbac_no_impersonation("http://unused.invalid").await;
    let cookie = seed_admin(&state, "root").await;
    upsert_user(&state, "victim", &[]).await;
    let db = state.db.clone();
    let app = common::app(state);

    // Roster still renders (viewing who's registered is independent of
    // impersonation) but offers no Impersonate control.
    let resp = app
        .serve(req_with_cookie(Method::GET, "/admin/users", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body_string(resp).await;
    assert!(
        body.contains("victim@example.com"),
        "roster should still list users"
    );
    assert!(
        !body.contains("/admin/users/impersonate"),
        "Impersonate control must be hidden when disabled"
    );
    assert!(
        body.contains("disabled"),
        "page should note impersonation is disabled"
    );

    // And the endpoint itself refuses, even hand-crafted.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/users/impersonate",
            &cookie,
            Some("user_id=victim"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        set_cookie_value(&resp).is_none(),
        "no session swap when disabled"
    );
    assert!(audit::recent(&db, 10).await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Banner wiring

#[tokio::test]
async fn banner_shows_while_impersonating_and_absent_otherwise() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let admin_cookie = seed_admin(&state, "root").await;
    upsert_user(&state, "victim", &[]).await;
    let app = common::app(state);

    // Ordinary admin session: no banner on an authed page.
    let resp = app
        .serve(req_with_cookie(Method::GET, "/tokens", &admin_cookie, None))
        .await
        .unwrap();
    let body = read_body_string(resp).await;
    assert!(
        !body.contains("/impersonate/stop"),
        "ordinary session must not show the impersonation banner"
    );

    // Start impersonation, then load an authed page with that cookie.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/admin/users/impersonate",
            &admin_cookie,
            Some("user_id=victim"),
        ))
        .await
        .unwrap();
    let imp_cookie = set_cookie_value(&resp).expect("impersonation cookie");

    let resp = app
        .serve(req_with_cookie(Method::GET, "/tokens", &imp_cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body_string(resp).await;
    assert!(
        body.contains("/impersonate/stop"),
        "impersonation session must show the banner with a stop control"
    );
    assert!(
        body.contains("You are impersonating"),
        "banner copy missing"
    );
    assert!(
        body.contains("victim@example.com"),
        "banner should name the impersonated user"
    );
}
