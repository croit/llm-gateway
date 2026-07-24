// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! RBAC enforcement suite.
//!
//! Proves the gateway-groups access model actually gates every surface:
//!
//!   * every `/v1/*` proxy route (chat, embeddings, images gen/edit, speech,
//!     transcriptions) + `/v1/models` listing + `/v1/models/{id}`,
//!   * the session proxy routes (`/api/v0/transcriptions`, `/api/v0/speech`),
//!   * the chat-UI model dropdown,
//!   * per-collection RAG gating,
//!   * per-connector MCP gating,
//!
//! for three principals against a pool restricted to a group:
//!   - a NON-member, non-admin user  → blocked (model invisible + `404`),
//!   - a MEMBER (holds the group)    → allowed,
//!   - an ADMIN                      → allowed (bypasses all restrictions).
//!
//! The upstream mock answers 200 on every path, so a `404` can only come from
//! our own route gate (`route_access` / `acquire_for_access`) — never from the
//! upstream — which is what makes "restricted ⇒ 404" a real enforcement signal.

use crate::common;

use std::collections::HashMap;
use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::router::service;
use gateway::rama_server::{RamaState, SessionStore};
use gateway::server::rbac::Resolver;
use gateway::server::rbac::config::{RbacConfig, RoleConfig, RoleMapping};
use gateway::server::upstreams::{
    self,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use gateway::server::{AppState, Config, db};
use rama::http::{Body, Method, Request, StatusCode};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_SECRET: [u8; 32] = [7u8; 32];

/// A backend pointed at the mock, optionally advertising image-edit support.
fn backend(name: &str, base_url: &str, supports_edit: bool) -> BackendConfig {
    BackendConfig {
        alias: None,
        probe_models: true,
        supports_edit,
        name: name.into(),
        base_url: base_url.into(),
        api_key_env: None,
        api_key: None,
        weight: 1,
        max_inflight: 16,
        health_path: "/models".into(),
        models: Vec::new(),
    }
}

fn pool(kind: PoolKind, be: BackendConfig, allowed_groups: Vec<String>) -> UpstreamPoolConfig {
    UpstreamPoolConfig {
        voices: Default::default(),
        allowed_groups,
        fallback_offline: None,
        compliance: Default::default(),
        enforce_limits: true,
        kind,
        strategy: PickerStrategy::RoundRobin,
        models: Vec::new(),
        backend: vec![be],
    }
}

/// One model id per pool kind (so a route resolves to exactly its pool).
const CHAT_MODEL: &str = "chat-model";
const EMBED_MODEL: &str = "embed-model";
const IMAGE_MODEL: &str = "image-model";
const SPEECH_MODEL: &str = "speech-model";
const TX_MODEL: &str = "tx-model";

/// The RBAC resolver used by every fixture: `platform-admins` → admin (admin
/// flag), `engineering` → engineering, default role `user`.
fn resolver() -> Arc<Resolver> {
    let rbac = RbacConfig {
        default_role: Some("user".into()),
        mappings: vec![
            RoleMapping {
                oidc_claim: "groups".into(),
                oidc_value: "platform-admins".into(),
                role: "admin".into(),
            },
            RoleMapping {
                oidc_claim: "groups".into(),
                oidc_value: "engineering".into(),
                role: "engineering".into(),
            },
        ],
    };
    let roles = vec![
        RoleConfig {
            id: "admin".into(),
            admin: true,
            models: vec!["*".into()],
            tools: vec!["*".into()],
            skills: vec![],
        },
        RoleConfig {
            id: "engineering".into(),
            admin: false,
            models: vec!["*".into()],
            tools: vec!["*".into()],
            skills: vec![],
        },
        RoleConfig {
            id: "user".into(),
            admin: false,
            models: vec!["*".into()],
            tools: vec![],
            skills: vec![],
        },
    ];
    Arc::new(Resolver::build(rbac, roles).unwrap())
}

struct Fixture {
    state: Arc<RamaState>,
    _upstream: MockServer,
}

/// Build a fixture whose pool of `restrict_kind` is limited to `groups` (all
/// other pools stay unrestricted). Seeds an admin user + a non-admin
/// engineering user, each with a bearer token AND a session cookie.
async fn fixture(restrict_kind: PoolKind, groups: &[&str]) -> (Fixture, Principals) {
    let upstream = MockServer::start().await;
    // Answer 200 on EVERY method/path so a 404 can only be our route gate.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list", "data": []
        })))
        .mount(&upstream)
        .await;
    let uri = upstream.uri();

    let g: Vec<String> = groups.iter().map(|s| (*s).to_string()).collect();
    let gof = |k: PoolKind| {
        if k == restrict_kind {
            g.clone()
        } else {
            Vec::new()
        }
    };

    let mut pools = HashMap::new();
    pools.insert(
        "chat".into(),
        pool(
            PoolKind::Chat,
            backend("chat-be", &uri, false),
            gof(PoolKind::Chat),
        ),
    );
    pools.insert(
        "embed".into(),
        pool(
            PoolKind::Embedding,
            backend("embed-be", &uri, false),
            gof(PoolKind::Embedding),
        ),
    );
    pools.insert(
        "image".into(),
        pool(
            PoolKind::Image,
            backend("image-be", &uri, true),
            gof(PoolKind::Image),
        ),
    );
    pools.insert(
        "speech".into(),
        pool(
            PoolKind::Speech,
            backend("speech-be", &uri, false),
            gof(PoolKind::Speech),
        ),
    );
    pools.insert(
        "voice".into(),
        pool(
            PoolKind::Transcription,
            backend("voice-be", &uri, false),
            gof(PoolKind::Transcription),
        ),
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    common::seed_pool_models(&registry, "chat", 0, &[CHAT_MODEL]);
    common::seed_pool_models(&registry, "embed", 0, &[EMBED_MODEL]);
    common::seed_pool_models(&registry, "image", 0, &[IMAGE_MODEL]);
    common::seed_pool_models(&registry, "speech", 0, &[SPEECH_MODEL]);
    common::seed_pool_models(&registry, "voice", 0, &[TX_MODEL]);

    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let tools = Arc::new(gateway::server::tools::ToolRegistry::new());
    let app = AppState::new(
        Config::default(),
        db_pool.clone(),
        registry,
        tools,
        resolver(),
    );
    let sessions = SessionStore::new(db_pool, TEST_SECRET);
    let state = RamaState::new(
        app,
        sessions,
        gateway::server::usage::UsageHandle::disabled(),
    );

    let admin = seed_principal(&state, "admin", &["platform-admins"]).await;
    let eng = seed_principal(&state, "eng", &["engineering"]).await;

    (
        Fixture {
            state: Arc::new(state),
            _upstream: upstream,
        },
        Principals { admin, eng },
    )
}

struct Principal {
    bearer: String,
    cookie: String,
}

struct Principals {
    admin: Principal,
    eng: Principal,
}

async fn seed_principal(state: &RamaState, id: &str, roles: &[&str]) -> Principal {
    use gateway::server::auth::token;
    use gateway::server::db::{tokens, users};
    use jiff::{SignedDuration, Timestamp};
    use uuid::Uuid;
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: id.into(),
            email: format!("{id}@example.com"),
            name: None,
            roles: roles.iter().map(|s| (*s).to_string()).collect(),
            created_at: now,
            updated_at: now,
            timezone: None,
        },
    )
    .await
    .unwrap();
    let (bearer, hash) = token::mint();
    tokens::insert(
        &state.db,
        &tokens::Token {
            id: Uuid::new_v4().to_string(),
            user_id: id.into(),
            name: "t".into(),
            hash,
            created_at: now,
            last_used_at: None,
            expires_at: now + SignedDuration::from_hours(1),
            revoked_at: None,
            tools_enabled: true,
        },
    )
    .await
    .unwrap();
    let session = state.sessions.create(id).await.unwrap();
    let cookie = state.sessions.sign(&session.id);
    Principal { bearer, cookie }
}

// ---- request drivers ------------------------------------------------------

async fn bearer_status(
    fx: &Fixture,
    bearer: &str,
    method: Method,
    uri: &str,
    body: Body,
    ct: Option<&str>,
) -> StatusCode {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"));
    if let Some(ct) = ct {
        b = b.header("content-type", ct);
    }
    let req = b.body(body).unwrap();
    service(fx.state.clone()).serve(req).await.unwrap().status()
}

async fn cookie_status(
    fx: &Fixture,
    cookie: &str,
    uri: &str,
    body: Body,
    ct: Option<&str>,
) -> StatusCode {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("cookie", format!("id={cookie}"));
    if let Some(ct) = ct {
        b = b.header("content-type", ct);
    }
    let req = b.body(body).unwrap();
    service(fx.state.clone()).serve(req).await.unwrap().status()
}

fn json_body(v: serde_json::Value) -> Body {
    Body::from(v.to_string())
}

/// A minimal multipart body carrying a `model` field plus one file field.
fn multipart(model: &str, file_field: &str, extra: &[(&str, &str)]) -> (Body, String) {
    let boundary = "TESTBOUNDARY123";
    let mut s = String::new();
    let mut push = |name: &str, val: &str| {
        s.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{val}\r\n"
        ));
    };
    push("model", model);
    for (k, v) in extra {
        push(k, v);
    }
    s.push_str(&format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"a.bin\"\r\nContent-Type: application/octet-stream\r\n\r\nRIFFxxxxWAVEfmt bytes\r\n--{boundary}--\r\n"
    ));
    (
        Body::from(s),
        format!("multipart/form-data; boundary={boundary}"),
    )
}

// ---- /v1 route enforcement ------------------------------------------------

/// One row of the matrix: restrict `kind`'s pool to `restrict_group`, then hit
/// `uri` and assert the three principals get the expected statuses. `allowed`
/// is the status a permitted caller gets (200 relayed from the mock).
async fn assert_route(kind: PoolKind, uri: &str, body: impl Fn() -> (Body, Option<String>)) {
    // Restrict to a group the eng user does NOT hold → eng blocked, admin bypasses.
    let (fx, p) = fixture(kind, &["finance"]).await;
    let (b, ct) = body();
    let eng = bearer_status(&fx, &p.eng.bearer, Method::POST, uri, b, ct.as_deref()).await;
    let (b, ct) = body();
    let adm = bearer_status(&fx, &p.admin.bearer, Method::POST, uri, b, ct.as_deref()).await;
    assert_eq!(
        eng,
        StatusCode::NOT_FOUND,
        "non-member must be blocked (404) on {uri}"
    );
    assert_ne!(
        adm,
        StatusCode::NOT_FOUND,
        "admin must bypass on {uri} (got {adm})"
    );

    // Restrict to the eng user's OWN group → eng allowed.
    let (fx, p) = fixture(kind, &["engineering"]).await;
    let (b, ct) = body();
    let eng = bearer_status(&fx, &p.eng.bearer, Method::POST, uri, b, ct.as_deref()).await;
    assert_ne!(
        eng,
        StatusCode::NOT_FOUND,
        "member must be allowed on {uri} (got {eng})"
    );

    // Unrestricted → eng allowed.
    let (fx, p) = fixture(kind, &[]).await;
    let (b, ct) = body();
    let eng = bearer_status(&fx, &p.eng.bearer, Method::POST, uri, b, ct.as_deref()).await;
    assert_ne!(
        eng,
        StatusCode::NOT_FOUND,
        "unrestricted must be allowed on {uri} (got {eng})"
    );
}

#[tokio::test]
async fn v1_chat_completions_gated() {
    assert_route(PoolKind::Chat, "/v1/chat/completions", || {
        (
            json_body(
                json!({"model": CHAT_MODEL, "messages": [{"role": "user", "content": "hi"}]}),
            ),
            Some("application/json".into()),
        )
    })
    .await;
}

#[tokio::test]
async fn v1_embeddings_gated() {
    assert_route(PoolKind::Embedding, "/v1/embeddings", || {
        (
            json_body(json!({"model": EMBED_MODEL, "input": "hi"})),
            Some("application/json".into()),
        )
    })
    .await;
}

#[tokio::test]
async fn v1_images_generations_gated() {
    assert_route(PoolKind::Image, "/v1/images/generations", || {
        (
            json_body(json!({"model": IMAGE_MODEL, "prompt": "x"})),
            Some("application/json".into()),
        )
    })
    .await;
}

#[tokio::test]
async fn v1_audio_speech_gated() {
    assert_route(PoolKind::Speech, "/v1/audio/speech", || {
        (
            json_body(json!({"model": SPEECH_MODEL, "input": "hi"})),
            Some("application/json".into()),
        )
    })
    .await;
}

#[tokio::test]
async fn v1_audio_transcriptions_gated() {
    assert_route(PoolKind::Transcription, "/v1/audio/transcriptions", || {
        let (b, ct) = multipart(TX_MODEL, "file", &[]);
        (b, Some(ct))
    })
    .await;
}

#[tokio::test]
async fn v1_images_edits_gated() {
    assert_route(PoolKind::Image, "/v1/images/edits", || {
        let (b, ct) = multipart(IMAGE_MODEL, "image", &[("prompt", "x")]);
        (b, Some(ct))
    })
    .await;
}

// ---- /v1/models listing + retrieve ---------------------------------------

#[tokio::test]
async fn v1_models_listing_is_per_group() {
    let (fx, p) = fixture(PoolKind::Chat, &["finance"]).await;
    // eng: chat-model withheld (its only pool is finance-only); others visible.
    let eng_body = models_ids(&fx, &p.eng.bearer).await;
    assert!(
        !eng_body.contains(&CHAT_MODEL.to_string()),
        "eng must NOT see restricted chat model: {eng_body:?}"
    );
    assert!(
        eng_body.contains(&EMBED_MODEL.to_string()),
        "eng still sees unrestricted models: {eng_body:?}"
    );
    // admin: sees everything (bypass).
    let adm_body = models_ids(&fx, &p.admin.bearer).await;
    assert!(
        adm_body.contains(&CHAT_MODEL.to_string()),
        "admin sees restricted model (bypass): {adm_body:?}"
    );

    // member sees it.
    let (fx, p) = fixture(PoolKind::Chat, &["engineering"]).await;
    let eng_body = models_ids(&fx, &p.eng.bearer).await;
    assert!(
        eng_body.contains(&CHAT_MODEL.to_string()),
        "member sees the model: {eng_body:?}"
    );
}

#[tokio::test]
async fn v1_retrieve_model_is_per_group() {
    let (fx, p) = fixture(PoolKind::Chat, &["finance"]).await;
    let uri = format!("/v1/models/{CHAT_MODEL}");
    let eng = bearer_status(&fx, &p.eng.bearer, Method::GET, &uri, Body::empty(), None).await;
    let adm = bearer_status(&fx, &p.admin.bearer, Method::GET, &uri, Body::empty(), None).await;
    assert_eq!(
        eng,
        StatusCode::NOT_FOUND,
        "non-member cannot retrieve a restricted model"
    );
    assert_eq!(adm, StatusCode::OK, "admin can retrieve it (bypass)");
}

async fn models_ids(fx: &Fixture, bearer: &str) -> Vec<String> {
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/models")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let resp = service(fx.state.clone()).serve(req).await.unwrap();
    let bytes = common::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

// ---- session proxy routes -------------------------------------------------

#[tokio::test]
async fn session_speech_is_per_group() {
    // /api/v0/speech resolves the speech pool via speech_target; gate applies.
    let (fx, p) = fixture(PoolKind::Speech, &["finance"]).await;
    let body = || json_body(json!({"text": "hello", "language": "en"}));
    let eng = cookie_status(
        &fx,
        &p.eng.cookie,
        "/api/v0/speech",
        body(),
        Some("application/json"),
    )
    .await;
    let adm = cookie_status(
        &fx,
        &p.admin.cookie,
        "/api/v0/speech",
        body(),
        Some("application/json"),
    )
    .await;
    assert_eq!(
        eng,
        StatusCode::NOT_FOUND,
        "non-member blocked on session speech (got {eng})"
    );
    assert_ne!(
        adm,
        StatusCode::NOT_FOUND,
        "admin bypasses on session speech (got {adm})"
    );
}

#[tokio::test]
async fn session_transcription_is_per_group() {
    let (fx, p) = fixture(PoolKind::Transcription, &["finance"]).await;
    let (b, ct) = multipart(TX_MODEL, "file", &[]);
    let eng = cookie_status(&fx, &p.eng.cookie, "/api/v0/transcriptions", b, Some(&ct)).await;
    let (b, ct) = multipart(TX_MODEL, "file", &[]);
    let adm = cookie_status(&fx, &p.admin.cookie, "/api/v0/transcriptions", b, Some(&ct)).await;
    assert_eq!(
        eng,
        StatusCode::NOT_FOUND,
        "non-member blocked on session transcription (got {eng})"
    );
    assert_ne!(
        adm,
        StatusCode::NOT_FOUND,
        "admin bypasses on session transcription (got {adm})"
    );
}

// ---- resolver-level primitive (shared by RAG + MCP + pools) ---------------

#[tokio::test]
async fn resource_allowed_matches_pool_rag_mcp_semantics() {
    let r = resolver();
    let eng = r.role_ids_for(&["engineering".into()]);
    let admin_ids = r.role_ids_for(&["platform-admins".into()]);
    // empty allowed_groups → everyone
    assert!(r.resource_allowed(&eng, &[]));
    // member holds the group
    assert!(r.resource_allowed(&eng, &["engineering".into()]));
    // non-member blocked
    assert!(!r.resource_allowed(&eng, &["finance".into()]));
    // admin bypasses
    assert!(r.resource_allowed(&admin_ids, &["finance".into()]));
}

// ---- RAG per-collection gating -------------------------------------------

#[tokio::test]
async fn rag_collection_gating_matches_resolver() {
    // The RAG tools filter each collection with `resolver.resource_allowed(
    // ctx.roles→ids, collection.allowed_groups)`. Persist collections with
    // different `allowed_groups` and assert the exact predicate the tools apply.
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mk = |name: &str| db::rag::NewCollection {
        name: name.into(),
        description: None,
        git_url: "https://example.com/r.git".into(),
        git_ref: "main".into(),
        pat: None,
        embedding_model: "e".into(),
        include_globs: vec![],
        exclude_globs: vec![],
        chunk_size: 100,
        chunk_overlap: 10,
        search_mode: db::rag::SearchMode::Versioned,
    };
    let open = db::rag::create_collection(&pool, &mk("open"))
        .await
        .unwrap();
    let dev = db::rag::create_collection(&pool, &mk("dev-docs"))
        .await
        .unwrap();
    let fin = db::rag::create_collection(&pool, &mk("fin-docs"))
        .await
        .unwrap();
    db::rag::set_allowed_groups(&pool, dev.id, &["engineering".into()])
        .await
        .unwrap();
    db::rag::set_allowed_groups(&pool, fin.id, &["finance".into()])
        .await
        .unwrap();

    let cols = db::rag::list_collections(&pool).await.unwrap();
    let r = resolver();
    let eng = r.role_ids_for(&["engineering".into()]);
    let admin_ids = r.role_ids_for(&["platform-admins".into()]);
    let visible = |ids: &[String]| -> Vec<String> {
        cols.iter()
            .filter(|c| r.resource_allowed(ids, &c.allowed_groups))
            .map(|c| c.name.clone())
            .collect()
    };
    let eng_vis = visible(&eng);
    assert!(
        eng_vis.contains(&"open".to_string()),
        "eng sees unrestricted"
    );
    assert!(
        eng_vis.contains(&"dev-docs".to_string()),
        "eng sees its own group's collection"
    );
    assert!(
        !eng_vis.contains(&"fin-docs".to_string()),
        "eng does NOT see finance-only collection"
    );
    // admin sees all three (bypass).
    assert_eq!(visible(&admin_ids).len(), 3);
    // roundtrip of the persisted list
    assert_eq!(open.allowed_groups, Vec::<String>::new());
    assert_eq!(
        db::rag::find_collection_by_id(&pool, dev.id)
            .await
            .unwrap()
            .unwrap()
            .allowed_groups,
        vec!["engineering".to_string()]
    );
}

// ---- MCP per-connector gating --------------------------------------------

#[tokio::test]
async fn mcp_connector_gating() {
    use gateway::server::db::mcp_catalog::{AuthKind, Connector, Scope};
    use jiff::Timestamp;
    let mut c = Connector {
        key: "net-tools".into(),
        name: "Network tools".into(),
        description: None,
        icon: None,
        category: None,
        url: "http://127.0.0.1:1/mcp".into(),
        auth: AuthKind::None,
        scope: Scope::Global,
        audit: false,
        use_dcr: false,
        client_id: None,
        client_secret_ct: None,
        client_secret_nonce: None,
        authorize_url: None,
        token_url: None,
        registration_url: None,
        scopes: vec![],
        allowed_groups: vec![],
        enabled: true,
        seeded: false,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
    };
    // unrestricted → everyone
    assert!(c.allows(&["engineering".into()], false));
    // restricted to a group the caller lacks → blocked
    c.allowed_groups = vec!["network_admin".into()];
    assert!(!c.allows(&["engineering".into()], false));
    // member → allowed
    assert!(c.allows(&["network_admin".into()], false));
    // admin → bypass
    assert!(c.allows(&["engineering".into()], true));
}
