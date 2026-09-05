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
use gateway_core::server::rbac::Resolver;
use gateway_core::server::upstreams::{
    self,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use gateway_core::server::{Config, db};
use gateway_runtime::server::AppState;
use gateway_runtime::server::tools::ToolRegistry;
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
pub fn mock_backend(name: &str, base_url: &str) -> BackendConfig {
    BackendConfig {
        alias: None,
        probe_models: true,
        supports_edit: false,
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

/// Assemble a `RamaState` from an already-seeded registry plus the shared
/// in-memory db — the identical tail every pool builder repeats.
pub fn state_from_registry(
    db_pool: db::Pool,
    registry: Arc<upstreams::UpstreamRegistry>,
) -> RamaState {
    let tools = Arc::new(ToolRegistry::new());
    let rbac = Arc::new(Resolver::empty());
    let app = AppState::new(Config::default(), db_pool.clone(), registry, tools, rbac)
        .with_tool_family_builder(gateway::tool_families::typst());
    let sessions = SessionStore::new(db_pool, TEST_SECRET);
    RamaState::new(
        app,
        sessions,
        gateway_core::server::usage::UsageHandle::disabled(),
    )
}

/// Build a minimal `RamaState` (empty upstream registry) with the Agent
/// Skills feature enabled: a global store at `root` plus a per-user store at
/// `root/.users`. For the `/skills` (per-user private skills) page tests.
pub async fn state_with_user_skills(root: std::path::PathBuf) -> RamaState {
    use gateway_features::server::skills::{SkillStore, UserSkillStore};
    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let registry = upstreams::UpstreamRegistry::new(&HashMap::new()).unwrap();
    let tools = Arc::new(ToolRegistry::new());
    let rbac = Arc::new(Resolver::empty());
    let app = AppState::new(Config::default(), db_pool.clone(), registry, tools, rbac)
        .with_skills(Arc::new(SkillStore::load(root.clone())))
        .with_user_skills(Arc::new(UserSkillStore::new(root.join(".users"))));
    let sessions = SessionStore::new(db_pool, TEST_SECRET);
    RamaState::new(
        app,
        sessions,
        gateway_core::server::usage::UsageHandle::disabled(),
    )
}

/// Build a minimal `RamaState` (empty upstream registry) with the Agent
/// Skills feature **not** configured — for asserting the `/skills` nav entry
/// is hidden when skills are off.
pub async fn state_no_skills() -> RamaState {
    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let registry = upstreams::UpstreamRegistry::new(&HashMap::new()).unwrap();
    state_from_registry(db_pool, registry)
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
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
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

/// [`state_with_pool`] plus a client-facing alias on the backend: `alias`
/// routes to `real_model`. Backs the Anthropic-format tests, where the model
/// ids a client sends (`claude-sonnet-4-6`) are names no self-hosted backend
/// serves.
pub async fn state_with_alias(upstream_url: &str, alias: &str, real_model: &str) -> RamaState {
    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mut backend = mock_backend("mock", upstream_url);
    backend.alias = Some(upstreams::config::AliasSpec::Targets(HashMap::from([(
        alias.to_string(),
        real_model.to_string(),
    )])));
    let mut pools = HashMap::new();
    pools.insert(
        "pool".to_string(),
        UpstreamPoolConfig {
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![backend],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    seed_pool_models(&registry, "pool", 0, &[real_model]);
    state_from_registry(db_pool, registry)
}

/// Like [`state_with_pool`] for a `speech` pool, with both voice surfaces the
/// operator controls: `voices` is the language→voice map that *resolves* a
/// default (`("", "alloy")` being the catch-all entry), `offer` is the menu the
/// per-user picker shows. Backs the voice-picker tests.
pub async fn state_with_speech_voices(
    upstream_url: &str,
    model_name: &str,
    voices: &[(&str, &str)],
    offer: &[&str],
) -> RamaState {
    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mut pools = HashMap::new();
    pools.insert(
        "pool".to_string(),
        UpstreamPoolConfig {
            voices: voices
                .iter()
                .map(|(lang, voice)| ((*lang).to_string(), (*voice).to_string()))
                .collect(),
            offer_voices: offer.iter().map(|v| (*v).to_string()).collect(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Speech,
            strategy: PickerStrategy::RoundRobin,
            models: vec![],
            backend: vec![mock_backend("mock", upstream_url)],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    seed_pool_models(&registry, "pool", 0, &[model_name]);
    state_from_registry(db_pool, registry)
}

/// Env var names the S3 test config points at for its credentials. Real
/// deployments name their own; the values are irrelevant to a mock endpoint
/// (it never verifies the signature) but must be non-empty or the config
/// reports a missing credential.
pub const TEST_S3_ACCESS_KEY_ENV: &str = "GATEWAY_TEST_S3_ACCESS_KEY";
pub const TEST_S3_SECRET_KEY_ENV: &str = "GATEWAY_TEST_S3_SECRET_KEY";
pub const TEST_S3_BUCKET: &str = "test-bucket";
pub const TEST_S3_PREFIX: &str = "chat-attachments";

/// Build a `RamaState` with `[chat.s3]` pointed at `endpoint` — a wiremock
/// server standing in for the object store. Path-style addressing means the
/// object lands at `GET /<bucket>/<key_prefix>/<turn_id>/<filename>`, which
/// is what the attachment tests assert on.
pub async fn state_with_s3(endpoint: &str) -> RamaState {
    use gateway_core::server::config::S3Config;
    // SAFETY: single-threaded test setup, before any concurrent env reads.
    unsafe {
        std::env::set_var(TEST_S3_ACCESS_KEY_ENV, "test-access-key");
        std::env::set_var(TEST_S3_SECRET_KEY_ENV, "test-secret-key");
    }
    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let registry = upstreams::UpstreamRegistry::new(&HashMap::new()).unwrap();
    let mut config = Config::default();
    config.chat.s3 = Some(S3Config {
        endpoint: endpoint.to_string(),
        region: "us-east-1".into(),
        bucket: TEST_S3_BUCKET.into(),
        access_key: None,
        secret_key: None,
        // The legacy env-var indirection, still honoured — which is what this
        // fixture happens to exercise, since the harness sets these two.
        access_key_env: Some(TEST_S3_ACCESS_KEY_ENV.into()),
        secret_key_env: Some(TEST_S3_SECRET_KEY_ENV.into()),
        key_prefix: TEST_S3_PREFIX.into(),
    });
    let tools = Arc::new(ToolRegistry::new());
    let rbac = Arc::new(Resolver::empty());
    let app = AppState::new(config, db_pool.clone(), registry, tools, rbac)
        .with_tool_family_builder(gateway::tool_families::typst());
    let sessions = SessionStore::new(db_pool, TEST_SECRET);
    RamaState::new(
        app,
        sessions,
        gateway_core::server::usage::UsageHandle::disabled(),
    )
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
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
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
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
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
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![mock_backend("chat", "http://unused.invalid")],
        },
    );
    pools.insert(
        "embed".to_string(),
        UpstreamPoolConfig {
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
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
    state_with_admin_rbac_cfg(upstream_url, true).await
}

/// Same as [`state_with_admin_rbac`] but with a ComfyUI store wired in
/// (empty content dir from a tempdir). For the `/admin/comfyui` route
/// tests.
pub async fn state_with_admin_rbac_and_comfyui(upstream_url: &str) -> RamaState {
    use gateway_features::server::comfyui::{Client, ComfyuiStore};
    use gateway_runtime::server::comfyui_tool::ComfyuiHandle;
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();

    let mut pools = HashMap::new();
    pools.insert(
        "pool".to_string(),
        UpstreamPoolConfig {
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                alias: None,
                probe_models: true,
                supports_edit: false,
                name: "mock".into(),
                base_url: upstream_url.into(),
                api_key_env: None,
                api_key: None,
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
    use gateway_core::server::rbac::config::{RbacConfig, RoleConfig, RoleMapping};
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
        admin: true,
        models: vec!["*".into()],
        tools: vec!["*".into()],
        skills: vec![],
    };
    let rbac = Arc::new(Resolver::build(rbac_config, vec![admin_role]).unwrap());

    let mut config = Config::default();
    config.gateway.allow_impersonation = true;
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(ComfyuiStore::load(tmp.keep()));
    let client = Client::new("http://unused.invalid".to_string()).unwrap();
    let comfyui = Arc::new(ComfyuiHandle {
        store,
        client,
        runner_poll_interval: std::time::Duration::from_millis(10),
        runner_timeout: std::time::Duration::from_secs(5),
        s3: None,
        max_concurrent_jobs: 1,
        job_slots: Arc::new(tokio::sync::Semaphore::new(1)),
        chat_updates: gateway_features::server::comfyui::ChatUpdateRegistry::default(),
    });
    let app = AppState::new(config, pool.clone(), registry, tools, rbac)
        .with_comfyui(comfyui)
        .with_tool_family_builder(gateway::tool_families::typst());
    let sessions = SessionStore::new(pool, TEST_SECRET);
    RamaState::new(
        app,
        sessions,
        gateway_core::server::usage::UsageHandle::disabled(),
    )
}

/// Same as [`state_with_admin_rbac`] but with `[gateway].allow_impersonation`
/// turned off, for tests that exercise the impersonation kill switch.
pub async fn state_with_admin_rbac_no_impersonation(upstream_url: &str) -> RamaState {
    state_with_admin_rbac_cfg(upstream_url, false).await
}

async fn state_with_admin_rbac_cfg(upstream_url: &str, allow_impersonation: bool) -> RamaState {
    use gateway_core::server::rbac::config::{RbacConfig, RoleConfig, RoleMapping};
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();

    let mut pools = HashMap::new();
    pools.insert(
        "pool".to_string(),
        UpstreamPoolConfig {
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                alias: None,
                probe_models: true,
                supports_edit: false,
                name: "mock".into(),
                base_url: upstream_url.into(),
                api_key_env: None,
                api_key: None,
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
        admin: true,
        models: vec!["*".into()],
        tools: vec!["*".into()],
        skills: vec![],
    };
    let rbac = Arc::new(Resolver::build(rbac_config, vec![admin_role]).unwrap());

    let mut config = Config::default();
    config.gateway.allow_impersonation = allow_impersonation;
    let app = AppState::new(config, pool.clone(), registry, tools, rbac)
        .with_tool_family_builder(gateway::tool_families::typst());
    let sessions = SessionStore::new(pool, TEST_SECRET);
    RamaState::new(
        app,
        sessions,
        gateway_core::server::usage::UsageHandle::disabled(),
    )
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
        .into_iter()
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
    use gateway_core::server::db::users;
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
            speech_voice: None,
        },
    )
    .await
    .unwrap();
    let session = state.sessions.create(user_id).await.unwrap();
    state.sessions.sign(&session.id)
}

/// A session-authenticated form POST — the shape every datastar-driven page
/// action takes. Six test modules had grown their own identical copy.
pub fn post_form(uri: &str, cookie: &str, body: &str) -> rama::http::Request {
    rama::http::Request::builder()
        .method(rama::http::Method::POST)
        .uri(uri)
        .header("cookie", format!("id={cookie}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Seed a user + an active session + a bearer token. Returns the
/// plaintext bearer suitable for an `Authorization: Bearer …` header.
pub async fn seed_user_with_token(state: &RamaState, user_id: &str) -> String {
    seed_user_with_token_id(state, user_id).await.0
}

/// [`seed_user_with_token`], also handing back the token's id — needed by
/// anything that configures the token itself (model allowlist, per-token
/// quota) rather than just authenticating with it.
pub async fn seed_user_with_token_id(state: &RamaState, user_id: &str) -> (String, String) {
    use gateway_core::server::auth::token;
    use gateway_core::server::db::{tokens, users};
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
            speech_voice: None,
        },
    )
    .await
    .unwrap();
    let (plaintext, hash) = token::mint();
    let token_id = Uuid::new_v4().to_string();
    tokens::insert(
        &state.db,
        &tokens::Token {
            id: token_id.clone(),
            user_id: user_id.into(),
            name: "test".into(),
            hash,
            created_at: now,
            last_used_at: None,
            expires_at: now + SignedDuration::from_hours(1),
            revoked_at: None,
            // Test tokens have tool use on so the gateway-tool paths the
            // proxy/tool tests exercise are actually reachable.
            tools_enabled: true,
        },
    )
    .await
    .unwrap();
    (plaintext, token_id)
}
