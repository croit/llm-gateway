// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Shared scaffolding for the integration tests.
//!
//! Each test builds a fresh `RamaState` against an in-memory SQLite
//! and a wiremock upstream, calls `router(state).serve(req)`
//! directly, and asserts on the response. No socket binding — rama's
//! `serve` is a pure async function that takes a `Request` and returns
//! a `Response`.
//!
//! Each integration test file is compiled as its own binary and only
//! uses a subset of these helpers, so `dead_code` is allowed at the
//! module level — clippy would otherwise flag the unused helpers in
//! every binary that doesn't reference them.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use gateway::rama_server::{RamaState, SessionStore, router::service};
use gateway::server::rbac::Resolver;
use gateway::server::tools::ToolRegistry;
use gateway::server::upstreams::{
    self,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use gateway::server::{AppState, Config, db};
// `Service::serve` is the call-router-directly entry point that lets
// us drive the rama Router without binding a socket. Re-exported here
// so each test file gets it via `use common::*;`.
pub use rama::Service;
use rama::http::Body;

/// Default HMAC secret used by the in-test SessionStore. Tests that
/// need to verify a cookie can re-derive the signature with this key.
pub const TEST_SECRET: [u8; 32] = [7u8; 32];

/// A backend with the standard test config, pointed at `base_url` (a wiremock
/// uri, or `http://unused.invalid` when no request should be forwarded).
fn mock_backend(name: &str, base_url: &str) -> BackendConfig {
    BackendConfig {
        name: name.into(),
        base_url: base_url.into(),
        api_key_env: None,
        weight: 1,
        max_inflight: 16,
        health_path: "/models".into(),
        models: Vec::new(),
    }
}

/// Assemble a `RamaState` from an already-seeded registry plus the shared
/// in-memory db — the identical tail every pool builder repeats.
fn state_from_registry(db_pool: db::Pool, registry: Arc<upstreams::UpstreamRegistry>) -> RamaState {
    let tools = Arc::new(ToolRegistry::new());
    let rbac = Arc::new(Resolver::empty());
    let app = AppState::new(Config::default(), db_pool.clone(), registry, tools, rbac);
    let sessions = SessionStore::new(db_pool, TEST_SECRET);
    RamaState::new(app, sessions)
}

/// Build a `RamaState` wired to a single backend pool (chat kind).
/// `upstream_url` is typically a wiremock `mock_server.uri()`.
pub async fn state_with_chat_pool(upstream_url: &str) -> RamaState {
    state_with_pool(upstream_url, PoolKind::Chat, "model-a").await
}

/// Build a `RamaState` wired to a single backend pool of the requested
/// kind. Tests bypass the health probe entirely — the pool's lone
/// backend has its advertised-model set seeded directly via
/// `Backend::set_models` so `acquire_for(model_name, kind)` succeeds.
pub async fn state_with_pool(upstream_url: &str, kind: PoolKind, model_name: &str) -> RamaState {
    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mut pools = HashMap::new();
    pools.insert(
        "pool".to_string(),
        UpstreamPoolConfig {
            compliance: Default::default(),
            kind,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![mock_backend("mock", upstream_url)],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    seed_pool_models(&registry, "pool", 0, &[model_name]);
    state_from_registry(db_pool, registry)
}

/// Build a `RamaState` with a 2-replica chat pool (both replicas probe-seeded
/// with the *same* id, to exercise `/v1/models` de-dup) plus a transcription
/// pool whose model id comes purely from config (`models = [...]`, no probe) —
/// mirroring a Voxtral realtime backend with no usable `/models` endpoint.
/// Used by the `/v1/models` OpenAI-parity tests.
pub async fn state_with_chat_and_config_transcription(
    chat_model: &str,
    transcription_model: &str,
) -> RamaState {
    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mut pools = HashMap::new();
    pools.insert(
        "chat".to_string(),
        UpstreamPoolConfig {
            compliance: Default::default(),
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![
                mock_backend("chat-a", "http://unused.invalid"),
                mock_backend("chat-b", "http://unused.invalid"),
            ],
        },
    );
    pools.insert(
        "voice".to_string(),
        UpstreamPoolConfig {
            compliance: Default::default(),
            kind: PoolKind::Transcription,
            strategy: PickerStrategy::RoundRobin,
            // Config-only fallback: no probe will ever seed this backend.
            models: vec![transcription_model.to_string()],
            backend: vec![mock_backend("voxtral", "http://unused.invalid")],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    // Both chat replicas advertise the same id → must collapse to one entry.
    seed_pool_models(&registry, "chat", 0, &[chat_model]);
    seed_pool_models(&registry, "chat", 1, &[chat_model]);
    state_from_registry(db_pool, registry)
}

/// Build a `RamaState` with a chat pool (one model) AND an embedding pool
/// (another model), both probe-seeded. For pool-kind routing-isolation tests:
/// a model living in one pool must NOT be routable via the other kind's
/// endpoint. Backends point at `unused.invalid` — the isolation cases reject
/// at the kind filter in `acquire_for`, before any forward.
pub async fn state_with_chat_and_embed(chat_model: &str, embed_model: &str) -> RamaState {
    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mut pools = HashMap::new();
    pools.insert(
        "chat".to_string(),
        UpstreamPoolConfig {
            compliance: Default::default(),
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![mock_backend("chat", "http://unused.invalid")],
        },
    );
    pools.insert(
        "embed".to_string(),
        UpstreamPoolConfig {
            compliance: Default::default(),
            kind: PoolKind::Embedding,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![mock_backend("embed", "http://unused.invalid")],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    seed_pool_models(&registry, "chat", 0, &[chat_model]);
    seed_pool_models(&registry, "embed", 0, &[embed_model]);
    state_from_registry(db_pool, registry)
}

/// Build a `RamaState` whose RBAC resolver maps OIDC value `"admin"`
/// → internal role `"admin"`, so tests that depend on the admin
/// gate (e.g. `/admin/models`) actually see admin status on users
/// whose `roles` includes `"admin"`. Tests that don't care use
/// `state_with_chat_pool`.
pub async fn state_with_admin_rbac(upstream_url: &str) -> RamaState {
    use gateway::server::rbac::config::{RbacConfig, RoleConfig, RoleMapping};
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
                base_url: upstream_url.into(),
                api_key_env: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    seed_pool_models(&registry, "pool", 0, &["model-a"]);

    let tools = Arc::new(ToolRegistry::new());
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
    };
    let rbac = Arc::new(Resolver::build(rbac_config, vec![admin_role]).unwrap());

    let app = AppState::new(Config::default(), pool.clone(), registry, tools, rbac);
    let sessions = SessionStore::new(pool, TEST_SECRET);
    RamaState::new(app, sessions)
}

/// Test-only: pretend the health probe just ran and advertise `models`
/// on the named pool's `backend_idx`-th backend. Real code calls
/// `Backend::set_models` from `upstreams::health::probe_once`; tests
/// use this to bypass the network and route deterministically.
pub fn seed_pool_models(
    registry: &upstreams::UpstreamRegistry,
    pool_name: &str,
    backend_idx: usize,
    models: &[&str],
) {
    use std::collections::HashSet;
    let pool = registry
        .pools()
        .find(|p| p.name == pool_name)
        .expect("seed_pool_models: pool not found");
    let set: HashSet<String> = models.iter().map(|s| (*s).to_string()).collect();
    pool.backends[backend_idx].set_models(set);
}

/// Build the production HTTP service for `state`. Tests drive it directly
/// via `app.serve(req).await` — the same wrapped stack `router::serve` binds
/// to a socket, so 404s and other `RouterError`s render as responses here too.
pub fn app(
    state: RamaState,
) -> impl Service<rama::http::Request, Output = rama::http::Response, Error = std::convert::Infallible>
+ Clone {
    service(Arc::new(state))
}

/// Drain a rama response body into Bytes for assertions.
pub async fn read_body(resp: rama::http::Response) -> rama::bytes::Bytes {
    use rama::http::body::util::BodyExt;
    resp.into_body().collect().await.unwrap().to_bytes()
}

/// Build a minimal request with the given method, URI, and an empty body.
pub fn req(method: rama::http::Method, uri: &str) -> rama::http::Request {
    rama::http::Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// Seed a user + an active session, return the signed cookie value.
/// Use the returned string as `Cookie: id=<value>` in subsequent requests.
pub async fn seed_session(state: &RamaState, user_id: &str, email: &str) -> String {
    use gateway::server::db::users;
    use jiff::Timestamp;
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: user_id.into(),
            email: email.into(),
            name: None,
            roles: vec![],
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

/// Seed a user + an active session + a bearer token. Returns the
/// plaintext bearer suitable for an `Authorization: Bearer …` header.
pub async fn seed_user_with_token(state: &RamaState, user_id: &str) -> String {
    use gateway::server::auth::token;
    use gateway::server::db::{tokens, users};
    use jiff::{SignedDuration, Timestamp};
    use uuid::Uuid;
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: user_id.into(),
            email: format!("{user_id}@example.com"),
            name: None,
            roles: vec![],
            created_at: now,
            updated_at: now,
            timezone: None,
        },
    )
    .await
    .unwrap();
    let (plaintext, hash) = token::mint();
    tokens::insert(
        &state.db,
        &tokens::Token {
            id: Uuid::new_v4().to_string(),
            user_id: user_id.into(),
            name: "test".into(),
            hash,
            created_at: now,
            last_used_at: None,
            expires_at: now + SignedDuration::from_hours(1),
            revoked_at: None,
        },
    )
    .await
    .unwrap();
    plaintext
}
