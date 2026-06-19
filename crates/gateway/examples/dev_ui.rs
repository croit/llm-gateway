// SPDX-License-Identifier: AGPL-3.0-only

// Copyright (C) 2026 croit GmbH

//! Dev / playwright harness: spins up the FULL rama gateway against an
//! in-memory SQLite, a wiremock OpenAI-style chat + transcription
//! backend, and a pre-seeded session — then listens on 127.0.0.1:8080.
//!
//! Every page is reachable here: `/`, `/login`, `/tokens`, `/chat`,
//! `/theme/toggle`, the `/api/v0/*` JSON routes — same code path as
//! production, the only thing faked is the upstream LLM and the OIDC
//! handoff. Use this for browser-driven debugging of anything on the
//! UI surface, not just the chat composer.
//!
//! Run with `cargo run --example dev_ui -p gateway` (or
//! `mise run dev-ui`). The example prints the signed session cookie
//! to stdout so playwright (or curl) can inject it:
//!
//! ```bash
//! curl --cookie "id=<the printed value>" http://localhost:8080/chat
//! ```
//!
//! Not part of any test target; not run by CI. Strictly a local-only
//! convenience.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gateway::rama_server::{RamaState, SessionStore, router};
use gateway::server::config::SkillsConfig;
use gateway::server::rbac::RoleConfig;
use gateway::server::rbac::{Resolver, config::RbacConfig};
use gateway::server::skills::SkillStore;
use gateway::server::tools::{ToolRegistry, echo, fetch_url, read_skill, search_web, time};
use gateway::server::upstreams::{
    self,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use gateway::server::{AppState, Config, db};
use jiff::Timestamp;
use rama::net::address::SocketAddress;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SESSION_SECRET: [u8; 32] = [9u8; 32];

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,gateway=debug".into()),
        )
        .init();

    // --- Wiremock upstreams ------------------------------------------
    //
    // Two mock servers: one for the chat pool, one for the
    // transcription pool. With the auto-discovery routing layer
    // (`upstreams::health` parses each backend's `/models` response
    // and routes by what it sees), sharing a single mock between
    // pools would have both pools advertise every model — the
    // transcription dropdown would show chat models and vice versa.
    // Splitting them keeps each pool's discovered set realistic.
    //
    //   chat mock: GET /models → `demo-model`
    //              POST /chat/completions → SSE stream
    //   voice mock: GET /models → `demo-whisper`
    //              POST /audio/transcriptions → JSON
    let chat_mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [{ "id": "demo-model", "object": "model" }],
        })))
        .mount(&chat_mock)
        .await;
    // Non-streaming response for the tool-loop branch — the runner
    // forces `stream:false` so it can inspect each round. Mounted
    // first so wiremock matches it before the streaming variant.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::body_string_contains("\"stream\":false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "demo",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hi! How can I help?",
                },
                "finish_reason": "stop",
            }],
        })))
        .mount(&chat_mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Hi! \"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"How can I help?\"}}]}\n\n",
                    "data: [DONE]\n\n",
                )),
        )
        .mount(&chat_mock)
        .await;

    let voice_mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [{ "id": "demo-whisper", "object": "model" }],
        })))
        .mount(&voice_mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "text": "dev transcription stub" })),
        )
        .mount(&voice_mock)
        .await;

    // --- RamaState (in-memory SQLite + chat + transcription pools) ---
    let pool = db::open(std::path::Path::new(":memory:")).await?;
    let mut pools = HashMap::new();
    pools.insert(
        "chat".to_string(),
        UpstreamPoolConfig {
            compliance: Default::default(),
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                name: "wiremock-chat".into(),
                base_url: chat_mock.uri(),
                api_key_env: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    pools.insert(
        "voice".to_string(),
        UpstreamPoolConfig {
            compliance: Default::default(),
            kind: PoolKind::Transcription,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                name: "wiremock-voice".into(),
                base_url: voice_mock.uri(),
                api_key_env: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools)?;
    // Run the initial probe round so each backend's `/models` set is
    // populated before we start serving requests. Without this, the
    // first chat-page render lands on empty dropdowns until the
    // looping probe catches up 5 s later.
    upstreams::health::spawn(registry.clone()).await;
    // Skills (for the /admin/skills screenshot + local debugging): load the
    // repo's `data/skills` bundles into a hot-reloadable store, grant the dev
    // role every skill, and register `read_skill`. Mirrors `main.rs`.
    //
    // A single `admin` role granting every tool + skill to the seed user, so
    // the operator/admin pages (/admin/*, /rag) are reachable and the skills
    // pages show content. Set on `config.roles` too (not just the Resolver) so
    // the skills page's "Granted to" column resolves it. The wiremock backend
    // doesn't actually invoke tools — the gateway-side path is what we want
    // for playwright / local-browser debugging.
    let roles = vec![RoleConfig {
        id: "admin".into(),
        models: vec!["*".into()],
        tools: vec!["*".into()],
        skills: vec!["*".into()],
    }];
    let config = Config {
        skills: Some(SkillsConfig {
            dir: PathBuf::from("data/skills"),
        }),
        roles: roles.clone(),
        ..Config::default()
    };
    let rbac = Arc::new(
        Resolver::build(
            RbacConfig {
                default_role: Some("admin".into()),
                mappings: vec![],
            },
            roles,
        )
        .expect("dev_ui RBAC build"),
    );
    let skill_store = Arc::new(SkillStore::load(PathBuf::from("data/skills")));
    let tools = Arc::new(
        ToolRegistry::new()
            .with(echo::Echo)
            .with(time::CurrentTimestamp)
            .with(fetch_url::FetchUrl)
            .with(search_web::SearchWeb)
            .with(read_skill::ReadSkill::new(
                skill_store.clone(),
                rbac.clone(),
            )),
    );
    let app = AppState::new(config, pool.clone(), registry, tools, rbac).with_skills(skill_store);
    let sessions = SessionStore::new(pool, SESSION_SECRET);
    let state = RamaState::new(app, sessions);

    // --- Seed a user + session so the authed UI is reachable ---------
    use gateway::server::db::users;
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: "dev".into(),
            email: "dev@example.com".into(),
            name: Some("Dev User".into()),
            roles: vec![],
            created_at: now,
            updated_at: now,
            timezone: None,
        },
    )
    .await?;
    let session = state.sessions.create("dev").await?;
    let cookie = state.sessions.sign(&session.id);

    eprintln!("---------------------------------------------------------------");
    eprintln!("dev gateway listening on http://127.0.0.1:8080");
    eprintln!("authed pages: /, /tokens, /chat, /theme/toggle, /api/v0/*");
    eprintln!("seed cookie (paste into playwright / curl):");
    eprintln!("    id={cookie}");
    eprintln!("---------------------------------------------------------------");

    // rc1's `SocketAddress: FromStr` yields a boxed error that anyhow can't
    // absorb via `?`; stringify it.
    let addr: SocketAddress = "127.0.0.1:8080"
        .parse()
        .map_err(|e| anyhow::anyhow!("parse bind address: {e}"))?;
    // `router::serve` binds the socket and listens until SIGINT / panic.
    router::serve(Arc::new(state), addr).await?;
    drop(chat_mock);
    drop(voice_mock);
    Ok(())
}
