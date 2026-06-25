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
    /// At-rest encryption for per-user MCP OAuth tokens + admin-stored
    /// connector client secrets. `new()` installs an ephemeral key; production
    /// overrides it via [`Self::with_mcp_crypto`] with a key derived from
    /// `$GATEWAY_MCP_KEY` / the session secret.
    pub mcp_crypto: Arc<Crypto>,
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
}

impl AppState {
    pub fn new(
        config: Config,
        db: Pool,
        upstreams: Arc<UpstreamRegistry>,
        tools: Arc<ToolRegistry>,
        rbac: Arc<Resolver>,
    ) -> Self {
        let mcp_crypto = Arc::new(Crypto::ephemeral());
        let mcp = crate::server::tools::mcp::manager::McpConnectionManager::new(
            db.clone(),
            mcp_crypto.clone(),
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
            mcp_crypto,
            mcp,
            typst_templates: Arc::new(Vec::new()),
        }
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

    /// Skill names this caller's roles permit, intersected with what's
    /// loaded. Empty when `[skills]` isn't configured. The single home for
    /// skill authorization, shared by the chat system-message listing, the
    /// `read_skill`-always-on rule below, and the admin page — so they can't
    /// drift, the same way [`Self::allowed_tools_for_user`] anchors tools.
    pub fn allowed_skills_for(&self, roles: &[String]) -> Vec<String> {
        let Some(store) = self.skills.as_ref() else {
            return Vec::new();
        };
        let role_ids = self.rbac.role_ids_for(roles);
        self.rbac.allowed_skills(&role_ids, &store.current())
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
        let disabled = crate::server::db::user_tool_prefs::disabled_for_user(&self.db, user_id)
            .await
            .unwrap_or_default();
        crate::server::tools::catalog::retain_enabled(&mut allowed, &disabled);
        allowed
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
        use crate::server::tools::catalog::{BOOTSTRAP_TOOL_ID, entry_key_for};
        use crate::server::tools::read_skill::READ_SKILL_ID;

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
            && !self.allowed_skills_for(roles).is_empty();
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

    /// Install the production MCP encryption key, rebuilding the connection
    /// manager so it seals/opens tokens under the same key.
    pub fn with_mcp_crypto(mut self, crypto: Arc<Crypto>) -> Self {
        self.mcp = crate::server::tools::mcp::manager::McpConnectionManager::new(
            self.db.clone(),
            crypto.clone(),
        );
        self.mcp_crypto = crypto;
        self
    }

    /// Resolved internal role ids for a caller's raw OIDC group claims. Used
    /// when building the per-request MCP layer so the connector `required_role`
    /// gate is enforced at tool-exposure time.
    pub fn role_ids_for(&self, roles: &[String]) -> Vec<String> {
        self.rbac.role_ids_for(roles)
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

#[cfg(test)]
mod skill_overlay_tests {
    use super::*;
    use crate::server::config::Config;
    use crate::server::db;
    use crate::server::rbac::config::{RbacConfig, RoleConfig};
    use crate::server::skills::{Skill, SkillRegistry, SkillStore};
    use crate::server::tools::enable_tools::EnableTools;
    use crate::server::tools::read_skill::{READ_SKILL_ID, ReadSkill};

    /// Build an `AppState` whose single role grants every tool + model and
    /// the given `skill_grant` (`["*"]`, `["brand"]`, or `[]`), with one
    /// skill `brand` loaded and `read_skill` + `enable_tools` registered.
    async fn state_with_skill_grant(skill_grant: &[&str]) -> AppState {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let registry = SkillRegistry::new([Skill {
            name: "brand".into(),
            title: "Brand".into(),
            description: "Enforce the brand.".into(),
            root: std::path::PathBuf::from("/nonexistent"),
        }]);
        let skills = Arc::new(SkillStore::with_registry(
            std::path::PathBuf::from("/nonexistent"),
            registry,
        ));
        let config = Config {
            rbac: RbacConfig {
                default_role: Some("user".into()),
                mappings: vec![],
            },
            roles: vec![RoleConfig {
                id: "user".into(),
                admin: false,
                tools: vec!["*".into()],
                models: vec!["*".into()],
                skills: skill_grant.iter().map(|s| (*s).to_string()).collect(),
            }],
            ..Config::default()
        };
        let rbac = Arc::new(Resolver::build(config.rbac.clone(), config.roles.clone()).unwrap());
        let mut reg = ToolRegistry::new().with(ReadSkill::new(skills.clone(), rbac.clone()));
        let et = EnableTools::from_registry(&reg);
        reg = reg.with(et);
        let upstreams = UpstreamRegistry::new(&config.upstream_pools).unwrap();
        AppState::new(config, pool, upstreams, Arc::new(reg), rbac).with_skills(skills)
    }

    #[tokio::test]
    async fn read_skill_is_always_on_when_caller_has_a_permitted_skill() {
        // Fresh session, nothing enabled: `read_skill` rides in alongside the
        // bootstrap because the role permits the loaded `brand` skill — the
        // model can act on the system-message skill listing immediately, no
        // enable_tools round needed.
        let state = state_with_skill_grant(&["*"]).await;
        let allowed = state
            .allowed_tools_for_session(&["user".into()], "u1", "s1")
            .await;
        assert!(
            allowed.iter().any(|id| id == READ_SKILL_ID),
            "read_skill should be always-on with a permitted skill: {allowed:?}"
        );
    }

    #[tokio::test]
    async fn read_skill_stays_lazy_when_no_skill_is_permitted() {
        // Same loaded skill, but the role grants no skills: `read_skill` must
        // not be force-injected (it's RBAC-granted via `*` but falls back to
        // the normal lazy/enable_tools path).
        let state = state_with_skill_grant(&[]).await;
        let allowed = state
            .allowed_tools_for_session(&["user".into()], "u1", "s1")
            .await;
        assert!(
            !allowed.iter().any(|id| id == READ_SKILL_ID),
            "read_skill must stay lazy with no permitted skill: {allowed:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::auth::UserCtx;
    use crate::server::config::Config;
    use crate::server::db::{self, token_tool_prefs};
    use crate::server::rbac::config::{RbacConfig, RoleConfig};
    use crate::server::tools::search_web::SearchWeb;
    use crate::server::tools::time::CurrentTimestamp;
    use std::collections::HashMap;
    use std::path::Path;

    /// AppState whose single role grants `*` (every registered tool), with
    /// a couple of easy-to-build tools registered. Enough to exercise the
    /// per-token gate without a live upstream.
    async fn star_state() -> AppState {
        let db = db::open(Path::new(":memory:")).await.unwrap();
        let upstreams = UpstreamRegistry::new(&HashMap::new()).unwrap();
        let tools = Arc::new(ToolRegistry::new().with(SearchWeb).with(CurrentTimestamp));
        let role = RoleConfig {
            id: "all".into(),
            admin: false,
            models: vec!["*".into()],
            tools: vec!["*".into()],
            skills: vec![],
        };
        let rbac = Arc::new(
            Resolver::build(
                RbacConfig {
                    default_role: Some("all".into()),
                    mappings: vec![],
                },
                vec![role],
            )
            .unwrap(),
        );
        AppState::new(Config::default(), db, upstreams, tools, rbac)
    }

    fn ctx(token_id: &str, tools_enabled: bool) -> UserCtx {
        UserCtx {
            user_id: "alice".into(),
            user_email: "alice@example.com".into(),
            token_id: token_id.into(),
            token_name: token_id.into(),
            roles: vec![], // empty → default role "all" applies
            tools_enabled,
        }
    }

    /// Seed a user + token so `token_tool_prefs` (FK to tokens) can hold
    /// rows for `token_id`.
    async fn seed_token(state: &AppState, token_id: &str) {
        let now = jiff::Timestamp::now();
        db::users::upsert(
            &state.db,
            &db::users::User {
                id: "alice".into(),
                email: "alice@example.com".into(),
                name: None,
                roles: vec![],
                created_at: now,
                updated_at: now,
                timezone: None,
            },
        )
        .await
        .unwrap();
        db::tokens::insert(
            &state.db,
            &db::tokens::Token {
                id: token_id.into(),
                user_id: "alice".into(),
                name: token_id.into(),
                hash: format!("hash-{token_id}"),
                created_at: now,
                last_used_at: None,
                expires_at: now + jiff::SignedDuration::from_hours(24),
                revoked_at: None,
                tools_enabled: true,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn master_off_yields_no_tools() {
        let state = star_state().await;
        // Default for a token is off → empty → proxy takes byte-dumb path.
        assert!(
            state
                .allowed_tools_for_token(&ctx("tok", false))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn master_on_with_no_prefs_grants_the_full_user_set() {
        let state = star_state().await;
        let got = state.allowed_tools_for_token(&ctx("tok", true)).await;
        assert!(got.contains(&"search_web".to_string()));
        assert!(got.contains(&"get_current_timestamp".to_string()));
    }

    #[tokio::test]
    async fn master_on_subtracts_a_disabled_capability() {
        let state = star_state().await;
        seed_token(&state, "tok").await;
        token_tool_prefs::set(&state.db, "tok", "search_web", false)
            .await
            .unwrap();
        let got = state.allowed_tools_for_token(&ctx("tok", true)).await;
        assert!(
            !got.contains(&"search_web".to_string()),
            "disabled key removed"
        );
        assert!(
            got.contains(&"get_current_timestamp".to_string()),
            "siblings kept"
        );
    }

    #[tokio::test]
    async fn token_prefs_are_scoped_per_token() {
        // Disabling on one token must not leak to another.
        let state = star_state().await;
        seed_token(&state, "tok-a").await;
        token_tool_prefs::set(&state.db, "tok-a", "search_web", false)
            .await
            .unwrap();
        let other = state.allowed_tools_for_token(&ctx("tok-b", true)).await;
        assert!(other.contains(&"search_web".to_string()));
    }
}
