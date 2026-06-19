// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-user `/tools` page: list the tools a user's roles grant, toggle
//! one off, and confirm the choice persists so the request path can
//! honour it.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::{RamaState, SessionStore, router::router};
use gateway::server::db::{self, user_tool_prefs, users};
use gateway::server::rbac::Resolver;
use gateway::server::rbac::config::{RbacConfig, RoleConfig, RoleMapping};
use gateway::server::tools::ToolRegistry;
use gateway::server::tools::echo::Echo;
use gateway::server::tools::fetch_url::FetchUrl;
use gateway::server::tools::memory::{Recall, Remember};
use gateway::server::tools::search_web::SearchWeb;
use gateway::server::tools::time::CurrentTimestamp;
use gateway::server::upstreams::{
    self,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use gateway::server::{AppState, Config};
use jiff::{SignedDuration, Timestamp};
use rama::http::{Body, Method, Request, StatusCode};
use serde_json::json;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// State with a handful of real tools registered and an `admin` role
/// that grants them all (`tools: ["*"]`). A user whose roles include
/// `"admin"` therefore sees the full list on /tools. `upstream_uri`
/// is the chat backend (use `"http://unused.invalid"` when the test
/// never forwards).
async fn state_with_tools(upstream_uri: &str) -> RamaState {
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mut pools = HashMap::new();
    pools.insert(
        "pool".to_string(),
        UpstreamPoolConfig {
            compliance: Default::default(),
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                name: "mock".into(),
                base_url: upstream_uri.into(),
                api_key_env: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    common::seed_pool_models(&registry, "pool", 0, &["model-a"]);

    let tools = Arc::new(
        ToolRegistry::new()
            .with(Echo)
            .with(SearchWeb)
            .with(FetchUrl)
            .with(CurrentTimestamp)
            .with(Remember)
            .with(Recall),
    );
    let rbac_config = RbacConfig {
        default_role: None,
        mappings: vec![RoleMapping {
            oidc_claim: "groups".into(),
            oidc_value: "admin".into(),
            role: "admin".into(),
        }],
    };
    let admin_role = RoleConfig {
        id: "admin".into(),
        models: vec!["*".into()],
        tools: vec!["*".into()],
        skills: vec![],
    };
    let rbac = Arc::new(Resolver::build(rbac_config, vec![admin_role]).unwrap());

    let app = AppState::new(Config::default(), pool.clone(), registry, tools, rbac);
    let sessions = SessionStore::new(pool, common::TEST_SECRET);
    RamaState::new(app, sessions)
}

/// Seed a user with the given OIDC roles + an active session; return
/// the signed cookie value.
async fn seed_session_with_roles(state: &RamaState, user_id: &str, roles: &[&str]) -> String {
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: user_id.into(),
            email: format!("{user_id}@example.com"),
            name: None,
            roles: roles.iter().map(|r| (*r).to_string()).collect(),
            created_at: now,
            updated_at: now,
            timezone: None,
        },
    )
    .await
    .unwrap();
    let session = state.sessions.create(user_id).await.unwrap();
    state.sessions.sign(&session.id)
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
async fn tools_page_anonymous_redirects_to_login() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app.serve(common::req(Method::GET, "/tools")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get(rama::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        location.starts_with("/login") && location.contains("return_to="),
        "anon must bounce to /login carrying return_to; got `{location}`"
    );
}

#[tokio::test]
async fn tools_page_lists_granted_tools_and_hides_smoke_test() {
    let state = state_with_tools("http://unused.invalid").await;
    let cookie = seed_session_with_roles(&state, "alice", &["admin"]).await;
    let app = router(Arc::new(state));

    let resp = app.serve(get_with_cookie("/tools", &cookie)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();

    assert!(body.contains("search_web"), "expected search_web row");
    assert!(body.contains("fetch_url"), "expected fetch_url row");
    assert!(
        body.contains("Web &amp; Network"),
        "expected category heading"
    );
    assert!(body.contains("toggle"), "expected daisyUI toggle");
    // company_echo is a smoke-test tool — granted via `*` but hidden.
    assert!(
        !body.contains("company_echo"),
        "smoke-test tool should be hidden, body:\n{body}"
    );
}

#[tokio::test]
async fn toggling_a_tool_off_persists_and_patches_the_row() {
    let state = Arc::new(state_with_tools("http://unused.invalid").await);
    let cookie = seed_session_with_roles(&state, "alice", &["admin"]).await;
    let app = router(state.clone());

    // Unchecked checkbox → the browser omits `enabled` from the form.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/tools/toggle")
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("tool_key=search_web"))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("event: datastar-patch-elements"),
        "expected an SSE patch, got:\n{body}"
    );
    assert!(
        body.contains("#tool-row-search_web"),
        "expected the row-targeted patch, got:\n{body}"
    );

    // The choice is persisted as a disable for this user.
    let disabled = user_tool_prefs::disabled_for_user(&state.db, "alice")
        .await
        .unwrap();
    assert!(disabled.contains("search_web"));
}

#[tokio::test]
async fn toggling_a_tool_back_on_clears_the_disable() {
    let state = Arc::new(state_with_tools("http://unused.invalid").await);
    let cookie = seed_session_with_roles(&state, "alice", &["admin"]).await;
    let app = router(state.clone());

    // Off, then on (checked checkbox sends `enabled=true`).
    for body in ["tool_key=search_web", "tool_key=search_web&enabled=true"] {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/tools/toggle")
            .header("cookie", format!("id={cookie}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        let resp = app.serve(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = common::read_body(resp).await;
    }

    let disabled = user_tool_prefs::disabled_for_user(&state.db, "alice")
        .await
        .unwrap();
    assert!(
        !disabled.contains("search_web"),
        "re-enabling should clear the disable, got: {disabled:?}"
    );
}

#[tokio::test]
async fn cannot_toggle_a_tool_the_roles_dont_grant() {
    let state = Arc::new(state_with_tools("http://unused.invalid").await);
    // No roles → RBAC grants nothing → no tool is toggleable.
    let cookie = seed_session_with_roles(&state, "bob", &[]).await;
    let app = router(state.clone());

    let req = Request::builder()
        .method(Method::POST)
        .uri("/tools/toggle")
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("tool_key=search_web"))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("unknown tool"), "expected rejection toast");

    // Nothing was written.
    let disabled = user_tool_prefs::disabled_for_user(&state.db, "bob")
        .await
        .unwrap();
    assert!(disabled.is_empty());
}

/// Seed an `admin`-role user with a bearer token for the `/v1` proxy.
async fn seed_admin_with_bearer(state: &RamaState, user_id: &str) -> String {
    use gateway::server::auth::token;
    use gateway::server::db::tokens;
    let _ = seed_session_with_roles(state, user_id, &["admin"]).await;
    let (plaintext, hash) = token::mint();
    tokens::insert(
        &state.db,
        &tokens::Token {
            id: Uuid::new_v4().to_string(),
            user_id: user_id.into(),
            name: "test".into(),
            hash,
            created_at: Timestamp::now(),
            last_used_at: None,
            expires_at: Timestamp::now() + SignedDuration::from_hours(1),
            revoked_at: None,
            // Tool use on so the per-user pref subtraction is observable on
            // the proxy tool path.
            tools_enabled: true,
        },
    )
    .await
    .unwrap();
    plaintext
}

/// End-to-end: a tool the user turned off is never offered to the
/// upstream model, while the rest of their granted tools still are.
#[tokio::test]
async fn disabled_tool_is_not_injected_into_the_upstream_request() {
    let upstream = MockServer::start().await;
    // Plain assistant reply (no tool_calls) so the runner forwards once
    // and we can inspect the single request it made.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "x",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }]
        })))
        .mount(&upstream)
        .await;

    let state = Arc::new(state_with_tools(&upstream.uri()).await);
    let bearer = seed_admin_with_bearer(&state, "alice").await;
    // Alice turns fetch_url off on her /tools page.
    user_tool_prefs::set(&state.db, "alice", "fetch_url", false)
        .await
        .unwrap();
    let app = router(state.clone());

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "model-a",
                "messages": [{"role": "user", "content": "hello"}]
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let requests = upstream.received_requests().await.unwrap();
    let forwarded: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let tool_names: Vec<&str> = forwarded["tools"]
        .as_array()
        .expect("forwarded request carries a tools array")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();

    assert!(
        tool_names.contains(&"search_web"),
        "still-enabled tools should be offered, got: {tool_names:?}"
    );
    assert!(
        !tool_names.contains(&"fetch_url"),
        "disabled tool must not be offered, got: {tool_names:?}"
    );
}

/// The `remember` + `recall` tools share one "memory" toggle — turning
/// it off must drop both from the upstream request.
#[tokio::test]
async fn disabling_memory_drops_both_remember_and_recall() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "x",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }]
        })))
        .mount(&upstream)
        .await;

    let state = Arc::new(state_with_tools(&upstream.uri()).await);
    let bearer = seed_admin_with_bearer(&state, "alice").await;
    // One switch governs the whole capability.
    user_tool_prefs::set(&state.db, "alice", "memory", false)
        .await
        .unwrap();
    let app = router(state.clone());

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "model-a",
                "messages": [{"role": "user", "content": "hello"}]
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let requests = upstream.received_requests().await.unwrap();
    let forwarded: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let tool_names: Vec<&str> = forwarded["tools"]
        .as_array()
        .expect("forwarded request carries a tools array")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();

    assert!(
        !tool_names.contains(&"remember") && !tool_names.contains(&"recall"),
        "disabling memory must drop both halves, got: {tool_names:?}"
    );
    // A sibling tool is unaffected.
    assert!(tool_names.contains(&"search_web"));
}

// ---------------------------------------------------------------------------
// Per-token tool gating (the master switch + per-token disable keys).

/// Seed an `admin` user + a bearer with an explicit `tools_enabled`
/// state, returning `(plaintext, token_id)` so the test can set
/// per-token prefs against the id.
async fn seed_bearer_with_tools(
    state: &RamaState,
    user_id: &str,
    tools_enabled: bool,
) -> (String, String) {
    use gateway::server::auth::token;
    use gateway::server::db::tokens;
    let _ = seed_session_with_roles(state, user_id, &["admin"]).await;
    let (plaintext, hash) = token::mint();
    let id = Uuid::new_v4().to_string();
    tokens::insert(
        &state.db,
        &tokens::Token {
            id: id.clone(),
            user_id: user_id.into(),
            name: "test".into(),
            hash,
            created_at: Timestamp::now(),
            last_used_at: None,
            expires_at: Timestamp::now() + SignedDuration::from_hours(1),
            revoked_at: None,
            tools_enabled,
        },
    )
    .await
    .unwrap();
    (plaintext, id)
}

fn plain_reply_mock() -> Mock {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "x",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }]
        })))
}

/// A token with tool use **off** (the default) gets the byte-dumb
/// passthrough: the gateway injects no `tools` and stamps no
/// `x-gateway-tool-rounds` header, even though the user's roles grant
/// every tool.
#[tokio::test]
async fn token_with_tool_use_off_gets_passthrough() {
    let upstream = MockServer::start().await;
    plain_reply_mock().mount(&upstream).await;

    let state = Arc::new(state_with_tools(&upstream.uri()).await);
    let (bearer, _id) = seed_bearer_with_tools(&state, "alice", false).await;
    let app = router(state.clone());

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model": "model-a", "messages": [{"role": "user", "content": "hi"}]})
                .to_string(),
        ))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Byte-dumb path never sets this header.
    assert!(
        resp.headers().get("x-gateway-tool-rounds").is_none(),
        "tool-use-off token must take the passthrough path"
    );

    let requests = upstream.received_requests().await.unwrap();
    let forwarded: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(
        forwarded.get("tools").is_none(),
        "no gateway tools should be injected, got: {forwarded}"
    );
}

/// Same user, a token with tool use **on**: gateway tools are injected.
/// Flipping the master switch is what gates the whole feature.
#[tokio::test]
async fn token_with_tool_use_on_injects_gateway_tools() {
    let upstream = MockServer::start().await;
    plain_reply_mock().mount(&upstream).await;

    let state = Arc::new(state_with_tools(&upstream.uri()).await);
    let (bearer, _id) = seed_bearer_with_tools(&state, "alice", true).await;
    let app = router(state.clone());

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model": "model-a", "messages": [{"role": "user", "content": "hi"}]})
                .to_string(),
        ))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let requests = upstream.received_requests().await.unwrap();
    let forwarded: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let tool_names: Vec<&str> = forwarded["tools"]
        .as_array()
        .expect("tool-use-on token gets a tools array")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(tool_names.contains(&"search_web"), "got: {tool_names:?}");
}

/// A per-token disabled capability is dropped from the upstream request
/// while the token's other tools remain — the "no RAG token" shape.
#[tokio::test]
async fn per_token_disabled_capability_is_not_injected() {
    let upstream = MockServer::start().await;
    plain_reply_mock().mount(&upstream).await;

    let state = Arc::new(state_with_tools(&upstream.uri()).await);
    let (bearer, id) = seed_bearer_with_tools(&state, "alice", true).await;
    // This token alone drops search_web (stand-in for a "no RAG" token).
    db::token_tool_prefs::set(&state.db, &id, "search_web", false)
        .await
        .unwrap();
    let app = router(state.clone());

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model": "model-a", "messages": [{"role": "user", "content": "hi"}]})
                .to_string(),
        ))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let requests = upstream.received_requests().await.unwrap();
    let forwarded: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let tool_names: Vec<&str> = forwarded["tools"]
        .as_array()
        .expect("forwarded request carries a tools array")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(
        !tool_names.contains(&"search_web"),
        "per-token disabled tool must not be offered, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"fetch_url"),
        "other tools still offered, got: {tool_names:?}"
    );
}
