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
use gateway::server::config::{FeedbackConfig, GatewayConfig, SkillsConfig};
use gateway::server::rbac::RoleConfig;
use gateway::server::rbac::{Resolver, config::RbacConfig, config::RoleMapping};
use gateway::server::skills::SkillStore;
use gateway::server::tools::{ToolRegistry, echo, fetch_url, read_skill, search_web, time};
use gateway::server::upstreams::{
    self,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use gateway::server::{AppState, Config, db};
use jiff::{Timestamp, ToSpan};
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
            // Two chat models so the feedback widget's "Text model" picker has
            // a real choice to render.
            "data": [
                { "id": "demo-model", "object": "model" },
                { "id": "demo-model-pro", "object": "model" },
            ],
        })))
        .mount(&chat_mock)
        .await;
    // Feedback field-extraction: matched before the generic non-streaming
    // mock (first-mounted wins on ties) via the unique system-prompt phrase,
    // so the voice→fields flow returns a valid structured JSON object the
    // dialog can drop into its fields.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::body_string_contains(
            "structured bug/feature report",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "demo-extract",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "{\"title\":\"Save button does nothing on the settings page\",\"description\":\"Clicking Save on the settings page shows no feedback and the change is lost after reload.\",\"business_value\":\"Users cannot persist their preferences, leading to repeated support tickets.\",\"acceptance_criteria\":\"- Clicking Save persists the change\\n- A success toast confirms the save\\n- The value survives a reload\",\"priority\":\"high\"}",
                },
                "finish_reason": "stop",
            }],
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
            // Two transcription models so the feedback widget's "Voice model"
            // picker has a real choice to render.
            "data": [
                { "id": "demo-whisper", "object": "model" },
                { "id": "demo-whisper-large", "object": "model" },
            ],
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
            voices: Default::default(),
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
                name: "wiremock-chat".into(),
                base_url: chat_mock.uri(),
                api_key_env: None,
                api_key: None,
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
            voices: Default::default(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Transcription,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                alias: None,
                probe_models: true,
                supports_edit: false,
                name: "wiremock-voice".into(),
                base_url: voice_mock.uri(),
                api_key_env: None,
                api_key: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    // Speech (TTS) pool — its mere presence flips `voice_available` on so the
    // chat composer renders the live-voice button (and modal). Points at the
    // chat mock's URL (never actually called just to render the button);
    // `probe_models: false` + an explicit pool model keeps it out of the
    // /models discovery path.
    pools.insert(
        "speech".to_string(),
        UpstreamPoolConfig {
            voices: Default::default(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Speech,
            strategy: PickerStrategy::RoundRobin,
            models: vec!["demo-tts".into()],
            backend: vec![BackendConfig {
                alias: None,
                probe_models: false,
                supports_edit: false,
                name: "wiremock-speech".into(),
                base_url: chat_mock.uri(),
                api_key_env: None,
                api_key: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: vec!["demo-tts".into()],
            }],
        },
    );
    // Image-generation pool — advertises an image model so the chat's
    // image-generation affordance is present. Static model id; no discovery.
    pools.insert(
        "image".to_string(),
        UpstreamPoolConfig {
            voices: Default::default(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Image,
            strategy: PickerStrategy::RoundRobin,
            models: vec!["demo-image".into()],
            backend: vec![BackendConfig {
                alias: None,
                probe_models: false,
                supports_edit: true,
                name: "wiremock-image".into(),
                base_url: chat_mock.uri(),
                api_key_env: None,
                api_key: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: vec!["demo-image".into()],
            }],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools)?;
    // The `/admin/upstreams` page reads topology from the DB (not the in-memory
    // registry), so seed the same pools into the DB. Health rows key off the
    // registry by backend name, which matches — so the seeded pools render as
    // live "up" backends. Backends here carry no API key, so the crypto instance
    // is never actually used to seal anything. Fresh in-memory DB every boot, so
    // this always runs (no seed marker like main.rs).
    let crypto = gateway::server::crypto::Crypto::from_env_or_session(&SESSION_SECRET);
    db::upstreams_config::seed_from_config(&pool, &pools, &Default::default(), &crypto).await?;
    // Run the initial probe round so each backend's `/models` set is
    // populated before we start serving requests. Without this, the
    // first chat-page render lands on empty dropdowns until the
    // looping probe catches up 5 s later.
    upstreams::health::spawn(registry.clone()).await;
    // Skills (for the /admin/skills screenshot + local debugging): load the
    // repo's `data/skills` bundles into a hot-reloadable store, grant the dev
    // role every skill, and register `read_skill`. Mirrors `main.rs`.
    //
    // A small, realistic role set so the operator pages look like a real
    // deployment: a privileged `admin`, a baseline `user` (the default role
    // every signed-in user gets), and a few team roles resolved from OIDC
    // groups. `admin` grants every tool + skill so the seed `dev` user (mapped
    // to it via the `platform-admins` group) can reach /admin/*, /rag, and the
    // skills content. Set on `config.roles` too (not just the Resolver) so the
    // skills page's "Granted to" column and the users page's role columns
    // resolve. The wiremock backend doesn't actually invoke tools — the
    // gateway-side path is what we want for playwright / local-browser work.
    let roles = vec![
        RoleConfig {
            id: "admin".into(),
            admin: true,
            models: vec!["*".into()],
            tools: vec!["*".into()],
            skills: vec!["*".into()],
        },
        RoleConfig {
            id: "user".into(),
            admin: false,
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
            id: "finance".into(),
            admin: false,
            models: vec!["demo-model".into()],
            tools: vec![],
            skills: vec![],
        },
        RoleConfig {
            id: "support".into(),
            admin: false,
            models: vec!["demo-model".into()],
            tools: vec!["search_web".into()],
            skills: vec![],
        },
    ];
    // Generic, non-croit demo skills shipped beside this example (the real
    // `data/skills` is gitignored local data) — keeps README screenshots clean.
    // Absolute, CARGO_MANIFEST_DIR-anchored so it resolves regardless of cwd.
    let skills_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/demo-skills");
    // OIDC→role mapping (seed-only, mirrors a real deployment). Every signed-in
    // user gets `user` as a baseline; team/admin roles come from their OIDC
    // groups. `dev` carries the `platform-admins` group → resolves to admin.
    let dev_rbac = RbacConfig {
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
            RoleMapping {
                oidc_claim: "groups".into(),
                oidc_value: "finance".into(),
                role: "finance".into(),
            },
            RoleMapping {
                oidc_claim: "groups".into(),
                oidc_value: "support".into(),
                role: "support".into(),
            },
        ],
    };
    let config = Config {
        skills: Some(SkillsConfig {
            dir: skills_dir.clone(),
        }),
        // Turn on impersonation so the /admin/users page renders its
        // Impersonate action column (audited in production; harmless here).
        // `bootstrap_admin_groups` mirrors production's break-glass admin so the
        // `dev` user stays admin even after an `/admin/groups` edit reloads the
        // resolver from the (DB-seeded) group tables.
        gateway: GatewayConfig {
            allow_impersonation: true,
            bootstrap_admin_groups: vec!["platform-admins".into()],
            ..Default::default()
        },
        rbac: dev_rbac.clone(),
        roles: roles.clone(),
        // Seed a feedback config so the floating feedback button + dialog
        // render in local UI debugging. The token is a dummy — recording,
        // transcription, model pickers, and field extraction all work against
        // the wiremock backend; only the final GitHub issue POST would fail
        // (which is fine for UI work). `extraction_model` left empty so the
        // picker defaults to the first chat model.
        feedback: Some(FeedbackConfig {
            github_owner: "demo-owner".into(),
            github_repo: "demo-repo".into(),
            github_token: Some("dev-ui-dummy-token".into()),
            github_token_env: None,
            labels: vec!["feedback".into()],
            assets_branch: "feedback-assets".into(),
            // Operator-chosen models (the form has no picker). Empty would also
            // work (first available); set them explicitly for a deterministic
            // demo against the wiremock pools.
            extraction_model: Some("demo-model".into()),
            voice_model: Some("demo-whisper".into()),
            github_api_base: "https://api.github.com".into(),
        }),
        ..Config::default()
    };
    // Seed the DB group tables from the config (mirrors main.rs first-boot
    // seeding), then build the resolver from the DB snapshot — so `/admin/groups`
    // shows the seeded groups and an edit + `reload_rbac` round-trips through the
    // same DB path production uses. `bootstrap_admin_groups` keeps `dev` admin.
    gateway::server::db::gateway_groups::seed_from_config(&pool, &dev_rbac, &config.roles)
        .await
        .expect("dev_ui RBAC seed");
    let group_snapshot = gateway::server::db::gateway_groups::load_snapshot(&pool)
        .await
        .expect("dev_ui load groups");
    let rbac = Arc::new(Resolver::from_snapshot(
        group_snapshot,
        config.gateway.bootstrap_admin_groups.clone(),
    ));
    if let Ok(grants) = gateway::server::db::skill_grants::all(&pool).await {
        rbac.set_skill_grant_overlay(grants);
    }
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
    // Enabled usage handle (90-day retention) so the /usage page renders real
    // aggregates instead of the "metrics disabled" banner. Spawn before the
    // pool is moved into the session store.
    let usage = gateway::server::usage::spawn(pool.clone(), 90);
    let sessions = SessionStore::new(pool, SESSION_SECRET);
    let state = RamaState::new(app, sessions, usage);

    // --- Seed a user + session so the authed UI is reachable ---------
    use gateway::server::db::users;
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: "dev".into(),
            email: "dev@example.com".into(),
            name: Some("Dev User".into()),
            // Maps to the admin role via the RBAC mapping above.
            roles: vec!["platform-admins".into()],
            created_at: now,
            updated_at: now,
            timezone: None,
        },
    )
    .await?;
    // A second, NON-admin user (OIDC group `engineering` → the `engineering`
    // gateway group, no admin flag) so RBAC enforcement can be exercised in the
    // browser: this user only sees pools / RAG / MCP their groups permit, with
    // no admin bypass.
    users::upsert(
        &state.db,
        &users::User {
            id: "eng".into(),
            email: "eng@example.com".into(),
            name: Some("Eng User".into()),
            roles: vec!["engineering".into()],
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
    let eng_session = state.sessions.create("eng").await?;
    let eng_cookie = state.sessions.sign(&eng_session.id);

    eprintln!("---------------------------------------------------------------");
    eprintln!(
        "dev gateway listening on http://{}",
        std::env::var("DEV_UI_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string())
    );
    eprintln!("authed pages: /, /tokens, /chat, /theme/toggle, /api/v0/*");
    eprintln!("seed cookie (paste into playwright / curl):");
    eprintln!("    id={cookie}");
    eprintln!("non-admin (engineering) seed cookie:");
    eprintln!("    eng_id={eng_cookie}");
    eprintln!("---------------------------------------------------------------");

    // rc1's `SocketAddress: FromStr` yields a boxed error that anyhow can't
    // absorb via `?`; stringify it. `DEV_UI_BIND` overrides the default bind
    // (e.g. to run alongside a real gateway already on :8080).
    let bind = std::env::var("DEV_UI_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let addr: SocketAddress = bind
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
    use session_core::attachments;
    use session_core::db::{self as chatdb, ToolCallStatus, TurnStatus};

    // --- An image-generation conversation (seeded FIRST so the gzip chat
    // below stays the most-recent session and `/chat` still lands on it).
    // The generated image is a self-contained SVG data URI carried via a
    // `gw-attachment` marker — the exact mechanism the image-gen tool uses,
    // minus S3 — so it renders inline like a real generated image.
    const DEMO_IMAGE_DATA_URI: &str = concat!(
        "data:image/svg+xml;base64,",
        include_str!("demo-genimg.b64")
    );
    let img = chatdb::create_session(&state.db, "dev").await?;
    chatdb::set_session_title(&state.db, &img.id, "Generate a hero image").await?;
    let iu = uuid::Uuid::new_v4().to_string();
    chatdb::create_user_turn(
        &state.db,
        &img.id,
        &iu,
        "Generate a wide hero image: a serene mountain lake at sunset, warm colors, digital art.",
    )
    .await?;
    let ia = uuid::Uuid::new_v4().to_string();
    chatdb::create_assistant_turn_in_progress(&state.db, &img.id, &ia, "demo-image").await?;
    let img_marker = attachments::marker_line(
        "mountain-lake-sunset.svg",
        "image/svg+xml",
        DEMO_IMAGE_DATA_URI,
        4096,
    );
    chatdb::append_content(
        &state.db,
        &ia,
        &format!("Here's your hero image — a serene mountain lake at sunset in a warm, painterly style.\n\n{img_marker}"),
    )
    .await?;
    chatdb::finalize_turn(&state.db, &ia, TurnStatus::Completed, None).await?;

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
                reuse_conversation: false,
                reuse_rounds: 5,
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
    // The global, audited Discord connector (shared bot) — configured + enabled
    // and given a few sample tool-call audit rows so /admin/connectors shows the
    // Global/Audited badges + "Audit log" button and the audit page renders
    // populated. Generic data only.
    sqlx::query("UPDATE mcp_catalog_connectors SET url = ? WHERE key = 'discord'")
        .bind("http://discord-mcp:8085/mcp")
        .execute(&state.db)
        .await?;
    mcp_catalog::set_enabled(&state.db, "discord", true).await?;
    {
        use gateway::server::db::mcp_audit;
        mcp_audit::record(
            &state.db,
            "dev",
            "discord",
            "mcp__discord__send_private_message",
            Some(
                r#"{"userId":"826733236931526666","message":"Standup reminder in 10 minutes 🕙"}"#,
            ),
            "ok",
            None,
            Some("chat-a1"),
        )
        .await?;
        mcp_audit::record(
            &state.db,
            "dev",
            "discord",
            "mcp__discord__send_message",
            Some(r#"{"channelId":"826729434073530409","message":"Deploy v1.4.2 finished ✅"}"#),
            "ok",
            None,
            Some("chat-a1"),
        )
        .await?;
        mcp_audit::record(
            &state.db,
            "dev",
            "discord",
            "mcp__discord__create_webhook",
            Some(r#"{"channelId":"826729434073530409","name":"ci-bot"}"#),
            "error",
            Some("Missing permission: MANAGE_WEBHOOKS"),
            Some("chat-b2"),
        )
        .await?;
    }

    // --- API tokens (for the /tokens screenshot) — a mix of active (one
    // recently used, one never) and a revoked one, all owned by `dev`. The
    // `hash` is a throwaway string: the page only lists tokens, it doesn't
    // authenticate with them. `created_at` is backdated; `touch`/`revoke`
    // stamp last-used / revoked-at to "now".
    use gateway::server::db::tokens;
    use gateway::server::db::users;
    let tnow = Timestamp::now();
    // (name, created N days ago, then: "used" | "unused" | "revoked")
    let seed_tokens = [
        ("Production API", 42i64, "used"),
        ("CI pipeline", 15i64, "revoked"),
        ("Local laptop", 4i64, "unused"),
    ];
    for (name, age_days, state_kind) in seed_tokens {
        let id = uuid::Uuid::new_v4().to_string();
        tokens::insert(
            &state.db,
            &tokens::Token {
                id: id.clone(),
                user_id: "dev".into(),
                name: name.into(),
                hash: format!("seed-hash-{id}"),
                created_at: tnow - (age_days * 24).hours(),
                last_used_at: None,
                expires_at: tnow + (90i64 * 24).hours(),
                revoked_at: None,
                tools_enabled: true,
            },
        )
        .await?;
        match state_kind {
            "used" => tokens::touch(&state.db, &id).await?,
            "revoked" => {
                tokens::revoke(&state.db, "dev", &id).await?;
            }
            _ => {}
        }
    }

    // --- Additional users (for /admin/users) with distinct OIDC groups so the
    // resolved gateway-role column varies (via the RBAC mappings in `main`).
    // Impersonation is enabled in config, so the action column populates.
    let unow = Timestamp::now();
    let seed_users = [
        (
            "u-anna",
            "anna.schmidt@example.com",
            "Anna Schmidt",
            vec!["engineering", "platform-admins"],
        ),
        (
            "u-ben",
            "ben.carter@example.com",
            "Ben Carter",
            vec!["engineering"],
        ),
        (
            "u-clara",
            "clara.novak@example.com",
            "Clara Novak",
            vec!["finance"],
        ),
        (
            "u-david",
            "david.kim@example.com",
            "David Kim",
            vec!["support"],
        ),
    ];
    for (id, email, name, groups) in seed_users {
        users::upsert(
            &state.db,
            &users::User {
                id: id.into(),
                email: email.into(),
                name: Some(name.into()),
                roles: groups.into_iter().map(String::from).collect(),
                created_at: unow,
                updated_at: unow,
                timezone: None,
            },
        )
        .await?;
    }

    // --- Usage events (for /usage). The page defaults to "Today" in the
    // viewer's timezone (dev = UTC), so we seed several of today's rows plus a
    // week of history across a couple of models, sources, and kinds — with a
    // few 4xx/5xx so the "errors" stat is non-zero. `insert_batch` writes both
    // the raw event and the daily rollup, so any period renders.
    use gateway::server::db::usage as usage_db;
    use usage_db::{UsageKind, UsageRecord, UsageSource};
    let mut usage_rows: Vec<UsageRecord> = Vec::new();
    // (hours-ago, source, kind, model, status, prompt, completion)
    let events: &[(i64, UsageSource, UsageKind, &str, u16, i64, i64)] = &[
        (
            1,
            UsageSource::Chat,
            UsageKind::Chat,
            "demo-model",
            200,
            820,
            240,
        ),
        (
            2,
            UsageSource::Chat,
            UsageKind::Chat,
            "demo-model-pro",
            200,
            1450,
            610,
        ),
        (
            3,
            UsageSource::V1Api,
            UsageKind::Chat,
            "demo-model",
            200,
            320,
            110,
        ),
        (
            4,
            UsageSource::Chat,
            UsageKind::Image,
            "demo-image",
            200,
            0,
            0,
        ),
        (
            5,
            UsageSource::V1Api,
            UsageKind::Chat,
            "demo-model-pro",
            429,
            0,
            0,
        ),
        (
            7,
            UsageSource::Scheduled,
            UsageKind::Chat,
            "demo-model",
            200,
            2100,
            540,
        ),
        (
            9,
            UsageSource::Chat,
            UsageKind::Transcription,
            "demo-whisper",
            200,
            0,
            0,
        ),
        (
            26,
            UsageSource::Chat,
            UsageKind::Chat,
            "demo-model",
            200,
            640,
            180,
        ),
        (
            28,
            UsageSource::V1Api,
            UsageKind::Chat,
            "demo-model",
            500,
            0,
            0,
        ),
        (
            50,
            UsageSource::Chat,
            UsageKind::Chat,
            "demo-model-pro",
            200,
            1720,
            690,
        ),
        (
            74,
            UsageSource::Scheduled,
            UsageKind::Chat,
            "demo-model",
            200,
            1980,
            500,
        ),
        (
            99,
            UsageSource::Chat,
            UsageKind::Speech,
            "demo-tts",
            200,
            0,
            0,
        ),
        (
            122,
            UsageSource::V1Api,
            UsageKind::Chat,
            "demo-model",
            200,
            410,
            150,
        ),
        (
            146,
            UsageSource::Chat,
            UsageKind::Chat,
            "demo-model-pro",
            200,
            1300,
            520,
        ),
    ];
    let unow2 = Timestamp::now();
    for (h, source, kind, model, status, prompt, completion) in events.iter().copied() {
        let backend = match kind {
            UsageKind::Transcription | UsageKind::Speech => "wiremock-voice",
            UsageKind::Image => "wiremock-image",
            _ => "wiremock-chat",
        };
        usage_rows.push(UsageRecord {
            created_at: unow2 - h.hours(),
            user_id: "dev".into(),
            user_email: Some("dev@example.com".into()),
            token_id: None,
            token_name: (source == UsageSource::V1Api).then(|| "Production API".to_string()),
            source,
            kind,
            backend: backend.into(),
            model: model.into(),
            status,
            duration_ms: 400 + (h % 7) * 130,
            prompt_tokens: (prompt > 0).then_some(prompt),
            completion_tokens: (completion > 0).then_some(completion),
            total_tokens: (prompt + completion > 0).then_some(prompt + completion),
            enforce_limits: true,
        });
    }
    // A couple of rows from other users so the admin "All users" view has more
    // than one row.
    for (uid, email, model, h) in [
        ("u-anna", "anna.schmidt@example.com", "demo-model-pro", 6i64),
        ("u-ben", "ben.carter@example.com", "demo-model", 20i64),
    ] {
        usage_rows.push(UsageRecord {
            created_at: unow2 - h.hours(),
            user_id: uid.into(),
            user_email: Some(email.into()),
            token_id: None,
            token_name: None,
            source: UsageSource::Chat,
            kind: UsageKind::Chat,
            backend: "wiremock-chat".into(),
            model: model.into(),
            status: 200,
            duration_ms: 620,
            prompt_tokens: Some(900),
            completion_tokens: Some(280),
            total_tokens: Some(1180),
            enforce_limits: true,
        });
    }
    // Price the two demo chat models BEFORE inserting usage, so the batched
    // writer computes a real `cost` on each seeded row (→ the /usage cost
    // column + the cost limit bar show non-zero spend).
    use gateway::server::db::model_defaults;
    model_defaults::set_pricing(&state.db, "demo-model", Some(0.5), Some(1.5)).await?;
    model_defaults::set_pricing(&state.db, "demo-model-pro", Some(3.0), Some(15.0)).await?;

    usage_db::insert_batch(&state.db, &usage_rows).await?;

    // A spread of demo limit rules so /admin/limits shows a populated table and
    // the /usage "Your limits" bars render (global rules apply to `dev`).
    use gateway::server::db::limits::{self, Dimension, SubjectType, Window};
    limits::upsert(
        &state.db,
        SubjectType::Global,
        "",
        None,
        Dimension::Requests,
        Window::Day,
        5_000.0,
    )
    .await?;
    limits::upsert(
        &state.db,
        SubjectType::Global,
        "",
        None,
        Dimension::Tokens,
        Window::Week,
        5_000_000.0,
    )
    .await?;
    limits::upsert(
        &state.db,
        SubjectType::Global,
        "",
        None,
        Dimension::Cost,
        Window::Month,
        50.0,
    )
    .await?;
    limits::upsert(
        &state.db,
        SubjectType::Global,
        "",
        Some("demo-model-pro"),
        Dimension::Tokens,
        Window::Week,
        1_000_000.0,
    )
    .await?;
    limits::upsert(
        &state.db,
        SubjectType::Role,
        "engineering",
        None,
        Dimension::Requests,
        Window::Hour,
        600.0,
    )
    .await?;

    Ok(())
}
