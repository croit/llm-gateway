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
        admin: true,
        models: vec!["*".into()],
        tools: vec!["*".into()],
        skills: vec!["*".into()],
    }];
    // Generic, non-croit demo skills shipped beside this example (the real
    // `data/skills` is gitignored local data) — keeps README screenshots clean.
    // Absolute, CARGO_MANIFEST_DIR-anchored so it resolves regardless of cwd.
    let skills_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/demo-skills");
    let config = Config {
        skills: Some(SkillsConfig {
            dir: skills_dir.clone(),
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
    let skill_store = Arc::new(SkillStore::load(skills_dir));
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
    let state = RamaState::new(
        app,
        sessions,
        gateway::server::usage::UsageHandle::disabled(),
    );

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

    // --- Seed representative (non-croit) demo data so the README pages
    // render populated instead of empty "create your first…" states.
    seed_demo_data(&state).await?;

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

/// Seed a handful of realistic, **non-croit** rows so the README
/// screenshots show populated pages: one finished chat conversation, a few
/// scheduled actions, and two indexed RAG collections. All owned by the
/// `dev` user. In-memory DB, so this is rebuilt fresh on every launch.
async fn seed_demo_data(state: &RamaState) -> anyhow::Result<()> {
    use gateway::server::db::rag;
    use gateway::server::scheduled::{self, NewAction};
    use session_core::db::{self as chatdb, ToolCallStatus, TurnStatus};

    // --- A finished chat conversation showcasing the tool-call loop:
    // reasoning → web search → page fetch → a markdown answer with a source.
    const REASONING: &str = "This is a configuration question with a canonical \
        answer in the official nginx module docs, so I'll search for the \
        ngx_http_gzip_module page and quote the key directives rather than rely \
        on memory.";
    const ANSWER_MD: &str = "Here's a minimal gzip setup for nginx:\n\n\
        ```nginx\n\
        gzip on;\n\
        gzip_types text/plain text/css application/json application/javascript;\n\
        gzip_min_length 1024;\n\
        gzip_comp_level 5;\n\
        ```\n\n\
        - `gzip on;` turns compression on.\n\
        - `gzip_types` lists the MIME types to compress (HTML is always included).\n\
        - `gzip_min_length` skips tiny responses where compression isn't worth the CPU.\n\n\
        **Source:** [nginx — ngx_http_gzip_module](https://nginx.org/en/docs/http/ngx_http_gzip_module.html)";
    let s = chatdb::create_session(&state.db, "dev").await?;
    chatdb::set_session_title(&state.db, &s.id, "Enabling gzip in nginx").await?;
    let u = uuid::Uuid::new_v4().to_string();
    chatdb::create_user_turn(
        &state.db,
        &s.id,
        &u,
        "How do I turn on gzip compression in nginx? Give me a minimal config and cite the official docs.",
    )
    .await?;
    let a = uuid::Uuid::new_v4().to_string();
    chatdb::create_assistant_turn_in_progress(&state.db, &s.id, &a, "demo-model").await?;
    chatdb::append_reasoning(&state.db, &a, REASONING).await?;
    chatdb::set_reasoning_elapsed(&state.db, &a, 1400).await?;
    // Tool call 1 — web search.
    chatdb::insert_running_tool_call(
        &state.db,
        &a,
        "call_search",
        "search_web",
        r#"{"query":"nginx ngx_http_gzip_module enable gzip directives"}"#,
    )
    .await?;
    chatdb::complete_tool_call(
        &state.db,
        "call_search",
        r#"{"results":[{"title":"Module ngx_http_gzip_module","url":"https://nginx.org/en/docs/http/ngx_http_gzip_module.html","snippet":"A filter that compresses responses with the gzip method. Directives: gzip, gzip_types, gzip_min_length, gzip_comp_level."}]}"#,
        ToolCallStatus::Completed,
    )
    .await?;
    // Tool call 2 — fetch the doc page.
    chatdb::insert_running_tool_call(
        &state.db,
        &a,
        "call_fetch",
        "fetch_url",
        r#"{"url":"https://nginx.org/en/docs/http/ngx_http_gzip_module.html"}"#,
    )
    .await?;
    chatdb::complete_tool_call(
        &state.db,
        "call_fetch",
        r#"{"url":"https://nginx.org/en/docs/http/ngx_http_gzip_module.html","text":"Syntax: gzip on | off; Default: gzip off; Context: http, server, location. Enables or disables gzipping of responses. gzip_types, gzip_min_length and gzip_comp_level tune which responses are compressed and how hard."}"#,
        ToolCallStatus::Completed,
    )
    .await?;
    chatdb::append_content(&state.db, &a, ANSWER_MD).await?;
    chatdb::finalize_turn(&state.db, &a, TurnStatus::Completed, None).await?;

    // --- Scheduled actions ----------------------------------------------
    let schedules = [
        (
            "Daily standup digest",
            "Summarize yesterday's merged PRs and open blockers into a short standup digest.",
            "0 8 * * 1-5",
            "2026-06-22T08:00:00Z",
        ),
        (
            "Weekly dependency report",
            "List dependencies with new releases this week and flag any security advisories.",
            "0 9 * * 1",
            "2026-06-22T09:00:00Z",
        ),
        (
            "Monthly cost summary",
            "Summarize this month's API usage and token spend, with the three biggest line items.",
            "0 7 1 * *",
            "2026-07-01T07:00:00Z",
        ),
    ];
    for (name, prompt, cron, next) in schedules {
        scheduled::create(
            &state.db,
            NewAction {
                user_id: "dev".into(),
                name: name.into(),
                prompt: prompt.into(),
                model: "demo-model".into(),
                cron: cron.into(),
                timezone: "Europe/Berlin".into(),
                tools_enabled: true,
                next_run_at: Some(next.parse()?),
            },
        )
        .await?;
    }

    // --- RAG collections (indexed → "ready", with a resolved commit) -----
    let collections = [
        (
            "acme-docs",
            "Product documentation for the Acme platform",
            "https://github.com/acme/docs.git",
            "main",
            "a1b2c3d",
        ),
        (
            "acme-api",
            "Backend API service — handlers, models, and OpenAPI specs",
            "https://github.com/acme/api.git",
            "release-2.4",
            "9f4e210",
        ),
    ];
    for (name, desc, git_url, git_ref, commit) in collections {
        let c = rag::create_collection(
            &state.db,
            &rag::NewCollection {
                name: name.into(),
                description: Some(desc.into()),
                git_url: git_url.into(),
                git_ref: git_ref.into(),
                pat: None,
                embedding_model: "demo-embed".into(),
                include_globs: vec!["**/*.md".into(), "**/*.rs".into()],
                exclude_globs: vec!["target/**".into(), "node_modules/**".into()],
                chunk_size: 800,
                chunk_overlap: 100,
                search_mode: rag::SearchMode::Versioned,
            },
        )
        .await?;
        rag::mark_indexed(&state.db, c.id, commit).await?;
        let r = rag::add_ref(&state.db, c.id, git_ref, None, true).await?;
        rag::set_ref_status(&state.db, r.id, rag::CollectionStatus::Indexing).await?;
        rag::swap_ref_index(&state.db, r.id, &uuid::Uuid::new_v4().to_string(), commit).await?;
    }

    // --- MCP connector catalog (for the /admin/connectors + /integrations
    // screenshots). Seed the built-in set, give the deployment-specific
    // connectors generic example.com URLs + a demo client id, then enable them
    // so both the admin store and the user connect surface render populated.
    // No real endpoints, credentials, or connections — nothing user-specific.
    use gateway::server::db::mcp_catalog;
    mcp_catalog::seed_defaults(&state.db).await?;
    sqlx::query("UPDATE mcp_catalog_connectors SET url = ? WHERE key = ?")
        .bind("https://gworkspace-mcp.example.com/mcp")
        .bind("google_workspace")
        .execute(&state.db)
        .await?;
    sqlx::query("UPDATE mcp_catalog_connectors SET url = ? WHERE key = ?")
        .bind("https://gitlab-mcp.example.com/mcp")
        .bind("gitlab_selfmanaged")
        .execute(&state.db)
        .await?;
    sqlx::query(
        "UPDATE mcp_catalog_connectors SET client_id = 'demo-client-id' WHERE key = 'github'",
    )
    .execute(&state.db)
    .await?;
    for key in [
        "atlassian",
        "github",
        "gitlab",
        "gitlab_selfmanaged",
        "google_workspace",
    ] {
        mcp_catalog::set_enabled(&state.db, key, true).await?;
    }

    Ok(())
}
