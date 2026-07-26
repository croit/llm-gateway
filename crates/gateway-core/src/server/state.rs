// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use std::sync::Arc;

use crate::server::auth::oidc::OidcClient;
use crate::server::config::Config;
use crate::server::crypto::Crypto;
use crate::server::db::Pool;
use crate::server::geoip::GeoIp;
use crate::server::rag::worker::Indexer;
use crate::server::rbac::Resolver;
use crate::server::skills::SkillStore;
use crate::server::tools::ToolRegistry;
use crate::server::upstreams::UpstreamRegistry;

/// Shared application state injected into Axum handlers.
///
/// Clone is cheap — every field is either Arc-shared or already cloneable
/// without I/O.
#[derive(Clone)]
pub struct AppState {
    pub http: reqwest::Client,
    pub config: Arc<Config>,
    pub db: Pool,
    pub oidc: Option<Arc<OidcClient>>,
    pub upstreams: Arc<UpstreamRegistry>,
    pub tools: Arc<ToolRegistry>,
    pub rbac: Arc<Resolver>,
    /// Client-IP → location resolver for the `get_user_location` tool.
    /// `None` when `[geoip]` isn't configured; the tool then relies on
    /// the browser-provided position alone. Cheap to clone (Arc inside).
    pub geoip: Option<GeoIp>,
    /// RAG indexer + index cache. `None` when `[rag]` isn't configured;
    /// `rag_search` / `rag_list_collections` then surface a clear "not
    /// configured" error rather than silently misroute.
    pub indexer: Option<Indexer>,
    /// Loaded Agent Skills, behind a hot-reloadable store (admin upload /
    /// delete re-scan and swap it live). `None` when `[skills]` isn't
    /// configured; an empty store is fine (uploads populate it without a
    /// restart). RBAC narrows which skills each caller sees (see
    /// [`Self::allowed_skills_for`]).
    pub skills: Option<Arc<SkillStore>>,
    /// Per-user **private** Agent Skills (the user-owned counterpart to
    /// [`Self::skills`]). `None` unless `[skills]` is configured — private
    /// skills piggyback on the same feature toggle and live under
    /// `<skills.dir>/.users/`. Ownership is the grant: a user may load any
    /// skill they own without an RBAC role, and their private skills overlay
    /// the global set (private shadows global; see
    /// [`crate::server::skills::combined_registry`]).
    pub user_skills: Option<Arc<crate::server::skills::UserSkillStore>>,
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
    /// Display metadata for the discovered typst templates (manifest title +
    /// description), snapshotted at startup. The catalog needs it to render a
    /// per-template toggle row — the human title isn't in the tool schema.
    /// Empty when `[typst]` isn't configured.
    pub typst_templates: Arc<Vec<crate::server::tools::catalog::TemplateMeta>>,
    /// Web Push sender (VAPID keypair + HTTP client). `None` when `[push]
    /// enabled = false`; the push endpoints then report "disabled" and the
    /// turn-complete hook is a no-op. Built at startup by [`Self::with_push`].
    pub push: Option<Arc<crate::server::push::PushSender>>,
    /// The sandbox-runner HTTP client, shared so the per-turn tool context can
    /// build a [`crate::server::tools::sandbox::SandboxLease`] (the container
    /// kept alive across a turn's tool rounds). `None` when `[sandbox]` is
    /// absent/disabled — leasing is off and every sandbox call is single-use.
    pub sandbox_client: Option<Arc<crate::server::tools::sandbox::SandboxClient>>,
    /// Hot-reloadable ComfyUI workflow catalog + HTTP client. `None` when
    /// `[comfyui]` is absent/disabled — no `comfyui_*` tools register. The
    /// tool source reads the live snapshot per request, so an admin
    /// `POST /api/v0/comfyui/reload` takes effect immediately.
    pub comfyui: Option<Arc<crate::server::comfyui::ComfyuiHandle>>,
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
        Self {
            http: reqwest::Client::new(),
            config: Arc::new(config),
            db,
            oidc: None,
            upstreams,
            tools,
            rbac,
            geoip: None,
            indexer: None,
            skills: None,
            user_skills: None,
            crypto,
            mcp,
            typst_templates: Arc::new(Vec::new()),
            push: None,
            sandbox_client: None,
            comfyui: None,
        }
    }

    /// Install the sandbox-runner client (built once at startup when
    /// `[sandbox]` is enabled) so per-turn tool contexts can lease a
    /// container. Off → sandbox calls stay single-use.
    pub fn with_sandbox_client(
        mut self,
        client: Arc<crate::server::tools::sandbox::SandboxClient>,
    ) -> Self {
        self.sandbox_client = Some(client);
        self
    }

    /// Install the ComfyUI catalog + HTTP client. Built once at startup
    /// when `[comfyui]` is enabled; off → no `comfyui_*` tools register.
    pub fn with_comfyui(mut self, comfyui: Arc<crate::server::comfyui::ComfyuiHandle>) -> Self {
        self.comfyui = Some(comfyui);
        self
    }

    /// Install the discovered typst templates' display metadata (for the
    /// per-template toggle rows in the tool menu / `/tools` page).
    pub fn with_typst_templates(
        mut self,
        templates: Vec<crate::server::tools::catalog::TemplateMeta>,
    ) -> Self {
        self.typst_templates = Arc::new(templates);
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
        let Some(store) = self.skills.as_ref() else {
            return Vec::new();
        };
        let role_ids = self.rbac.role_ids_for(roles);
        let mut allowed = self.rbac.allowed_skills(&role_ids, &store.current());
        if let Some(user_store) = self.user_skills.as_ref() {
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
        self.user_skills
            .as_ref()
            .is_some_and(|s| crate::server::skills::dir_accessible(s.root()))
    }

    /// True when the (global) skills feature is configured but its directory
    /// isn't accessible — the admin `/admin/skills` page shows a "no directory
    /// access" message in this state. `false` when unconfigured (which gets a
    /// different message) or when the directory is fine.
    pub fn skills_dir_inaccessible(&self) -> bool {
        self.skills
            .as_ref()
            .is_some_and(|s| !crate::server::skills::dir_accessible(s.dir()))
    }

    /// The skill registry to resolve names against for `user_id`: their private
    /// skills overlaid on the global operator skills (private shadows global).
    /// `None` when `[skills]` isn't configured. Used by the chat driver's
    /// skill advertisement / re-injection and the chat capability rows, so the
    /// name a caller sees always resolves to the right bundle body.
    pub fn combined_skills_for(
        &self,
        user_id: &str,
    ) -> Option<Arc<crate::server::skills::SkillRegistry>> {
        let global = self.skills.as_ref()?.current();
        match self.user_skills.as_ref() {
            Some(user_store) => {
                let private = user_store.registry_for(user_id);
                Some(Arc::new(crate::server::skills::combined_registry(
                    &global, &private,
                )))
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
        let mut allowed = self.rbac.allowed_tools(&role_ids, &self.tools);
        self.expand_comfyui_tools(&mut allowed, &role_ids);
        let disabled = crate::server::db::user_tool_prefs::disabled_for_user(&self.db, user_id)
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
        let Some(handle) = self.comfyui.as_ref() else {
            return;
        };
        match self.rbac.grants_comfyui_overlay(role_ids) {
            crate::server::rbac::resolver::ComfyuiGrant::Wildcard => {
                for m in handle.store.current().workflows() {
                    let id = format!("comfyui_{}", m.id);
                    if !allowed.contains(&id) {
                        allowed.push(id);
                    }
                }
            }
            crate::server::rbac::resolver::ComfyuiGrant::Specific(ids) => {
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
            crate::server::rbac::resolver::ComfyuiGrant::None => {}
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
    pub async fn allowed_tools_for_token(&self, ctx: &crate::server::auth::UserCtx) -> Vec<String> {
        if !ctx.tools_enabled {
            return Vec::new();
        }
        let mut allowed = self.allowed_tools_for_user(&ctx.roles, &ctx.user_id).await;
        let disabled =
            crate::server::db::token_tool_prefs::disabled_for_token(&self.db, &ctx.token_id)
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
        let enabled =
            crate::server::db::chat_session_tools::enabled_keys_for_session(&self.db, session_id)
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

    pub fn with_oidc(mut self, oidc: Arc<OidcClient>) -> Self {
        self.oidc = Some(oidc);
        self
    }

    pub fn with_geoip(mut self, geoip: GeoIp) -> Self {
        self.geoip = Some(geoip);
        self
    }

    pub fn with_indexer(mut self, indexer: Indexer) -> Self {
        self.indexer = Some(indexer);
        self
    }

    pub fn with_skills(mut self, skills: Arc<SkillStore>) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Install the per-user private-skills store. Wired at startup alongside
    /// [`Self::with_skills`] (both gate on `[skills]`), so `user_skills` is
    /// `Some` exactly when `skills` is.
    pub fn with_user_skills(
        mut self,
        user_skills: Arc<crate::server::skills::UserSkillStore>,
    ) -> Self {
        self.user_skills = Some(user_skills);
        self
    }

    /// Install the Web Push sender (VAPID keypair + HTTP client). Called at
    /// startup only when `[push] enabled = true`.
    pub fn with_push(mut self, push: Arc<crate::server::push::PushSender>) -> Self {
        self.push = Some(push);
        self
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
    pub fn pool_access_for(&self, roles: &[String]) -> crate::server::upstreams::PoolAccess {
        let role_ids = self.rbac.role_ids_for(roles);
        let is_admin = self.rbac.is_admin(&role_ids);
        crate::server::upstreams::PoolAccess { role_ids, is_admin }
    }

    /// Reload the RBAC resolver from the DB after an admin edit to groups,
    /// OIDC mappings, or tool grants (`/admin/groups`). Also refreshes the
    /// skill-grant overlay. Best-effort: a DB hiccup leaves the previous
    /// snapshot in place and logs, rather than clearing access.
    pub async fn reload_rbac(&self) {
        match crate::server::db::gateway_groups::load_snapshot(&self.db).await {
            Ok(snap) => self.rbac.reload(snap),
            Err(e) => {
                tracing::warn!(error = %e, "reloading RBAC groups; keeping previous snapshot")
            }
        }
        match crate::server::db::skill_grants::all(&self.db).await {
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
