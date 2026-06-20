// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Gateway entry point. Boots config + db + upstream registry + session
//! store + (optional) OIDC client, then hands the assembled state to the
//! rama server in `gateway::rama_server`.
//!
//! Run:
//! ```sh
//! cargo run --bin gateway --release
//! ```

use std::sync::Arc;

use anyhow::Context as _;
use rama::net::address::SocketAddress;

use gateway::rama_server::SessionStore;
use gateway::server::{self as srv, AppState, Config};
// `Tool` brings the `.id()` method into scope for the MCP-tool registration
// loop below; the built-in registrations only use `ToolRegistry::with`.
use gateway::server::tools::Tool;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,gateway=info")),
        )
        .init();

    let config = Config::load().context("loading gateway config")?;
    for (name, pool) in &config.upstream_pools {
        tracing::info!(
            pool = %name, kind = ?pool.kind, strategy = ?pool.strategy,
            backends = pool.backend.len(),
            "upstream pool configured"
        );
    }
    let db_path = config.db.path.clone();

    let db = srv::db::open(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("opening database: {e:#}"))?;

    let upstreams = srv::upstreams::UpstreamRegistry::new(&config.upstream_pools)
        .map_err(|e| anyhow::anyhow!("building upstream registry: {e}"))?;
    // `spawn` does an initial parallel probe round before returning, so
    // the first request lands on a registry that already knows which
    // model lives where. Worst case: every backend is unreachable, in
    // which case we wait the 2 s probe timeout and start serving with
    // empty model sets — the looping probe will populate them once the
    // backends come up. (It builds its own no-idle-pool probe client.)
    srv::upstreams::health::spawn(upstreams.clone()).await;
    // Positive liveness heartbeat (one line every 15s) so quiet logs can't be
    // mistaken for a hung process — see `spawn_heartbeat`.
    srv::upstreams::health::spawn_heartbeat(upstreams.clone());

    // Build the RBAC resolver up front: the `read_skill` tool holds a clone
    // so it can authorize skill access at call time, the same way the rest of
    // the gateway resolves roles → grants.
    let rbac = srv::rbac::Resolver::build(config.rbac.clone(), config.roles.clone())
        .map_err(|e| anyhow::anyhow!("building RBAC resolver: {e}"))?;
    let rbac = Arc::new(rbac);

    let mut tool_registry = srv::tools::ToolRegistry::new()
        .with(srv::tools::echo::Echo)
        .with(srv::tools::time::CurrentTimestamp)
        .with(srv::tools::fetch_url::FetchUrl)
        .with(srv::tools::fetch_attachment::FetchAttachment)
        .with(srv::tools::upload_attachment::UploadAttachment)
        .with(srv::tools::search_web::SearchWeb)
        .with(srv::tools::location::GetUserLocation)
        .with(srv::tools::memory::Remember)
        .with(srv::tools::memory::Recall)
        // Read-only public-data lookups — no secrets, no writes, safe to
        // leave always-on.
        .with(srv::tools::netcheck::DnsLookup)
        .with(srv::tools::netcheck::WhoisLookup)
        .with(srv::tools::netcheck::TlsCert)
        .with(srv::tools::wikipedia::Wikipedia)
        .with(srv::tools::currency::ConvertCurrency)
        // RAG. These tools are no-ops without the indexer wired into
        // AppState; registering them unconditionally keeps RBAC config
        // stable across deployments where `[rag]` is only sometimes set.
        .with(srv::tools::rag::RagListCollections)
        .with(srv::tools::rag::RagSearch);
    // `lookup_ip` is GeoIP-only — unlike `get_user_location` (which also has
    // the browser-GPS path), it can do nothing without a database. Register
    // it only when `[geoip]` is configured, so the model is never offered a
    // tool that could only ever answer "not available". A configured-but-
    // not-yet-loaded file is fine: the handle hot-reloads (see below) and the
    // tool's own runtime guard returns a clean `known:false` in the gap.
    if config.geoip.is_some() {
        tool_registry = tool_registry.with(srv::tools::lookup_ip::LookupIp);
        tracing::info!(tool = "lookup_ip", "registered GeoIP lookup tool");
    } else {
        tracing::info!("no [geoip] config — lookup_ip tool not registered");
    }
    if let Some(typst_cfg) = config.typst.as_ref() {
        // Discover one tool per template directory. Failures here
        // are warnings, not errors: a broken templates_dir
        // shouldn't keep the gateway from booting. The static tool
        // surface above is still available.
        match srv::typst::discover_templates(&typst_cfg.templates_dir) {
            Ok(templates) => {
                tracing::info!(
                    dir = %typst_cfg.templates_dir.display(),
                    count = templates.len(),
                    "discovered typst templates"
                );
                for t in templates {
                    let tool_id = format!("typst_{}", t.id);
                    tool_registry =
                        tool_registry.with(srv::tools::typst_render::TypstRenderTool::new(t));
                    tracing::info!(tool = %tool_id, "registered typst tool");
                }
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    dir = %typst_cfg.templates_dir.display(),
                    "skipping typst tools — discovery failed",
                );
            }
        }
    }
    // Bridge any configured MCP servers: connect, enumerate their tools, and
    // register each as a `Tool` alongside the built-ins. Non-fatal — a server
    // that's down at boot is logged and skipped (its tools just won't be
    // available this run); see `srv::tools::mcp`.
    if let Some(mcp_cfg) = config.mcp.as_ref() {
        for tool in srv::tools::mcp::connect_all(mcp_cfg).await {
            let id = tool.id();
            // Defend against an id collision (two servers, or a server vs a
            // built-in) by skipping rather than panicking in `with`.
            if tool_registry.contains(id) {
                tracing::warn!(tool = id, "MCP tool id already registered — skipping");
                continue;
            }
            tracing::info!(tool = id, "registered MCP tool");
            tool_registry = tool_registry.with(tool);
        }
    }
    // Code-execution sandbox. Registered only when `[sandbox]` points at a
    // reachable sandbox-runner; the three tools share one HTTP client.
    match config.sandbox.as_ref() {
        Some(sandbox_cfg) if sandbox_cfg.enabled => {
            let client = srv::tools::sandbox::SandboxClient::new(
                Arc::new(sandbox_cfg.clone()),
                config.gateway.public_url.clone(),
            );
            tool_registry = tool_registry
                .with(srv::tools::sandbox::RunInSandbox(client.clone()))
                .with(srv::tools::sandbox::GenerateDocument(client.clone()))
                .with(srv::tools::sandbox::CaptureWebpage(client))
                .with(srv::tools::sandbox::ReadSandboxOutput);
            tracing::info!(runner = %sandbox_cfg.runner_url, "registered sandbox tools");
        }
        Some(_) => tracing::info!("[sandbox] enabled = false — sandbox tools not registered"),
        None => tracing::info!("no [sandbox] config — sandbox tools not registered"),
    }
    // `enable_tools` is registered last so its catalog snapshot covers every
    // other tool (static + typst + MCP). It's part of the always-on core so
    // the model can always reach it; calling it writes per-conversation rows
    // that the next round's `allowed_tools_for_session` picks up.
    let enable_tools = srv::tools::enable_tools::EnableTools::from_registry(&tool_registry);
    tool_registry = tool_registry.with(enable_tools);

    // Agent Skills: a hot-reloadable store over `[skills] dir` (admin upload /
    // delete re-scan and swap it live — no restart). Registered *after*
    // `enable_tools` (so the loader isn't itself an enableable group — it's
    // always-on when the caller has a permitted skill; see
    // `AppState::allowed_tools_for_session`). When `[skills]` is configured we
    // register `read_skill` even if the dir is currently empty, so an upload
    // works without a restart; skill-less deployments (no `[skills]` block)
    // keep the exact same tool surface.
    let skill_store = config.skills.as_ref().map(|skills_cfg| {
        let store = srv::skills::SkillStore::load(skills_cfg.dir.clone());
        tracing::info!(
            dir = %skills_cfg.dir.display(),
            count = store.current().len(),
            "loaded skills store"
        );
        Arc::new(store)
    });
    if let Some(store) = skill_store.as_ref() {
        tool_registry = tool_registry.with(srv::tools::read_skill::ReadSkill::new(
            store.clone(),
            rbac.clone(),
        ));
        tracing::info!(tool = "read_skill", "registered skills tool");
    }
    let tools = Arc::new(tool_registry);

    // Session HMAC key, read from $GATEWAY_SESSION_KEY — a single 64-hex
    // value (32 bytes) so an operator only has to configure one knob.
    let session_secret = load_session_secret(&state_session_key())?;
    let sessions = SessionStore::new(db.clone(), session_secret);

    // Retry OIDC discovery so transient failures don't crash the
    // gateway. After exhaustion we boot
    // anyway with `state.oidc = None`; /auth/* returns a clean 500 then.
    let oidc = build_oidc_with_retry(&config).await;

    let mut state = AppState::new(config, db, upstreams, tools, rbac);
    if let Some(client) = oidc {
        state = state.with_oidc(client);
    }
    if let Some(store) = skill_store {
        state = state.with_skills(store);
    }

    // GeoIP (client-IP → coarse location) for the `get_user_location`
    // tool. Optional: with no `[geoip]` block we skip it entirely. A
    // missing DB file is fine — the handle loads lazily, hot-reloads when
    // a file appears, and the (token-gated) weekly updater is a no-op
    // without a token. So this never blocks boot or fails the gateway.
    if let Some(geoip_cfg) = state.config.geoip.clone() {
        let geo = srv::geoip::GeoIp::new(geoip_cfg.db_path.clone());
        geo.watch();
        srv::geoip::update::spawn(geoip_cfg.db_path.clone(), geoip_cfg.update_token());
        state = state.with_geoip(geo);
    }

    // RAG indexer — always wired in. The DB-backed collection registry
    // starts empty, so deployments that don't use RAG just have a quiet
    // poller running every 30s. Operators add collections via the admin
    // API; the worker picks them up.
    //
    // `[rag] data_dir` MUST resolve to a writable path. The container
    // image runs with a read-only rootfs (see deploy/quadlet/gateway.container),
    // so the default `data/rag` works for local dev only — operators
    // point this at a subdirectory of the named volume.
    let rag_config = state.config.rag.clone().unwrap_or_default();
    let indexer_config = srv::rag::worker::IndexerConfig {
        data_dir: rag_config.data_dir,
        ..srv::rag::worker::IndexerConfig::default()
    };
    let indexer = srv::rag::worker::Indexer::new(
        state.db.clone(),
        state.upstreams.clone(),
        state.http.clone(),
        indexer_config,
    );
    // Re-queue any ref left mid-build by a previous crash/restart and reap
    // orphaned build folders before the loop starts handling new work.
    indexer.recover_on_startup().await;
    srv::rag::worker::spawn(indexer.clone());
    state = state.with_indexer(indexer);

    // Usage metrics: a batched background writer + retention-prune task,
    // fronted by a fire-and-forget handle on the shared state. When
    // `[usage] enabled = false` the handle is a no-op and no tasks spawn.
    let usage = if state.config.usage.enabled {
        srv::usage::spawn(state.db.clone(), state.config.usage.retention_days)
    } else {
        tracing::info!("usage metrics disabled via [usage].enabled = false");
        srv::usage::UsageHandle::disabled()
    };

    let state = Arc::new(gateway::rama_server::RamaState::new(state, sessions, usage));

    // Scheduled actions: start the background loop that fires due actions
    // (the `scheduled_actions` table is created by migration 0021).
    srv::scheduled::worker::spawn(state.clone());

    let ip: std::net::IpAddr = std::env::var("IP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| "127.0.0.1".parse().unwrap());
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddress::new(ip, port);
    tracing::info!(%ip, port, "rama gateway starting (spike)");

    gateway::rama_server::router::serve(state, addr).await
}

/// Picks up the session HMAC secret from `$GATEWAY_SESSION_KEY` —
/// 64 hex chars (32 bytes). Falls
/// back to an ephemeral random key with a warning in dev so the
/// gateway boots without a configured key.
fn state_session_key() -> String {
    std::env::var("GATEWAY_SESSION_KEY").unwrap_or_default()
}

fn load_session_secret(raw: &str) -> anyhow::Result<[u8; 32]> {
    if raw.is_empty() {
        tracing::warn!(
            "GATEWAY_SESSION_KEY unset — using an ephemeral random key; \
             all existing sessions will be invalidated on restart"
        );
        use rand::TryRngCore;
        let mut buf = [0u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut buf)
            .map_err(|e| anyhow::anyhow!("OsRng fill: {e}"))?;
        return Ok(buf);
    }
    let bytes = hex_decode(raw)
        .ok_or_else(|| anyhow::anyhow!("GATEWAY_SESSION_KEY must be 64 hex chars (32 bytes)"))?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "GATEWAY_SESSION_KEY decoded to {} bytes, expected 32",
            bytes.len()
        );
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let nib = |c: u8| -> Option<u8> {
        Some(match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        })
    };
    for chunk in bytes.chunks(2) {
        out.push((nib(chunk[0])? << 4) | nib(chunk[1])?);
    }
    Some(out)
}

/// OIDC discovery retry loop: 5 attempts with
/// exponential backoff (500ms → 8s), then give up and boot without
/// OIDC. `/auth/*` returns a clean 500 in that state, so operators can
/// fix the config and `systemctl restart` rather than babysit a crash
/// loop on a transient network blip.
async fn build_oidc_with_retry(
    config: &Config,
) -> Option<Arc<gateway::server::auth::oidc::OidcClient>> {
    let oidc_config = config.oidc.as_ref()?;
    let mut attempt = 0;
    let max_attempts = 5;
    loop {
        attempt += 1;
        match gateway::server::auth::oidc::OidcClient::build(
            oidc_config,
            &config.gateway.public_url,
        )
        .await
        {
            Ok(client) => return Some(client),
            Err(err) if attempt < max_attempts => {
                let backoff = std::time::Duration::from_millis(500u64 * (1 << (attempt - 1)));
                tracing::warn!(
                    attempt, max_attempts,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %err,
                    "OIDC discovery failed; retrying",
                );
                tokio::time::sleep(backoff).await;
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "OIDC discovery failed after {max_attempts} attempts; \
                     starting without OIDC — /auth/login + /auth/callback \
                     will return 500 until this is resolved",
                );
                return None;
            }
        }
    }
}
