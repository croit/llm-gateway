// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::server::tools::ToolRegistry;
use gateway_core::server::auth::oidc::OidcClient;
use gateway_core::server::config::Config;
use gateway_core::server::crypto::Crypto;
use gateway_core::server::db::Pool;
use gateway_core::server::rbac::Resolver;
use gateway_core::server::upstreams::UpstreamRegistry;
use gateway_features::server::geoip::GeoIp;
use gateway_features::server::rag::worker::Indexer;
use gateway_features::server::skills::SkillStore;

/// OIDC discovery retry loop: 5 attempts with exponential backoff (500ms → 8s),
/// then give up and leave the client unset. `/auth/*` reports that cleanly, so
/// a transient network blip at boot costs a delayed sign-in rather than a crash
/// loop. Callers that just validated the provider themselves (the setup wizard)
/// return on the first attempt in practice.
async fn build_oidc_with_retry(
    params: &gateway_core::server::auth::oidc::OidcParams,
    public_url: &str,
) -> Option<Arc<OidcClient>> {
    let max_attempts = 5;
    for attempt in 1..=max_attempts {
        match OidcClient::build(params, public_url).await {
            Ok(client) => return Some(client),
            Err(err) if attempt < max_attempts => {
                let backoff = std::time::Duration::from_millis(500u64 << (attempt - 1));
                tracing::warn!(
                    attempt, max_attempts,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %err,
                    "OIDC discovery failed; retrying",
                );
                tokio::time::sleep(backoff).await;
            }
            Err(err) => tracing::error!(
                error = %err,
                "OIDC discovery failed after {max_attempts} attempts; continuing without \
                 OIDC — /auth/login + /auth/callback will report it until this is resolved",
            ),
        }
    }
    None
}

/// Shared handle to the wizard-owned settings.
///
/// Held by [`AppState`], and by the few things constructed *before* it that
/// still need a live public URL — today the sandbox client, whose artifact
/// download links would otherwise be frozen at the boot-time fallback
/// (`http://localhost:8080` on a fresh install) until the next restart.
pub type RuntimeHandle = Arc<ArcSwap<RuntimeSettings>>;

/// The slice of configuration the setup wizard writes and the gateway
/// re-reads without restarting. Read it through [`AppState::public_url`],
/// [`AppState::oidc`] and [`AppState::setup_completed`].
///
/// These three are bundled because they are swapped at the same instant — the
/// moment the wizard finishes — not because they belong together. That means
/// anything holding a [`RuntimeHandle`] for one of them (the sandbox client
/// wants only `public_url`) also sees the other two. Acceptable while there are
/// three; if a fourth arrives that is unrelated to setup, split rather than
/// widen.
#[derive(Clone)]
pub struct RuntimeSettings {
    /// The gateway's own base URL, no trailing slash. Every absolute link the
    /// server builds — the OIDC `redirect_uri`, connector callbacks, share
    /// links — and the `Secure` flag on the session cookie derive from it.
    pub public_url: String,
    /// The live OIDC client. `None` before setup, or when the stored provider
    /// settings could not be turned into a working client (discovery down at
    /// boot, at-rest key changed); `/auth/login` then reports it cleanly.
    pub oidc: Option<Arc<OidcClient>>,
    /// Whether the setup wizard has completed. `false` puts the UI in
    /// first-run mode: every page redirects to `/setup`.
    pub setup_completed: bool,
}

impl RuntimeSettings {
    /// A fresh handle seeded with the config-file fallback and nothing
    /// configured. `AppState::reload_runtime` fills in what the database says.
    pub fn new_handle(config_public_url: &str) -> RuntimeHandle {
        Arc::new(ArcSwap::from_pointee(Self {
            public_url: config_public_url.trim_end_matches('/').to_string(),
            oidc: None,
            // A gateway that has not consulted the DB yet must not put the UI
            // into first-run mode; `reload_runtime` corrects this immediately.
            setup_completed: true,
        }))
    }
}

/// Everything the operator settings can switch on, off or point somewhere
/// else — rebuilt as one unit whenever those settings change.
///
/// These seven were the reason most of `/admin/settings` used to say "takes
/// effect after a restart": each is an object constructed once at boot from a
/// config block (a client, a loaded database, a scanned directory, a tool
/// registry), so changing the block changed nothing until the process came
/// back. Bundling them makes the rebuild atomic: a save can never leave the
/// gateway with `sandbox.enabled` on but no sandbox tools registered, because
/// the registry and the client are replaced in the same swap.
///
/// Every one of these is built from a `gateway-features` scanner or a client
/// this crate owns, so [`AppState::reload_settings`] can rebuild the bundle
/// itself — no boot-installed callback, even though registering a *tool* would
/// need `gateway-tools`, which this crate deliberately does not depend on.
/// That works because tool availability is decided per request by
/// [`AppState::allowed_tools_for_user`], not at registration time: the registry
/// holds what the build can do, and the live config decides what this
/// deployment currently offers.
/// Rebuilds the one tool family whose membership depends on a setting.
///
/// Today that is typst: it registers one concrete tool per discovered
/// template, so pointing `typst.templates_dir` somewhere else — or switching
/// typst on at all — changes which tools should exist. A per-request filter
/// cannot invent a tool that was never registered, which is why this is a
/// rebuild rather than a filter.
///
/// A callback installed at boot rather than a function this crate can call:
/// naming `TypstRenderTool` means depending on `gateway-tools`, and that crate
/// depends on this one. The binary crate sees everything, so it supplies the
/// closure via [`AppState::with_tool_family_builder`], and
/// [`AppState::reload_settings`] calls it on every settings save. Returning the
/// tools rather than a whole registry keeps the boot path as the single place
/// that knows the full tool list.
pub type ToolFamilyBuilder =
    Arc<dyn Fn(&Config, &FeatureSurface) -> Vec<Arc<dyn crate::server::tools::Tool>> + Send + Sync>;

#[derive(Clone, Default)]
pub struct FeatureSurface {
    /// Display metadata for the discovered typst templates (manifest title +
    /// description). The catalog needs it to render a per-template toggle row —
    /// the human title isn't in the tool schema. Empty when typst is off.
    pub typst_templates: Arc<Vec<crate::server::tools::catalog::TemplateMeta>>,
    /// Loaded Agent Skills, itself behind a hot-reloadable store (an admin
    /// upload re-scans and swaps it). `None` when skills are off; an empty
    /// store is fine. RBAC narrows which skills each caller sees.
    pub skills: Option<Arc<SkillStore>>,
    /// Per-user **private** Agent Skills, under `<skills.dir>/.users/`. Moves
    /// in lockstep with [`Self::skills`] — same feature toggle, one level
    /// deeper, so the two scanners never cross.
    pub user_skills: Option<Arc<gateway_features::server::skills::UserSkillStore>>,
    /// Client-IP → location resolver for `get_user_location`. `None` when
    /// GeoIP is off; the tool then relies on the browser-provided position.
    pub geoip: Option<GeoIp>,
    /// The sandbox-runner HTTP client, shared so the per-turn code can build a
    /// lease (one container across a turn's tool rounds). `None` when the
    /// sandbox is off.
    pub sandbox_client: Option<Arc<crate::server::tools::sandbox::SandboxClient>>,
    /// ComfyUI workflow catalog + HTTP client. `None` when ComfyUI is off. The
    /// tool source reads the live snapshot per request, which is why an admin
    /// `POST /api/v0/comfyui/reload` already took effect without a restart.
    pub comfyui: Option<Arc<crate::server::comfyui_tool::ComfyuiHandle>>,
}

/// Display metadata for the typst templates the current config points at.
///
/// The one place that maps a discovered template to its catalog row, so the
/// boot path and [`AppState::reload_settings`] cannot disagree about the
/// per-template toggle list. Discovery failures are a warning and an empty
/// list: a broken templates directory must not keep the gateway from booting.
pub fn typst_template_metas(config: &Config) -> Vec<crate::server::tools::catalog::TemplateMeta> {
    let Some(cfg) = config.typst.as_ref() else {
        return Vec::new();
    };
    match gateway_features::server::typst::discover_templates(&cfg.templates_dir) {
        Ok(templates) => templates
            .into_iter()
            .map(|t| crate::server::tools::catalog::TemplateMeta {
                key: format!(
                    "{}{}",
                    gateway_core::server::tool_naming::TYPST_PREFIX,
                    t.id
                ),
                title: t.title,
                description: t.description,
            })
            .collect(),
        Err(err) => {
            tracing::warn!(
                error = %err, dir = %cfg.templates_dir.display(),
                "could not read the typst templates directory"
            );
            Vec::new()
        }
    }
}

/// Shared application state injected into Axum handlers.
///
/// Clone is cheap — every field is either Arc-shared or already cloneable
/// without I/O.
#[derive(Clone)]
pub struct AppState {
    pub http: reqwest::Client,
    /// The effective configuration: what the file said, with the twelve
    /// settings-owned blocks overwritten from the database.
    ///
    /// Swappable, because `/admin/settings` has to be able to change them
    /// without a restart. Read it through [`Self::config`] — one load, then use
    /// the returned `Arc` for the whole request, so a save landing mid-request
    /// cannot show that request two different configurations.
    config: Arc<ArcSwap<Config>>,
    pub db: Pool,
    /// The settings the setup wizard owns, swappable at runtime.
    ///
    /// These are the only three values a running gateway has to be able to
    /// change without a restart: finishing the wizard has to produce a working
    /// `/login` right there in the browser, which means installing a live OIDC
    /// client and the public URL its `redirect_uri` was built from. Everything
    /// else is either static (`config`) or already hot-reloadable in its own
    /// right (the upstream registry, the skill store).
    ///
    /// Read through [`Self::public_url`] / [`Self::oidc`] /
    /// [`Self::setup_completed`] rather than touching this field.
    runtime: Arc<ArcSwap<RuntimeSettings>>,
    pub upstreams: Arc<UpstreamRegistry>,
    /// The tool surface the model can be offered. Swappable, because a feature
    /// switched on at runtime has to be able to register its tools — read it
    /// through [`Self::tools`].
    tools: Arc<ArcSwap<ToolRegistry>>,
    /// How to rebuild the settings-dependent tool family. `None` in tests and
    /// in the dev harness, which build a registry once and never change it;
    /// production installs one at boot.
    tool_family_builder: Option<ToolFamilyBuilder>,
    pub rbac: Arc<Resolver>,
    /// The features the operator settings can switch on, off or repoint —
    /// swapped as one unit by [`Self::reload_settings`]. Read through
    /// [`Self::geoip`], [`Self::skills`], [`Self::user_skills`],
    /// [`Self::typst_templates`], [`Self::sandbox_client`] and
    /// [`Self::comfyui`] rather than touching this field.
    surface: Arc<ArcSwap<FeatureSurface>>,
    /// RAG indexer + index cache. `None` when RAG is off; `rag_search` /
    /// `rag_list_collections` then surface a clear "not configured" error
    /// rather than silently misroute.
    ///
    /// Deliberately *not* in [`FeatureSurface`]: the indexer owns open handles
    /// and in-flight jobs, and `rag.data_dir` is the one setting that stays
    /// restart-only — a hot swap there would strand the existing index tree at
    /// the old path. See `settings::SECTIONS`.
    pub indexer: Option<Indexer>,
    /// At-rest encryption for the gateway's database-stored secrets: per-user
    /// MCP OAuth tokens, admin-stored connector client secrets, and upstream
    /// backend API keys. `new()` installs an ephemeral key; production overrides
    /// it via [`Self::with_crypto`] with a key derived from
    /// `$GATEWAY_ENCRYPTION_KEY` / the session secret.
    pub crypto: Arc<Crypto>,
    /// Per-user MCP connection manager: live connections to each user's
    /// connected connectors + the per-request tool overlay. `new()` installs
    /// one bound to the same pool + ephemeral crypto; production overrides via
    /// [`Self::with_mcp`].
    pub mcp: Arc<crate::server::tools::mcp::manager::McpConnectionManager>,
    /// Web Push sender (VAPID keypair + HTTP client). `None` when `[push]
    /// enabled = false`; the push endpoints then report "disabled" and the
    /// turn-complete hook is a no-op. Built at startup by [`Self::with_push`].
    pub push: Option<Arc<gateway_features::server::push::PushSender>>,
}

impl AppState {
    pub fn new(
        config: Config,
        db: Pool,
        upstreams: Arc<UpstreamRegistry>,
        tools: Arc<ToolRegistry>,
        rbac: Arc<Resolver>,
    ) -> Self {
        let crypto = Arc::new(Crypto::ephemeral());
        let mcp = crate::server::tools::mcp::manager::McpConnectionManager::new(
            db.clone(),
            crypto.clone(),
        );
        // Seeded from the config file so tests and the dev harness get the
        // historical behaviour; production replaces it with the shared handle
        // via `with_runtime_handle` and then fills it in from the DB.
        let runtime = RuntimeSettings::new_handle(config.public_url_fallback());
        Self {
            http: reqwest::Client::new(),
            config: Arc::new(ArcSwap::from_pointee(config)),
            db,
            runtime,
            upstreams,
            tools: Arc::new(ArcSwap::from(tools)),
            tool_family_builder: None,
            rbac,
            surface: Arc::new(ArcSwap::from_pointee(FeatureSurface::default())),
            indexer: None,
            crypto,
            mcp,
            push: None,
        }
    }

    /// Install the sandbox-runner client (built once at startup when
    /// `[sandbox]` is enabled) so per-turn tool contexts can lease a
    /// container. Off → sandbox calls stay single-use.
    pub fn with_sandbox_client(
        self,
        client: Arc<crate::server::tools::sandbox::SandboxClient>,
    ) -> Self {
        self.mutate_surface(|s| s.sandbox_client = Some(client));
        self
    }

    /// Install the ComfyUI catalog + HTTP client. Built once at startup
    /// when `[comfyui]` is enabled; off → no `comfyui_*` tools register.
    pub fn with_comfyui(self, comfyui: Arc<crate::server::comfyui_tool::ComfyuiHandle>) -> Self {
        self.mutate_surface(|s| s.comfyui = Some(comfyui));
        self
    }

    /// Install the discovered typst templates' display metadata (for the
    /// per-template toggle rows in the tool menu / `/tools` page).
    pub fn with_typst_templates(
        self,
        templates: Vec<crate::server::tools::catalog::TemplateMeta>,
    ) -> Self {
        self.mutate_surface(|s| s.typst_templates = Arc::new(templates));
        self
    }

    /// Skill names this caller may load: the global (operator) skills their
    /// roles permit, unioned with **all** of their private skills — ownership
    /// alone grants a private skill, no RBAC role needed. Global names come
    /// first (RBAC order), then any private names not already present; a
    /// private skill that shadows a global one appears once, under that name.
    /// Empty when `[skills]` isn't configured. The single home for skill
    /// authorization, shared by the chat system-message listing, the
    /// `read_skill`-always-on rule below, and the chat capability rows — so
    /// they can't drift, the same way [`Self::allowed_tools_for_user`] anchors
    /// tools.
    pub fn allowed_skills_for(&self, roles: &[String], user_id: &str) -> Vec<String> {
        let Some(store) = self.skills() else {
            return Vec::new();
        };
        let role_ids = self.rbac.role_ids_for(roles);
        let mut allowed = self.rbac.allowed_skills(&role_ids, &store.current());
        if let Some(user_store) = self.user_skills() {
            for name in user_store.registry_for(user_id).names() {
                if !allowed.iter().any(|a| a == name) {
                    allowed.push(name.to_string());
                }
            }
        }
        allowed
    }

    /// True when per-user private skills are usable right now: the feature is
    /// configured **and** the store's directory is accessible. Drives whether
    /// the `/skills` nav entry is shown — it's hidden when skills aren't
    /// configured, or the directory can't be read/created.
    pub fn user_skills_enabled(&self) -> bool {
        self.user_skills()
            .as_ref()
            .is_some_and(|s| gateway_features::server::skills::dir_accessible(s.root()))
    }

    /// True when the (global) skills feature is configured but its directory
    /// isn't accessible — the admin `/admin/skills` page shows a "no directory
    /// access" message in this state. `false` when unconfigured (which gets a
    /// different message) or when the directory is fine.
    pub fn skills_dir_inaccessible(&self) -> bool {
        self.skills()
            .as_ref()
            .is_some_and(|s| !gateway_features::server::skills::dir_accessible(s.dir()))
    }

    /// The skill registry to resolve names against for `user_id`: their private
    /// skills overlaid on the global operator skills (private shadows global).
    /// `None` when `[skills]` isn't configured. Used by the chat driver's
    /// skill advertisement / re-injection and the chat capability rows, so the
    /// name a caller sees always resolves to the right bundle body.
    pub fn combined_skills_for(
        &self,
        user_id: &str,
    ) -> Option<Arc<gateway_features::server::skills::SkillRegistry>> {
        let global = self.skills()?.current();
        match self.user_skills() {
            Some(user_store) => {
                let private = user_store.registry_for(user_id);
                Some(Arc::new(
                    gateway_features::server::skills::combined_registry(&global, &private),
                ))
            }
            // Skills configured but no per-user store (shouldn't happen — both
            // are wired together): fall back to the global registry alone.
            None => Some(global),
        }
    }

    /// The tool ids a user may actually use this request: the union of
    /// their roles' RBAC grants, minus the tools they turned off on the
    /// `/tools` page. A DB hiccup on the per-user prefs degrades to
    /// "nothing disabled" rather than failing the request. Single home
    /// for the authorization stack so the proxy + chat + regeneration
    /// paths can't drift.
    pub async fn allowed_tools_for_user(&self, roles: &[String], user_id: &str) -> Vec<String> {
        let role_ids = self.rbac.role_ids_for(roles);
        let mut allowed = self.rbac.allowed_tools(&role_ids, &self.tools());
        self.expand_comfyui_tools(&mut allowed, &role_ids);
        let disabled =
            gateway_core::server::db::user_tool_prefs::disabled_for_user(&self.db, user_id)
                .await
                .unwrap_or_default();
        crate::server::tools::catalog::retain_enabled(&mut allowed, &disabled);
        allowed
    }

    /// Append the currently-loaded `comfyui_*` tool ids to `allowed` based
    /// on the RBAC grant (admin / wildcard / explicit list). Centralised so
    /// every call site that asks "what tools does this caller see"
    /// (`/api/v0/me`, `/tools` page, `allowed_tools_for_user`) agrees on
    /// ComfyUI visibility. No-op when `[comfyui]` isn't configured.
    pub fn expand_comfyui_tools(&self, allowed: &mut Vec<String>, role_ids: &[String]) {
        let Some(handle) = self.comfyui() else {
            return;
        };
        match self.rbac.grants_comfyui_overlay(role_ids) {
            gateway_core::server::rbac::resolver::ComfyuiGrant::Wildcard => {
                for m in handle.store.current().workflows() {
                    let id = format!("comfyui_{}", m.id);
                    if !allowed.contains(&id) {
                        allowed.push(id);
                    }
                }
            }
            gateway_core::server::rbac::resolver::ComfyuiGrant::Specific(ids) => {
                let snapshot = handle.store.current();
                for id in ids {
                    let Some(workflow_id) =
                        id.strip_prefix(crate::server::tools::catalog::COMFYUI_PREFIX)
                    else {
                        continue;
                    };
                    if snapshot.lookup(workflow_id).is_some() && !allowed.contains(&id) {
                        allowed.push(id);
                    }
                }
            }
            gateway_core::server::rbac::resolver::ComfyuiGrant::None => {}
        }
    }

    /// The tool ids an **API token** may use this request — the per-token
    /// overlay on top of [`Self::allowed_tools_for_user`]:
    ///
    /// ```text
    /// effective = (rbac_allowed − user_global_disabled − token_disabled)  if tools_enabled
    ///           = ∅                                                       otherwise (DEFAULT)
    /// ```
    ///
    /// The master `tools_enabled` flag defaults off, so a token sees no
    /// gateway tools until its owner opts in; an empty result makes the
    /// proxy take its byte-dumb 1:1 passthrough. Once on, the
    /// `token_tool_prefs` rows subtract individual capabilities (same
    /// toggle-key semantics as the `/tools` page). RBAC + the user's
    /// global toggles stay the outer bound — a token can only ever
    /// *narrow*, never grant. A DB hiccup on the per-token lookup degrades
    /// to "nothing disabled" rather than failing the request. This is the
    /// single home every bearer (`/v1`) path resolves through, so buffered,
    /// streaming, and passthrough can't drift.
    pub async fn allowed_tools_for_token(
        &self,
        ctx: &gateway_core::server::auth::UserCtx,
    ) -> Vec<String> {
        if !ctx.tools_enabled {
            return Vec::new();
        }
        let mut allowed = self.allowed_tools_for_user(&ctx.roles, &ctx.user_id).await;
        let disabled =
            gateway_core::server::db::token_tool_prefs::disabled_for_token(&self.db, &ctx.token_id)
                .await
                .unwrap_or_default();
        crate::server::tools::catalog::retain_enabled(&mut allowed, &disabled);
        // NB: per-user MCP connector tool ids are unioned in by the caller from
        // a once-per-request `UserMcpLayer` (see `union_mcp_tool_ids`), so the
        // advertised set and the executing `CompositeToolSource` never diverge.
        allowed
    }

    /// The tool ids to inject for a turn **in a given conversation**: the
    /// per-user grant from [`Self::allowed_tools_for_user`], narrowed to
    /// `enable_tools` (the always-on bootstrap) plus whatever groups this
    /// conversation has explicitly enabled via `chat_session_tools`. The
    /// per-conversation overlay from `docs/tool-context-optimization.md`:
    ///
    /// ```text
    /// effective = (rbac_allowed − user_global_disabled)
    ///           ∩ ({enable_tools} ∪ conversation_enabled)
    /// ```
    ///
    /// RBAC stays the outer bound; this only ever narrows. A DB hiccup on the
    /// per-conversation lookup degrades to "bootstrap only" rather than
    /// failing the turn. The result is ordered bootstrap-first (a byte-stable
    /// prefix shared across conversations) then the per-conversation tail, so
    /// the upstream prefix cache stays warm.
    pub async fn allowed_tools_for_session(
        &self,
        roles: &[String],
        user_id: &str,
        session_id: &str,
    ) -> Vec<String> {
        use crate::server::tools::catalog::{BOOTSTRAP_TOOL_ID, READ_SKILL_ID, entry_key_for};

        let mut allowed = self.allowed_tools_for_user(roles, user_id).await;
        let enabled = gateway_core::server::db::chat_session_tools::enabled_keys_for_session(
            &self.db, session_id,
        )
        .await
        .unwrap_or_default();
        // `read_skill` is always-on (like the `enable_tools` bootstrap) *when*
        // the caller has at least one permitted skill: the system message
        // advertises those skills every turn, so the loader must always be
        // callable — making the model enable it first would be pointless
        // friction. With no permitted skills it stays lazy (and is usually not
        // even registered), so skill-less deployments are unaffected.
        let skill_loader_on = allowed.iter().any(|id| id == READ_SKILL_ID)
            && !self.allowed_skills_for(roles, user_id).is_empty();
        allowed.retain(|id| {
            id == BOOTSTRAP_TOOL_ID
                || (skill_loader_on && id == READ_SKILL_ID)
                || enabled.contains(entry_key_for(id))
        });
        // Deterministic, cache-friendly order: enable_tools first (identical
        // across every conversation), then the per-conversation tail sorted
        // by toggle key then id.
        allowed.sort_by(|a, b| {
            let a_boot = a == BOOTSTRAP_TOOL_ID;
            let b_boot = b == BOOTSTRAP_TOOL_ID;
            b_boot
                .cmp(&a_boot)
                .then_with(|| entry_key_for(a).cmp(entry_key_for(b)))
                .then_with(|| a.as_str().cmp(b.as_str()))
        });
        // NB: per-user MCP connector tool ids are account-level and unioned in
        // by the caller from a once-per-request `UserMcpLayer` (after this
        // per-conversation narrowing), so the advertised set matches what the
        // `CompositeToolSource` can actually execute.
        allowed
    }

    /// Adopt an externally created handle, so things built before `AppState`
    /// (the sandbox client) observe the same live settings rather than a
    /// snapshot. Must be called before anything reads the settings.
    pub fn with_runtime_handle(mut self, runtime: RuntimeHandle) -> Self {
        self.runtime = runtime;
        self
    }

    /// Replace the wizard-owned settings wholesale. Tests use it to inject a
    /// mock OIDC client; production goes through [`Self::reload_runtime`], so
    /// there is exactly one place that knows how to derive these from the DB.
    pub fn set_runtime(&self, settings: RuntimeSettings) {
        self.runtime.store(Arc::new(RuntimeSettings {
            // Normalise once, here, so no reader has to remember to trim. Every
            // absolute URL the gateway builds is `public_url` + an absolute
            // path, and a stored trailing slash would double the separator.
            public_url: settings.public_url.trim_end_matches('/').to_string(),
            ..settings
        }));
    }

    /// Re-derive the wizard-owned settings from the database and swap them in.
    ///
    /// The counterpart to [`Self::reload_rbac`], and for the same reason: the
    /// DB is the source of truth, so the DB→memory derivation should live in
    /// one place rather than being written out by every caller. Called once at
    /// boot and again the moment the setup wizard finishes — which is what
    /// makes `/login` work immediately, with no restart.
    ///
    /// Building the OIDC client does network I/O (provider discovery); a
    /// failure leaves the client `None`, which `/auth/login` reports cleanly.
    pub async fn reload_runtime(&self) {
        use gateway_core::server::{oidc_settings, setup};

        let setup_completed = setup::is_completed(&self.db).await.unwrap_or(false);
        let public_url = setup::public_url(&self.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| self.config().public_url_fallback().to_string());

        let oidc = match oidc_settings::params(&self.db, &self.crypto).await {
            Ok(Some(params)) => build_oidc_with_retry(&params, &public_url).await,
            Ok(None) => {
                if setup_completed {
                    tracing::error!(
                        "no OIDC provider is configured but setup is marked complete — nobody \
                         can sign in. Run `restore-setup` on the host to reopen the wizard."
                    );
                } else {
                    tracing::info!(
                        %public_url,
                        "no OIDC provider configured yet — open the gateway in a browser to run setup"
                    );
                }
                None
            }
            Err(err) => {
                tracing::error!(error = %err, "reading OIDC settings");
                None
            }
        };

        self.set_runtime(RuntimeSettings {
            public_url,
            oidc,
            setup_completed,
        });
    }

    /// The effective configuration.
    ///
    /// Returns an owned snapshot rather than a borrow, so a concurrent
    /// `/admin/settings` save cannot swap the config out from under a request
    /// halfway through. Bind it once per request (`let config = state.config();`)
    /// and read fields off that; calling it repeatedly is correct but pointless.
    pub fn config(&self) -> Arc<Config> {
        self.config.load_full()
    }

    /// Re-read the settings-owned blocks from the database and swap in a
    /// config that has them.
    ///
    /// Called once at boot and again whenever `/admin/settings` saves, which is
    /// what makes an edited value live for the next request without a restart.
    /// Blocks whose value was baked into something built at boot — the sandbox
    /// and ComfyUI clients, the loaded GeoIP database, the scanned skills and
    /// Typst directories, the RAG indexer — are marked `restart` in
    /// [`SECTIONS`] and genuinely do need one; this swap is what makes every
    /// *other* field take effect immediately.
    ///
    /// Re-applies onto the *current* config, which is safe because [`apply`]
    /// overwrites all twelve blocks wholesale and touches nothing else:
    /// `[bind]`, `[db]`, `[gateway]`, the topology and the seed-only blocks
    /// survive untouched, however many times this runs.
    ///
    /// [`SECTIONS`]: gateway_core::server::settings::SECTIONS
    /// [`apply`]: gateway_core::server::settings::apply
    pub async fn reload_settings(&self) {
        use gateway_core::server::settings;

        let stored = match settings::load(&self.db, &self.crypto).await {
            Ok(s) => s,
            Err(err) => {
                // Keep serving on the configuration we already have. The
                // alternative — reverting to file defaults because one query
                // failed — would silently turn features off.
                tracing::error!(error = %err, "reading operator settings; keeping the current configuration");
                return;
            }
        };
        let mut next = (*self.config()).clone();
        settings::apply(&stored, &mut next);
        let next = Arc::new(next);
        // The features before the config, so nothing can observe a config that
        // claims a feature is on while its client is still absent. Both are
        // single `ArcSwap` stores, so a reader sees one or the other, never a
        // half-built pair.
        self.rebuild_surface(&next);
        // The tool family last, from the freshly built bundle: a typst tool
        // holds the sandbox client (for its PPTX export), so it must be built
        // against the new one rather than the one being replaced.
        if let Some(build) = self.tool_family_builder.clone() {
            let family = build(&next, &self.surface());
            let rebuilt = self
                .tools()
                .with_family_replaced(gateway_core::server::tool_naming::TYPST_PREFIX, family);
            self.tools.store(Arc::new(rebuilt));
        }
        self.config.store(next);
    }

    /// Rebuild the feature bundle from `config` and swap it in.
    ///
    /// This is what makes `/admin/settings` take effect without a restart for
    /// everything except `rag.data_dir`: each member is re-derived from the
    /// block that owns it — a client re-pointed at a new URL, a directory
    /// re-scanned, a database re-opened — and the whole bundle is replaced at
    /// once.
    ///
    /// Failures are per-member and non-fatal. A GeoIP file that has gone
    /// missing leaves `geoip: None`, which the tool already reports cleanly;
    /// aborting the reload instead would leave the *other* settings unapplied
    /// and the operator staring at a form that saved but changed nothing.
    fn rebuild_surface(&self, config: &Config) {
        // Display metadata only; the per-template *tools* come from the
        // family builder further down, which is what actually makes
        // `typst.templates_dir` take effect without a restart.
        let typst_templates = Arc::new(typst_template_metas(config));
        let (skills, user_skills) = match config.skills.as_ref() {
            Some(cfg) => {
                let store = Arc::new(SkillStore::load(cfg.dir.clone()));
                let users_dir = cfg.dir.join(".users");
                if let Err(err) = std::fs::create_dir_all(&users_dir) {
                    tracing::warn!(
                        error = %err, dir = %users_dir.display(),
                        "could not create the private-skills directory"
                    );
                }
                let user_store = Arc::new(gateway_features::server::skills::UserSkillStore::new(
                    users_dir,
                ));
                (Some(store), Some(user_store))
            }
            None => (None, None),
        };
        // `GeoIp::new` is lazy and watches its file, so only a changed *path*
        // needs a new handle. Reuse the existing one when the path is
        // unchanged, or the watcher would be replaced on every unrelated save.
        let geoip = match config.geoip.as_ref() {
            Some(cfg) => {
                let current = self.surface().geoip.clone();
                match current {
                    Some(geo) if geo.db_path() == cfg.db_path => Some(geo),
                    _ => {
                        let geo = GeoIp::new(cfg.db_path.clone());
                        geo.watch();
                        Some(geo)
                    }
                }
            }
            None => None,
        };
        let sandbox_client = config.sandbox.as_ref().filter(|c| c.enabled).map(|cfg| {
            crate::server::tools::sandbox::SandboxClient::new(
                Arc::new(cfg.clone()),
                self.runtime.clone(),
            )
        });
        // ComfyUI keeps whatever handle it has. The handle owns a workflow
        // catalog an admin may have reloaded through `/admin/comfyui`, and
        // rebuilding it here would silently discard that; building a *new* one
        // also needs the S3 config and the chat-update registry that only the
        // boot path assembles. Its `restart` badge stays until that moves in
        // here too — but `/admin/comfyui/reload` already re-scans the catalog
        // without one.
        let comfyui = self.surface().comfyui.clone();

        self.set_surface(FeatureSurface {
            typst_templates,
            skills,
            user_skills,
            geoip,
            sandbox_client,
            comfyui,
        });
    }

    /// The tool registry. One `ArcSwap` load; bind it once per request rather
    /// than calling this repeatedly.
    pub fn tools(&self) -> Arc<ToolRegistry> {
        self.tools.load_full()
    }

    /// Install the closure that rebuilds the settings-dependent tool family.
    /// Called once at boot by the binary crate, the only place that can name
    /// the concrete tool types. See [`ToolFamilyBuilder`].
    pub fn with_tool_family_builder(mut self, builder: ToolFamilyBuilder) -> Self {
        self.tool_family_builder = Some(builder);
        self
    }

    /// The live feature bundle. One `ArcSwap` load; bind it once if you need
    /// several of its members in one request.
    pub fn surface(&self) -> Arc<FeatureSurface> {
        self.surface.load_full()
    }

    /// Read-modify-write one member of the bundle.
    ///
    /// Only for the `with_*` builders, which run at boot before anything else
    /// holds the state, so the read-then-store is not racing a reader. Runtime
    /// changes go through [`Self::reload_settings`], which builds a whole
    /// bundle and swaps it once.
    fn mutate_surface(&self, f: impl FnOnce(&mut FeatureSurface)) {
        let mut next = (*self.surface()).clone();
        f(&mut next);
        self.surface.store(Arc::new(next));
    }

    /// Replace the feature bundle wholesale. Tests use it to inject a store or
    /// a client; production goes through [`Self::reload_settings`].
    pub fn set_surface(&self, surface: FeatureSurface) {
        self.surface.store(Arc::new(surface));
    }

    pub fn geoip(&self) -> Option<GeoIp> {
        self.surface().geoip.clone()
    }

    pub fn skills(&self) -> Option<Arc<SkillStore>> {
        self.surface().skills.clone()
    }

    pub fn user_skills(&self) -> Option<Arc<gateway_features::server::skills::UserSkillStore>> {
        self.surface().user_skills.clone()
    }

    pub fn typst_templates(&self) -> Arc<Vec<crate::server::tools::catalog::TemplateMeta>> {
        self.surface().typst_templates.clone()
    }

    pub fn sandbox_client(&self) -> Option<Arc<crate::server::tools::sandbox::SandboxClient>> {
        self.surface().sandbox_client.clone()
    }

    pub fn comfyui(&self) -> Option<Arc<crate::server::comfyui_tool::ComfyuiHandle>> {
        self.surface().comfyui.clone()
    }

    /// The gateway's own base URL, no trailing slash.
    pub fn public_url(&self) -> String {
        self.runtime.load().public_url.clone()
    }

    /// The live OIDC client, or `None` when sign-in is not configured.
    pub fn oidc(&self) -> Option<Arc<OidcClient>> {
        self.runtime.load().oidc.clone()
    }

    /// False while the gateway is in first-run mode.
    pub fn setup_completed(&self) -> bool {
        self.runtime.load().setup_completed
    }

    pub fn with_geoip(self, geoip: GeoIp) -> Self {
        self.mutate_surface(|s| s.geoip = Some(geoip));
        self
    }

    pub fn with_indexer(mut self, indexer: Indexer) -> Self {
        self.indexer = Some(indexer);
        self
    }

    pub fn with_skills(self, skills: Arc<SkillStore>) -> Self {
        self.mutate_surface(|s| s.skills = Some(skills));
        self
    }

    /// Install the per-user private-skills store. Wired at startup alongside
    /// [`Self::with_skills`] (both gate on `[skills]`), so `user_skills` is
    /// `Some` exactly when `skills` is.
    pub fn with_user_skills(
        self,
        user_skills: Arc<gateway_features::server::skills::UserSkillStore>,
    ) -> Self {
        self.mutate_surface(|s| s.user_skills = Some(user_skills));
        self
    }

    /// Install the Web Push sender (VAPID keypair + HTTP client). Called at
    /// startup only when `[push] enabled = true`.
    pub fn with_push(mut self, push: Arc<gateway_features::server::push::PushSender>) -> Self {
        self.push = Some(push);
        self
    }

    /// The providers this gateway can index from.
    ///
    /// Falls back to the built-in set when no indexer is wired (RAG is not
    /// configured here), so a source picker still renders and the operator
    /// sees what *would* be available rather than a form with a silently
    /// missing control. Lives here rather than in each caller so one process
    /// has one registry: the web pages and the JSON API previously each held
    /// their own `LazyLock` fallback.
    pub fn provider_registry(&self) -> &gateway_features::server::rag::source::ProviderRegistry {
        use gateway_features::server::rag::source::ProviderRegistry;
        static FALLBACK: std::sync::LazyLock<ProviderRegistry> =
            std::sync::LazyLock::new(ProviderRegistry::with_builtins);
        match self.indexer.as_ref() {
            Some(indexer) => indexer.providers(),
            None => &FALLBACK,
        }
    }

    /// Install the production at-rest encryption key, rebuilding the MCP
    /// connection manager so it seals/opens its tokens under the same key.
    pub fn with_crypto(mut self, crypto: Arc<Crypto>) -> Self {
        self.mcp = crate::server::tools::mcp::manager::McpConnectionManager::new(
            self.db.clone(),
            crypto.clone(),
        );
        self.crypto = crypto;
        self
    }

    /// Resolved gateway-group ids for a caller's raw OIDC group claims — the
    /// "effective groups" seam. Used when building the per-request MCP layer and
    /// when gating pools / RAG collections so resource `allowed_groups` are
    /// enforced at exposure time.
    pub fn role_ids_for(&self, roles: &[String]) -> Vec<String> {
        self.rbac.role_ids_for(roles)
    }

    /// Build a caller's [`PoolAccess`] from their raw OIDC group claims — the
    /// per-request gate threaded into the group-aware upstream listing/routing
    /// methods so pool `allowed_groups` are enforced. Admins bypass.
    pub fn pool_access_for(&self, roles: &[String]) -> gateway_core::server::upstreams::PoolAccess {
        let role_ids = self.rbac.role_ids_for(roles);
        let is_admin = self.rbac.is_admin(&role_ids);
        gateway_core::server::upstreams::PoolAccess {
            role_ids,
            is_admin,
            allowed_models: None,
        }
    }

    /// [`Self::pool_access_for`] plus the calling API token's model
    /// allowlist — the `/v1` variant. Every bearer-authenticated handler
    /// builds its access this way, so a token's model restriction applies to
    /// listing and routing alike without each handler remembering to ask.
    ///
    /// No database read: the allowlist was resolved during auth (see
    /// `require_bearer`), where a failure to read it fails the request rather
    /// than downgrading the token to unrestricted.
    pub fn pool_access_for_token(
        &self,
        ctx: &gateway_core::server::auth::UserCtx,
    ) -> gateway_core::server::upstreams::PoolAccess {
        gateway_core::server::upstreams::PoolAccess {
            allowed_models: ctx.allowed_models.clone(),
            ..self.pool_access_for(&ctx.roles)
        }
    }

    /// Reload the RBAC resolver from the DB after an admin edit to groups,
    /// OIDC mappings, or tool grants (`/admin/groups`). Also refreshes the
    /// skill-grant overlay. Best-effort: a DB hiccup leaves the previous
    /// snapshot in place and logs, rather than clearing access.
    pub async fn reload_rbac(&self) {
        match gateway_core::server::db::gateway_groups::load_snapshot(&self.db).await {
            Ok(snap) => self.rbac.reload(snap),
            Err(e) => {
                tracing::warn!(error = %e, "reloading RBAC groups; keeping previous snapshot")
            }
        }
        match gateway_core::server::db::skill_grants::all(&self.db).await {
            Ok(grants) => self.rbac.set_skill_grant_overlay(grants),
            Err(e) => tracing::warn!(error = %e, "reloading skill-grant overlay"),
        }
    }

    /// Union a once-per-request [`UserMcpLayer`]'s tool ids into an
    /// already-resolved registry `allowed` set. Keeping the layer the *single*
    /// source of both the advertised ids and the executing
    /// `CompositeToolSource` is what guarantees they can't diverge.
    pub fn union_mcp_tool_ids(
        &self,
        allowed: &mut Vec<String>,
        layer: &crate::server::tools::mcp::manager::UserMcpLayer,
    ) {
        for id in layer.tool_ids() {
            if !allowed.iter().any(|a| a == &id) {
                allowed.push(id);
            }
        }
    }

    /// Like [`Self::union_mcp_tool_ids`], but only unions the tools whose
    /// connector toggle key is in `enabled_keys` — the per-conversation overlay
    /// (`chat_session_tools`). This is what makes per-user MCP connectors
    /// *progressive* on the chat path: a connected-but-not-enabled connector
    /// contributes no tool schemas (it's advertised in the system context
    /// instead), and only enabling it — by the model via `enable_tools` or the
    /// user via the composer — surfaces its tools. The `/v1` path keeps the
    /// unconditional [`Self::union_mcp_tool_ids`] (API clients manage their own
    /// context and have no conversation overlay).
    pub fn union_enabled_mcp_tool_ids(
        &self,
        allowed: &mut Vec<String>,
        layer: &crate::server::tools::mcp::manager::UserMcpLayer,
        enabled_keys: &std::collections::HashSet<String>,
    ) {
        for id in layer.enabled_tool_ids(enabled_keys) {
            if !allowed.iter().any(|a| a == &id) {
                allowed.push(id);
            }
        }
    }
}
