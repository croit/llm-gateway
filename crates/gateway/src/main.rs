// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Gateway entry point. Boots config + db + upstream registry + session
//! store + (optional) OIDC client, then hands the assembled state to the
//! rama server in `gateway::rama_server`.
//!
//! Run it with `mise run dev`, which supplies the mandatory
//! `GATEWAY_SESSION_KEY` (see [`load_session_secret`] — the gateway refuses to
//! boot without one, so a bare `cargo run` aborts).

use std::sync::Arc;

use anyhow::Context as _;
use rama::net::address::SocketAddress;

use gateway::rama_server::SessionStore;
use gateway_core::server::{self as srv, Config};
use gateway_features::server as feat;
use gateway_runtime::AppState;
use gateway_runtime::server as rt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,gateway=info")),
        )
        .init();

    // `mut` because the operator settings stored in the database are applied
    // onto it below, before anything is built from it.
    let mut config = Config::load().context("loading gateway config")?;

    // Subcommands. Deliberately explicit about the else branch: a typo like
    // `restore-setpu` must say so, not silently start a production server —
    // whoever types it is locked out and would otherwise watch the binary
    // appear to hang.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => {}
        // The break-glass path. Runs against the same database the server is
        // using (SQLite in WAL mode takes a second writer for one row), so the
        // operator does not have to stop anything, and the running gateway
        // picks the change up on its next `/setup` request.
        ["restore-setup"] => return restore_setup(&config).await,
        ["-h" | "--help"] => {
            println!("{USAGE}");
            return Ok(());
        }
        _ => anyhow::bail!("unrecognised arguments: {}\n\n{USAGE}", args.join(" ")),
    }

    for (name, pool) in &config.upstream_pools {
        tracing::info!(
            pool = %name, kind = ?pool.kind, strategy = ?pool.strategy,
            backends = pool.backend.len(),
            "upstream pool configured"
        );
    }
    config.warn_about_ignored_blocks();
    let db_path = config.db_path()?;
    refuse_to_orphan_an_existing_database(&db_path)?;
    tracing::info!(path = %db_path.display(), "database");

    let db = srv::db::open(&db_path)
        .await
        .map_err(|e| anyhow::anyhow!("opening database: {e:#}"))?;

    // Session HMAC key + at-rest crypto, built up front (before topology seeding
    // and the registry build): the first-boot seed seals backend API keys into
    // the DB, and building the registry unseals them. `session_secret` is reused
    // for the SessionStore below.
    let session_secret = load_session_secret(&state_session_key())?;
    let crypto = std::sync::Arc::new(srv::crypto::Crypto::from_env_or_session(&session_secret));

    // Setup state, resolved before anything that needs the public URL. A config
    // file's `[oidc]` block is imported into the DB once — that is what carries
    // an existing deployment across this release without an operator touching
    // anything — and from then on the DB is the only source of truth, edited
    // through the setup wizard.
    match srv::setup::import_config_once(
        &db,
        &crypto,
        config.oidc.as_ref(),
        config.public_url_fallback(),
    )
    .await
    {
        Ok(true) => tracing::info!(
            "imported the config file's [oidc] provider into the database; it is managed \
             at /setup from now on and the config block is ignored"
        ),
        Ok(false) => {}
        Err(err) => tracing::warn!(error = %err, "importing OIDC settings from the config file"),
    }

    // The operator settings — `[chat]`, `[sandbox]`, `[comfyui]`, `[rag]`,
    // `[skills]`, `[typst]`, `[geoip]`, `[usage]`, `[limits]`, `[feedback]`,
    // `[push]`, and the session/token half of `[gateway]`. Same one-way move as
    // the topology and the OIDC provider above: the file's values are copied
    // into the database once, and from then on `/admin/settings` owns them.
    // `[gateway].bootstrap_admin_groups` and `public_url` are the two keys that
    // stay behind — see `settings::GATEWAY_KEYS_STAYING_IN_THE_FILE`.
    //
    // This runs BEFORE anything is built out of `config` — the sandbox and
    // ComfyUI clients, the skills store, the Typst templates, the GeoIP
    // database, the RAG indexer are all constructed further down from these
    // very blocks, and each of them must see what the database says, not what
    // the file did.
    match srv::settings::import_once(&db, &crypto, &config).await {
        // Distinguished because the two mean different things to whoever is
        // reading the log: one is an upgrade that moved their settings, the
        // other is a fresh install with nothing to move.
        Ok(true) if config.loaded_from.is_some() => tracing::info!(
            "imported the config file's operator settings into the database; they are \
             managed at /admin/settings from now on and those config blocks are ignored"
        ),
        Ok(true) => tracing::info!(
            "no config file, so the operator settings start at their defaults; they are \
             managed at /admin/settings"
        ),
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(error = %err, "importing operator settings from the config file")
        }
    }
    // The process coming back *is* the restart every `restart`-flagged save was
    // waiting for, so the banner clears itself here rather than needing anyone
    // to dismiss it.
    if let Err(err) = srv::settings::clear_restart_pending(&db).await {
        tracing::warn!(error = %err, "clearing the pending-restart marker");
    }
    match srv::settings::load(&db, &crypto).await {
        Ok(stored) => srv::settings::apply(&stored, &mut config),
        // Booting on the file's values would quietly undo every change an
        // operator has made since, so say it loudly rather than looking fine.
        Err(err) => tracing::error!(
            error = %err,
            "could not read operator settings from the database; falling back to the config \
             file's values for this boot"
        ),
    }
    // The one handle everything shares. `reload_runtime` further down fills it
    // in from the database; anything built before that (the sandbox client)
    // holds this same Arc and therefore sees the wizard's URL the moment it
    // lands. Nothing downstream may read the config-file value directly — on a
    // wizard-configured deployment that is still the localhost fallback, which
    // is why the field is named `public_url_import_only`.
    let runtime =
        gateway_runtime::server::state::RuntimeSettings::new_handle(config.public_url_fallback());

    // On first boot (or after migrating from config-managed topology), seed
    // the DB from config.toml. After that the DB is the source of truth and
    // the TOML sections are ignored — admins manage topology via the UI.
    //
    // Gated on a persistent marker, NOT on the pool table being empty: once an
    // admin has (re)configured topology through the UI — including deleting
    // every pool to start over — we must not resurrect the config.toml pools on
    // the next restart. The marker is set exactly once, after the first boot's
    // seed succeeds. A seed failure is fatal (aborts boot) rather than logged
    // and forgotten, so we never start serving a half-seeded topology; upserts
    // are idempotent, so the next boot retries cleanly.
    const SEED_MARKER: &str = "topology.seeded";
    let already_seeded = srv::db::app_settings::get(&db, SEED_MARKER)
        .await
        .map_err(|e| anyhow::anyhow!("reading topology seed marker: {e:#}"))?
        .is_some();
    if !already_seeded {
        if !config.upstream_pools.is_empty() {
            tracing::info!("first boot: seeding upstream topology from config.toml to DB");
            srv::db::upstreams_config::seed_from_config(
                &db,
                &config.upstream_pools,
                &config.fallback,
                &crypto,
            )
            .await
            .map_err(|e| anyhow::anyhow!("seeding upstream topology from config.toml: {e:#}"))?;
        }
        srv::db::app_settings::set(&db, SEED_MARKER, "1")
            .await
            .map_err(|e| anyhow::anyhow!("recording topology seed marker: {e:#}"))?;
    }

    // Web-search settings used to be env-only. Take over whatever the
    // environment still carries into any setting that's still empty, then
    // treat the DB as the only source of truth (the import logs which vars it
    // took, and which it ignored because the DB already had a value). Non-fatal:
    // a failure here leaves search unconfigured, which the tool reports
    // cleanly — it must not stop the gateway from booting.
    match feat::search_settings::import_env_once(&db, &crypto).await {
        Ok(vars) if !vars.is_empty() => tracing::info!(
            imported = ?vars,
            "migrated web-search settings from the environment into the database; \
             configure them at /admin/models from now on"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(
            error = %e,
            "could not migrate web-search settings from the environment"
        ),
    }

    // Build the registry from the DB snapshot (falls back to empty if the DB
    // has no pools — e.g. a fresh install the admin hasn't configured yet).
    let snapshot = srv::db::upstreams_config::load_snapshot(&db)
        .await
        .map_err(|e| anyhow::anyhow!("loading upstream topology from DB: {e:#}"))?;
    let upstreams = srv::upstreams::UpstreamRegistry::from_snapshot(&snapshot, &crypto)
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

    // Seed the RBAC group tables from the legacy `[rbac]` + `[[roles]]` config
    // on first boot, then treat the DB as the source of truth — same
    // marker-gated pattern as the upstream topology above, so an admin who
    // later deletes every group in the UI doesn't get the config resurrected on
    // the next restart. We validate the config through `Resolver::build` first
    // so a malformed `[[roles]]` block still fails fast with a clear error.
    const RBAC_SEED_MARKER: &str = "rbac.seeded";
    let rbac_already_seeded = srv::db::app_settings::get(&db, RBAC_SEED_MARKER)
        .await
        .map_err(|e| anyhow::anyhow!("reading rbac seed marker: {e:#}"))?
        .is_some();
    if !rbac_already_seeded {
        srv::rbac::Resolver::build(config.rbac.clone(), config.roles.clone())
            .map_err(|e| anyhow::anyhow!("validating RBAC config for seeding: {e}"))?;
        srv::db::gateway_groups::seed_from_config(&db, &config.rbac, &config.roles)
            .await
            .map_err(|e| anyhow::anyhow!("seeding RBAC groups from config: {e:#}"))?;
        srv::db::app_settings::set(&db, RBAC_SEED_MARKER, "1")
            .await
            .map_err(|e| anyhow::anyhow!("recording rbac seed marker: {e:#}"))?;
    }

    // Build the RBAC resolver from the DB snapshot: the `read_skill` tool holds
    // a clone so it can authorize skill access at call time, the same way the
    // rest of the gateway resolves groups → grants. `bootstrap_admin_groups`
    // is layered on so a break-glass admin works even with an empty/broken DB.
    let group_snapshot = srv::db::gateway_groups::load_snapshot(&db)
        .await
        .map_err(|e| anyhow::anyhow!("loading RBAC groups from DB: {e:#}"))?;
    let rbac = Arc::new(srv::rbac::Resolver::from_snapshot(
        group_snapshot,
        config.bootstrap_admin_groups(),
    ));

    // Seed the dynamic skill-grant overlay (the per-group skill grants in
    // `skill_role_grants`) from the DB so grants survive a restart. A DB hiccup
    // here just leaves the overlay empty rather than failing startup.
    match srv::db::skill_grants::all(&db).await {
        Ok(grants) => rbac.set_skill_grant_overlay(grants),
        Err(e) => {
            tracing::warn!(error = %e, "loading skill-grant overlay; UI grants disabled until next edit")
        }
    }

    // Build the sandbox client once (when `[sandbox]` is enabled) — shared by
    // the sandbox tools, the typst PPTX/DOCX-export path, AND fetch_attachment's
    // Office-file extractor. `None` leaves typst rendering PDF + preview only
    // and Office uploads unreadable (binary stub).
    let sandbox_client: Option<Arc<rt::tools::sandbox::SandboxClient>> =
        match config.sandbox.as_ref() {
            Some(c) if c.enabled => Some(rt::tools::sandbox::SandboxClient::new(
                Arc::new(c.clone()),
                runtime.clone(),
            )),
            _ => None,
        };

    let mut tool_registry = rt::tools::ToolRegistry::new()
        .with(rt::tools::echo::Echo)
        .with(rt::tools::time::CurrentTimestamp)
        .with(gateway_tools::fetch_url::FetchUrl)
        // Fetch an image from a URL and keep it as a reusable attachment
        // (so it can be embedded in a later typst render). Always on — the
        // runtime guard errors cleanly off the chat path / without [chat.s3].
        .with(gateway_tools::load_image_url::LoadImageUrl)
        .with(gateway_tools::fetch_attachment::FetchAttachment::new(
            sandbox_client.clone(),
        ))
        .with(gateway_tools::upload_attachment::UploadAttachment)
        // Hand an existing conversation object to the user as a download —
        // a file from an earlier turn, or a data object that never got a
        // chip (a render's `.json` base). Reference-only: the bytes are
        // copied inside storage instead of being re-emitted by the model.
        .with(gateway_tools::offer_download::OfferDownload)
        // Same, for a *set* of files: one zip chip instead of one chip per
        // file, which past about four stops being a delivery and starts being
        // a scavenger hunt (and on a phone buries the reply).
        .with(gateway_tools::zip_attachments::ZipAttachments)
        // The other direction: an uploaded/produced text file becomes an
        // editable, versioned canvas document, so it can be changed a
        // passage at a time (and hand-edited by the user) instead of
        // rewritten wholesale through the model.
        .with(gateway_tools::import_file::ImportFile)
        // Inventory of the conversation's files (uploads + tool outputs), so
        // the model reuses existing assets instead of regenerating them.
        // Reads only the session's turn markers — no storage config needed.
        .with(gateway_tools::list_attachments::ListAttachments)
        .with(gateway_tools::search_web::SearchWeb)
        .with(gateway_tools::location::GetUserLocation)
        // Mid-turn question prompt. Configuration-free; the runtime gate is
        // `chat_feedback` being present, so it errors cleanly off the chat path
        // (and `requires_chat_session` keeps it out of the /v1 tool list).
        .with(gateway_tools::ask_user::AskUser)
        // Reach the user when they aren't watching: a finished long job, or a
        // scheduled action that found something. Runtime-gated on `[push]`
        // being configured plus a subscribed device, so it stays registered
        // (and RBAC-grantable) on deployments without push.
        .with(gateway_tools::notify_user::NotifyUser)
        // Reach the existing cron stack from a conversation. Create + delete
        // require an `ask_user` confirmation (and so are chat-only); listing
        // works anywhere. Actions created here always run without tools.
        .with(gateway_tools::schedule::ScheduleAction)
        .with(gateway_tools::schedule::ListScheduledActions)
        .with(gateway_tools::schedule::DeleteScheduledAction)
        .with(gateway_tools::memory::Remember)
        .with(gateway_tools::memory::Recall)
        // Correcting a memory needs no config either, and without these the
        // store is append-only: a changed fact could only be answered by a
        // second, contradicting memory.
        .with(gateway_tools::memory::UpdateMemory)
        .with(gateway_tools::memory::Forget)
        // Read-only public-data lookups — no secrets, no writes, safe to
        // leave always-on.
        .with(gateway_tools::netcheck::DnsLookup)
        .with(gateway_tools::netcheck::WhoisLookup)
        .with(gateway_tools::netcheck::TlsCert)
        .with(gateway_tools::wikipedia::Wikipedia)
        .with(gateway_tools::currency::ConvertCurrency)
        // RAG. These tools are no-ops without the indexer wired into
        // AppState; registering them unconditionally keeps RBAC config
        // stable across deployments where `[rag]` is only sometimes set.
        .with(gateway_tools::rag::RagListCollections::new(rbac.clone()))
        .with(gateway_tools::rag::RagSearch::new(rbac.clone()))
        // Regex over the same indexed corpus, for patterns BM25 can't express
        // (`TODO\(.*\)`, `impl .* for Tool`). Same per-collection group ACL.
        .with(gateway_tools::rag::RagGrep::new(rbac.clone()))
        // Document-level retrieval over the fields the extraction profile
        // pulled out. This is what answers questions about *sets* of
        // documents — "the latest invoice from X", "everything about project
        // Y" — which passage retrieval structurally cannot.
        .with(gateway_tools::rag_documents::RagQueryDocuments::new(
            rbac.clone(),
        ))
        .with(gateway_tools::rag_documents::RagListDocuments::new(
            rbac.clone(),
        ))
        .with(gateway_tools::rag_documents::RagFetchDocument::new(
            rbac.clone(),
        ))
        // Document canvas — build up and incrementally edit long documents
        // across turns. Content lives in the `documents` store, not S3, so
        // these need no extra config; off the chat path they error cleanly.
        .with(gateway_tools::document::CreateDocument)
        .with(gateway_tools::document::EditDocument)
        .with(gateway_tools::document::ReadDocument)
        .with(gateway_tools::document::ListDocuments)
        .with(gateway_tools::document::EditDocumentSection)
        .with(gateway_tools::document::ListDocumentVersions)
        .with(gateway_tools::document::RestoreDocumentVersion)
        // Soft delete, so the canvas can be cleaned up without breaking its
        // "nothing is ever lost" promise — the tombstone keeps the version
        // history and `undelete_document` reverses it.
        .with(gateway_tools::document::DeleteDocument)
        .with(gateway_tools::document::UndeleteDocument)
        // QR codes render natively in-process (no sandbox, no upstream), so
        // the tool is always on; it needs [chat.s3] at runtime to deliver
        // the file and errors cleanly without it.
        .with(gateway_tools::qr::GenerateQrCode);
    // `lookup_ip` is GeoIP-only — unlike `get_user_location` (which also has
    // the browser-GPS path), it can do nothing without a database. Register
    // it only when `[geoip]` is configured, so the model is never offered a
    // tool that could only ever answer "not available". A configured-but-
    // not-yet-loaded file is fine: the handle hot-reloads (see below) and the
    // tool's own runtime guard returns a clean `known:false` in the gap.
    if config.geoip.is_some() {
        tool_registry = tool_registry.with(gateway_tools::lookup_ip::LookupIp);
        tracing::info!(tool = "lookup_ip", "registered GeoIP lookup tool");
    } else {
        tracing::info!("no [geoip] config — lookup_ip tool not registered");
    }
    // generate_image only when an `kind = "image"` upstream pool exists —
    // same rationale as lookup_ip: don't offer the model a tool whose every
    // call would fail with "no image backend configured". Keyed off the live
    // DB-backed registry (not config.toml), so the tool surface matches the
    // topology the admin actually configured through the UI.
    let image_pools: Vec<_> = upstreams
        .pools()
        .into_iter()
        .filter(|p| p.kind == srv::upstreams::PoolKind::Image)
        .collect();
    if !image_pools.is_empty() {
        tool_registry = tool_registry.with(gateway_tools::generate_image::GenerateImage);
        tracing::info!(tool = "generate_image", "registered image-generation tool");
    } else {
        tracing::info!("no image-kind upstream pool — generate_image tool not registered");
    }
    // edit_image only when an image backend advertises edit support — same
    // "don't offer a tool that can only fail" rule. z.AI's GLM-Image is
    // generation-only (supports_edit = false); a self-hosted Qwen-Image-Edit
    // sets it true.
    if image_pools
        .iter()
        .any(|p| p.backends.iter().any(|b| b.supports_edit()))
    {
        tool_registry = tool_registry.with(gateway_tools::edit_image::EditImage);
        tracing::info!(tool = "edit_image", "registered image-editing tool");
    } else {
        tracing::info!("no edit-capable image backend — edit_image tool not registered");
    }
    // Display metadata for the discovered templates, for the per-template
    // toggle rows in the tool menu / `/tools` page (the human title isn't in
    // the tool schema). Stays empty when `[typst]` isn't configured.
    // One tool per typst template. Built through the same closure the settings
    // reload uses (`with_tool_family_builder` below), so there is exactly one
    // place that knows which tools a template produces — the boot path and the
    // rebuild cannot drift apart.
    let typst_family = gateway::tool_families::typst();
    let boot_surface = rt::state::FeatureSurface {
        sandbox_client: sandbox_client.clone(),
        ..Default::default()
    };
    tool_registry = tool_registry.with_family_replaced(
        srv::tool_naming::TYPST_PREFIX,
        typst_family(&config, &boot_surface),
    );
    let typst_metas = rt::state::typst_template_metas(&config);
    // MCP servers are no longer registered at boot: they live in the
    // admin-managed connector catalog (`/admin/connectors`) and are connected
    // lazily per request by `rt::tools::mcp::manager` — global connectors as a
    // shared connection, per-user connectors with the user's own credential.
    // Code-execution sandbox. Registered only when `[sandbox]` points at a
    // reachable sandbox-runner; the three tools share one HTTP client.
    match config.sandbox.as_ref() {
        Some(sandbox_cfg) if sandbox_cfg.enabled => {
            // Reuse the client built above (Some whenever [sandbox] is enabled).
            let client = sandbox_client
                .clone()
                .expect("sandbox_client is built when [sandbox] is enabled");
            // Ask the runner what it can do before deciding what to register.
            // Egress is a property of the *runner's* deployment (a podman
            // network + an allowlisting proxy) that this config can't see, so
            // without asking we would offer web tools on an offline runner and
            // every call would fail. Unreachable runner → treated as capable;
            // see `SandboxClient::egress_available`.
            client.probe_capabilities().await;
            let egress = client.egress_available();
            tool_registry = tool_registry
                .with(rt::tools::sandbox::RunInSandbox(client.clone()))
                .with(rt::tools::sandbox::GenerateDocument(client.clone()))
                .with(rt::tools::sandbox::ExportDocument(client.clone()))
                .with(rt::tools::sandbox::ConvertDocument(client.clone()))
                .with(rt::tools::sandbox::EditPresentation(client.clone()))
                .with(rt::tools::sandbox::RenderExcalidraw(client.clone()))
                .with(rt::tools::sandbox::RenderTypst(client.clone()))
                .with(rt::tools::sandbox::RenderVideo(client.clone()))
                .with(rt::tools::sandbox::ReadSandboxOutput);
            // Web tools need egress by definition — every call would fail
            // without it, so absent beats always-failing (the rule in
            // docs/tools-inventory.md).
            if egress {
                tool_registry = tool_registry
                    .with(rt::tools::sandbox::CaptureWebpage(client.clone()))
                    .with(rt::tools::sandbox::BrowsePage(client));
            }
            tracing::info!(
                runner = %sandbox_cfg.runner_url, egress,
                "registered sandbox tools"
            );
        }
        Some(_) => tracing::info!("[sandbox] enabled = false — sandbox tools not registered"),
        None => tracing::info!("no [sandbox] config — sandbox tools not registered"),
    }
    // `enable_tools` is registered last so its catalog snapshot covers every
    // other tool (static + typst + MCP + ComfyUI). It's part of the
    // always-on core so the model can always reach it; calling it writes
    // per-conversation rows that the next round's `allowed_tools_for_session`
    // picks up.
    //
    // We need the ComfyUI store handle here (so enable_tools can advertise
    // the `comfyui` master toggle in its description), but the store is built
    // from config — same source the rest of the registry sees. Build it now
    // from `[comfyui]`, before wiring the ComfyuiHandle onto AppState further
    // down.
    let comfyui_store: Option<std::sync::Arc<feat::comfyui::ComfyuiStore>> =
        if let Some(comfyui_cfg) = config.comfyui.clone().filter(|c| c.enabled) {
            let store = std::sync::Arc::new(feat::comfyui::ComfyuiStore::load(
                comfyui_cfg.content_dir.clone(),
            ));
            tracing::info!(
                content_dir = %comfyui_cfg.content_dir.display(),
                loaded = store.current().len(),
                "loaded ComfyUI workflow catalog",
            );
            Some(store)
        } else {
            None
        };
    let mut enable_tools = gateway_tools::enable_tools::EnableTools::from_registry(&tool_registry);
    if let Some(store) = comfyui_store.clone() {
        enable_tools = enable_tools.with_comfyui_store(store);
    }
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
        let store = feat::skills::SkillStore::load(skills_cfg.dir.clone());
        tracing::info!(
            dir = %skills_cfg.dir.display(),
            count = store.current().len(),
            "loaded skills store"
        );
        Arc::new(store)
    });
    // Per-user **private** skills live under `<skills.dir>/.users/` — a level
    // deeper than the global scanner reaches, so the two never cross. Wired
    // only when `[skills]` is configured, in lockstep with `skill_store`.
    let user_skill_store = config.skills.as_ref().map(|skills_cfg| {
        let users_dir = skills_cfg.dir.join(".users");
        // Best-effort: create the skills dir + its `.users` subdir now, so the
        // accessibility check (which drives the `/skills` nav visibility and
        // the admin "no directory access" message) reflects a real, usable
        // directory. A permission failure here is logged, not fatal — the
        // check then reports "not accessible" and the feature hides itself.
        if let Err(err) = std::fs::create_dir_all(&users_dir) {
            tracing::warn!(
                error = %err,
                dir = %users_dir.display(),
                "could not create private-skills directory — /skills will be hidden until it's accessible"
            );
        }
        Arc::new(feat::skills::UserSkillStore::new(users_dir))
    });
    if let (Some(store), Some(user_store)) = (skill_store.as_ref(), user_skill_store.as_ref()) {
        tool_registry = tool_registry.with(gateway_tools::read_skill::ReadSkill::new(
            store.clone(),
            user_store.clone(),
            rbac.clone(),
        ));
        tracing::info!(tool = "read_skill", "registered skills tool");
    }
    let tools = Arc::new(tool_registry);

    // Session store reuses the HMAC key derived up top. `crypto` (also built
    // up top) is the same at-rest key used for per-user MCP OAuth tokens,
    // connector client secrets, and backend API keys.
    // Sliding idle timeout + absolute cap from config (30 / 90 days by
    // default) — see `rama_server::session` for why the cookie's own
    // Max-Age is longer than either.
    let days = |d: i64| std::time::Duration::from_secs(d.clamp(1, 400) as u64 * 24 * 60 * 60);
    let sessions = SessionStore::new(db.clone(), session_secret)
        .with_ttl(days(config.gateway.session_ttl_days))
        .with_absolute_max(days(config.gateway.session_absolute_max_days));
    // Seed the built-in MCP connector catalog (all disabled) so an admin only
    // has to flip a switch. Idempotent — existing rows (incl. admin edits +
    // the enabled flag) are never overwritten. Non-fatal: a failure here just
    // means the store starts empty until an admin adds connectors manually.
    if let Err(err) = srv::db::mcp_catalog::seed_defaults(&db).await {
        tracing::warn!(error = %err, "seeding default MCP connectors failed");
    }

    let mut state = AppState::new(config, db, upstreams, tools, rbac)
        .with_runtime_handle(runtime)
        .with_crypto(crypto)
        .with_typst_templates(typst_metas)
        // The same closure the boot path just used, so a settings save
        // rebuilds the typst family exactly the way boot built it.
        .with_tool_family_builder(typst_family);
    // Share the sandbox client with the per-turn code so it can build a
    // `SandboxLease` (the container kept alive across a turn's tool rounds).
    // `None` leaves leasing off — every sandbox call stays single-use.
    if let Some(client) = sandbox_client.clone() {
        state = state.with_sandbox_client(client);
    }
    // ComfyUI worker — optional, internal-only. The store was built above
    // (before enable_tools, so the catalog could advertise the `comfyui`
    // master toggle). Here we just pair it with the HTTP client + S3
    // handle and install it on AppState. The store hot-reloads on
    // `POST /api/v0/comfyui/reload`; the tool source is wired into the
    // chat tool-overlay path (mirrors MCP per-request layering), not
    // into the static `ToolRegistry`, so new workflows discovered via
    // reload don't need the registry to rebuild.
    if let Some(comfyui_cfg) = state.config().comfyui.clone().filter(|c| c.enabled) {
        match feat::comfyui::Client::new(comfyui_cfg.base_url.clone()) {
            Ok(client) => {
                // The store was already built above — reuse the same
                // Arc so live reloads stay visible to enable_tools.
                let store = comfyui_store
                    .clone()
                    .expect("comfyui_store built alongside enable_tools");
                // Pass the chat-attachment S3 config through so the
                // runner can resolve Image/Video/AudioAttachment params
                // via the same path `edit_image` uses. The config is
                // already validated when the state was built — `None`
                // only when `[chat.s3]` is unconfigured, in which case
                // workflows with attachment-kind params fail cleanly.
                let s3 = state.config().chat.s3.clone().map(std::sync::Arc::new);
                let chat_updates = feat::comfyui::ChatUpdateRegistry::default();
                let handle = std::sync::Arc::new(rt::comfyui_tool::ComfyuiHandle {
                    store: store.clone(),
                    client: client.clone(),
                    runner_poll_interval: std::time::Duration::from_millis(
                        comfyui_cfg.queue_poll_interval_ms,
                    ),
                    runner_timeout: std::time::Duration::from_secs(comfyui_cfg.timeout_secs),
                    s3,
                    max_concurrent_jobs: comfyui_cfg.max_concurrent_jobs,
                    job_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(
                        comfyui_cfg.max_concurrent_jobs.max(1),
                    )),
                    chat_updates: chat_updates.clone(),
                });
                tracing::info!(
                    base_url = %comfyui_cfg.base_url,
                    content_dir = %comfyui_cfg.content_dir.display(),
                    loaded = store.current().len(),
                    "registered ComfyUI catalog",
                );
                // Spawn the async job scheduler. It polls pending jobs
                // every `queue_poll_interval_ms`, fetches completed
                // assets from ComfyUI, re-hosts them in S3, and appends
                // the attachment marker to the owning turn. Boot-tolerant:
                // pending jobs survive in the DB across restarts.
                let scheduler = feat::comfyui::ComfyuiScheduler::new(
                    state.db.clone(),
                    client.clone(),
                    state.config().chat.s3.clone().map(std::sync::Arc::new),
                    std::time::Duration::from_millis(comfyui_cfg.queue_poll_interval_ms),
                    std::time::Duration::from_secs(comfyui_cfg.timeout_secs),
                    chat_updates,
                );
                scheduler.spawn();
                state = state.with_comfyui(handle);
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    base_url = %comfyui_cfg.base_url,
                    "[comfyui] client build failed — no comfyui tools will be registered",
                );
            }
        }
    }
    // Install the wizard-owned settings (public URL, live OIDC client,
    // first-run flag) from the database. From here on they are read through
    // `state.public_url()` / `state.oidc()` / `state.setup_completed()`, so
    // finishing the wizard can swap them without a restart — the wizard calls
    // this very same function.
    state.reload_runtime().await;
    // Logged after the settings land, because "why was I logged out?" is
    // otherwise unanswerable from the outside and `secure_cookies` depends on
    // the public URL the wizard recorded — not the config-file fallback.
    tracing::info!(
        idle_timeout_days = state.config().gateway.session_ttl_days,
        absolute_max_days = state.config().gateway.session_absolute_max_days,
        public_url = %state.public_url(),
        secure_cookies =
            gateway::rama_server::session::secure_cookies(&state.public_url()),
        "session policy",
    );
    if let Some(store) = skill_store {
        state = state.with_skills(store);
    }
    if let Some(user_store) = user_skill_store {
        state = state.with_user_skills(user_store);
    }

    // Web Push: load (or first-time generate + persist) the VAPID keypair and
    // wire the sender in. Disabled via `[push] enabled = false`; a keypair
    // failure logs and leaves push off rather than blocking boot.
    if state.config().push.enabled {
        match feat::push::PushSender::new(
            &state.db,
            &state.crypto,
            state.config().push.contact.clone(),
        )
        .await
        {
            Ok(sender) => state = state.with_push(std::sync::Arc::new(sender)),
            Err(err) => {
                tracing::warn!(error = %err, "Web Push disabled: could not initialize VAPID key")
            }
        }
    } else {
        tracing::info!("Web Push disabled via [push].enabled = false");
    }

    // GeoIP (client-IP → coarse location) for the `get_user_location`
    // tool. Optional: with no `[geoip]` block we skip it entirely. A
    // missing DB file is fine — the handle loads lazily, hot-reloads when
    // a file appears, and the (token-gated) weekly updater is a no-op
    // without a token. So this never blocks boot or fails the gateway.
    if let Some(geoip_cfg) = state.config().geoip.clone() {
        let geo = feat::geoip::GeoIp::new(geoip_cfg.db_path.clone());
        geo.watch();
        feat::geoip::update::spawn(geoip_cfg.db_path.clone(), geoip_cfg.update_token());
        state = state.with_geoip(geo);
    }

    // Usage metrics: a batched background writer + retention-prune task,
    // fronted by a fire-and-forget handle on the shared state. When
    // `[usage] enabled = false` the handle is a no-op and no tasks spawn.
    let usage = if state.config().usage.enabled {
        srv::usage::spawn(state.db.clone(), state.config().usage.retention_days)
    } else {
        tracing::info!("usage metrics disabled via [usage].enabled = false");
        srv::usage::UsageHandle::disabled()
    };

    // RAG indexer — always wired in. The DB-backed collection registry
    // starts empty, so deployments that don't use RAG just have a quiet
    // poller running every 30s. Operators add collections via the admin
    // API; the worker picks them up.
    //
    // `[rag] data_dir` MUST resolve to a writable path. The container
    // image runs with a read-only rootfs (see deploy/quadlet/gateway.container),
    // so the default `data/rag` works for local dev only — operators
    // point this at a subdirectory of the named volume.
    let rag_config = state.config().rag.clone().unwrap_or_default();
    let indexer_config = feat::rag::worker::IndexerConfig {
        data_dir: rag_config.data_dir,
        clone_concurrency: rag_config.clone_concurrency,
        ..feat::rag::worker::IndexerConfig::default()
    };
    // One OCR service for the whole process. Built here rather than letting
    // `RamaState` construct its own, because its concurrency gate only bounds
    // GPU work if the indexer and the chat path share the same instance —
    // two would allow twice the configured concurrency.
    let ocr = feat::ocr::OcrService::new(
        state.config().chat.ocr.clone(),
        state.upstreams.clone(),
        state.http.clone(),
        usage.clone(),
        state.db.clone(),
    );
    // Office documents are read through the sandbox. `gateway-features` sits
    // below the sandbox client, so the capability is injected here rather
    // than reached for from inside the indexer.
    let office: Option<std::sync::Arc<dyn feat::rag::extract::OfficeExtractor>> =
        sandbox_client.clone().map(|sb| {
            std::sync::Arc::new(rt::tools::sandbox::SandboxOfficeExtractor::new(sb))
                as std::sync::Arc<dyn feat::rag::extract::OfficeExtractor>
        });
    let indexer = feat::rag::worker::Indexer::new(
        state.db.clone(),
        state.upstreams.clone(),
        state.http.clone(),
        indexer_config,
        // Opens the sealed credentials of remote (non-git) sources.
        Some(state.crypto.clone()),
    )
    .with_document_readers(Some(ocr.clone()), office);
    // Re-queue any ref left mid-build by a previous crash/restart and reap
    // orphaned build folders before the loop starts handling new work.
    indexer.recover_on_startup().await;
    feat::rag::worker::spawn(indexer.clone());
    state = state.with_indexer(indexer);

    let state =
        Arc::new(gateway::rama_server::RamaState::new(state, sessions, usage).with_ocr(ocr));

    // Scheduled actions: start the background loop that fires due actions
    // (the `scheduled_actions` table is created by migration 0021).
    rt::scheduled::worker::spawn(state.clone());

    // Per-user MCP connections: proactively refresh OAuth tokens before they
    // expire (and exercise idle refresh tokens) so connectors stay alive
    // without the user re-authenticating; also sweeps stale pending auths.
    rt::tools::mcp::worker::spawn(state.clone());

    // `$IP`/`$PORT`, else loopback:8080. See `Config::bind_address`.
    let addr = state.config().bind_address();
    tracing::info!(%addr, "rama gateway starting");

    gateway::rama_server::router::serve(state, SocketAddress::new(addr.ip(), addr.port())).await
}

/// Picks up the session HMAC secret from `$GATEWAY_SESSION_KEY` —
/// 64 hex chars (32 bytes). Empty when unset, which
/// [`load_session_secret`] turns into a fatal, actionable error.
fn state_session_key() -> String {
    std::env::var(srv::config::SESSION_KEY_VAR).unwrap_or_default()
}

/// What `gateway --help` prints, and what an unrecognised argument points at.
const USAGE: &str = "\
Usage: gateway [COMMAND]

With no command the gateway starts serving.

Commands:
  restore-setup   Reopen the setup wizard for 30 minutes and print a one-time
                  link. Run it inside the container (docker compose exec /
                  podman exec) when you can no longer sign in. The gateway
                  keeps serving while the window is open.
  -h, --help      Print this help.

Configuration lives in the database and is managed in the browser. The only
required environment variable is GATEWAY_SESSION_KEY.";

/// The one environment variable a deployment MUST set. Everything else the
/// gateway needs is either derived from it (the at-rest encryption key, via
/// [`gateway_core::server::crypto::Crypto::from_env_or_session`]), defaulted
/// by the container image, or configured through the web UI.
const SESSION_KEY_HELP: &str = "GATEWAY_SESSION_KEY is required: it signs browser sessions and \
     (unless GATEWAY_ENCRYPTION_KEY is set) derives the key that seals every secret \
     stored in the database. Generate one ONCE and keep it for the life of the \
     deployment:\n\n    GATEWAY_SESSION_KEY=$(openssl rand -hex 32)\n\n\
     Losing or changing it logs every user out and makes stored backend API keys, \
     connector secrets and OAuth tokens permanently unreadable.";

fn load_session_secret(raw: &str) -> anyhow::Result<[u8; 32]> {
    // Refusing to boot is deliberate. The old behaviour — fall back to an
    // ephemeral key and log an error — produced a gateway that LOOKED healthy
    // and then silently logged everyone out on every restart and lost every
    // sealed secret. That failure is invisible until it has already cost data,
    // so it belongs at startup, where the operator is watching.
    if raw.is_empty() {
        anyhow::bail!("{SESSION_KEY_HELP}");
    }
    let bytes = srv::crypto::hex_decode(raw).ok_or_else(|| {
        anyhow::anyhow!("GATEWAY_SESSION_KEY must be 64 hex chars (32 bytes). {SESSION_KEY_HELP}")
    })?;
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

/// Abort if the database we are about to open is *not* the one this deployment
/// has been using.
///
/// `GATEWAY_DATA_DIR` moved the *default* database path from the working
/// directory to the data volume. That is right for new deployments and wrong,
/// silently, for an existing one that ran without `[db].path` and persisted its
/// working directory: `db::open` would happily create an empty database at the
/// new path, the gateway would find no users, conclude it is a fresh install —
/// and serve an **open, unauthenticated `/setup`** on a production URL while
/// the real users, chats and sealed keys sat untouched a directory away.
///
/// Fires only in that exact case: the path is the *default* (an operator who
/// set `[db].path` explicitly chose it and is not affected), the default has
/// actually moved (`GATEWAY_DATA_DIR` is set), a database exists where the old
/// default put it, and none exists where the new one points.
fn refuse_to_orphan_an_existing_database(resolved: &std::path::Path) -> anyhow::Result<()> {
    let legacy = srv::config::legacy_default_db_path();
    // Not the default path → the operator named it, so nothing moved under
    // them. Also covers the case where the default has not moved at all.
    if resolved != srv::config::default_db_path() || resolved == legacy {
        return Ok(());
    }
    if !legacy.exists() || resolved.exists() {
        return Ok(());
    }
    anyhow::bail!(
        "found an existing database at {} (the working directory), but this release resolves \
         the default database to {}.\n\nThat path changed when GATEWAY_DATA_DIR was \
         introduced. Booting would create an empty database and treat this deployment as a \
         fresh install — which opens an unauthenticated setup wizard while your real data \
         sits where it is. Nothing has been changed.\n\nMove the existing file to the new \
         location (with its -wal and -shm siblings), or point GATEWAY_DATA_DIR / [db].path at \
         where it already lives.",
        legacy.display(),
        resolved.display(),
    )
}

/// Reopen the setup wizard on an already-configured gateway, for an operator
/// who can no longer sign in — the IdP moved, or no group maps to admin.
///
/// Deliberately narrow. It does **not** stop the gateway, log anyone out,
/// delete anything, or put the gateway back into first-run mode: everyone
/// still using it keeps working while the window is open, and the wizard comes
/// up pre-filled with the current provider. All it does is open a 30-minute
/// window and print a one-time link.
///
/// The link carries a token because, unlike a first run, there is now
/// something to steal: a reachable production gateway with real users. The
/// operator is already at a terminal reading this output, so copying a URL
/// costs them nothing and closes the hole completely.
async fn restore_setup(config: &Config) -> anyhow::Result<()> {
    // `attach`, not `open`: the gateway is still running and streaming turns.
    // `open` would run migrations and sweep every `in_progress` turn to
    // `errored` — a maintenance command must not fail every conversation in
    // flight. See `db::attach`.
    let db_path = config.db_path()?;
    let db = srv::db::attach(&db_path).await.map_err(|e| {
        anyhow::anyhow!(
            "opening the gateway database at {}: {e:#}\n\nRun this inside the gateway container \
             (e.g. `docker compose exec gateway restore-setup`), so it sees the same database.",
            db_path.display()
        )
    })?;

    let token = srv::crypto::random_hex(32);
    let until = srv::setup::open_recovery(&db, &token)
        .await
        .map_err(|e| anyhow::anyhow!("opening the recovery window: {e:#}"))?;

    // Prefer the URL the gateway actually serves on; fall back to the config
    // file's placeholder so the line is still copy-pasteable on a fresh box.
    let base = srv::setup::public_url(&db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| config.public_url_fallback().to_string());

    // Straight to stdout, not the tracing subscriber: this is the command's
    // output, and it must not be swallowed by a RUST_LOG filter.
    println!(
        "Setup reopened until {until} ({} minutes).",
        srv::setup::RECOVERY_WINDOW.as_mins()
    );
    println!();
    println!("    {base}/setup?claim={token}");
    println!();
    println!("Open that link once. The gateway keeps serving normally in the meantime —");
    println!("nobody is logged out and no configuration has been changed yet.");
    tracing::info!(%until, "setup recovery window opened via restore-setup");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_session_key_refuses_to_boot() {
        // The regression this pins: the gateway used to fall back to an
        // ephemeral key here and keep booting, which logged every user out on
        // every restart and made sealed secrets unreadable — invisibly.
        let err = load_session_secret("").expect_err("empty key must be fatal");
        let msg = err.to_string();
        assert!(msg.contains("GATEWAY_SESSION_KEY is required"), "{msg}");
        // The message has to carry the fix, not just the complaint: an operator
        // reading a crash log should be able to copy exactly one line.
        assert!(msg.contains("openssl rand -hex 32"), "{msg}");
    }

    #[test]
    fn malformed_session_key_is_rejected_with_the_same_guidance() {
        let msg = load_session_secret("not-hex").unwrap_err().to_string();
        assert!(msg.contains("64 hex chars"), "{msg}");
        assert!(msg.contains("openssl rand -hex 32"), "{msg}");

        // Right alphabet, wrong length — must not silently zero-pad.
        let msg = load_session_secret("abcd").unwrap_err().to_string();
        assert!(msg.contains("expected 32"), "{msg}");
    }

    #[test]
    fn a_valid_session_key_round_trips() {
        assert_eq!(load_session_secret(&"0".repeat(64)).unwrap(), [0u8; 32]);
    }
}
