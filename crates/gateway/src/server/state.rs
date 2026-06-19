// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use std::sync::Arc;

use crate::server::auth::oidc::OidcClient;
use crate::server::config::Config;
use crate::server::db::Pool;
use crate::server::geoip::GeoIp;
use crate::server::rag::worker::Indexer;
use crate::server::rbac::Resolver;
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
}

impl AppState {
    pub fn new(
        config: Config,
        db: Pool,
        upstreams: Arc<UpstreamRegistry>,
        tools: Arc<ToolRegistry>,
        rbac: Arc<Resolver>,
    ) -> Self {
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

        let mut allowed = self.allowed_tools_for_user(roles, user_id).await;
        let enabled =
            crate::server::db::chat_session_tools::enabled_keys_for_session(&self.db, session_id)
                .await
                .unwrap_or_default();
        allowed.retain(|id| id == BOOTSTRAP_TOOL_ID || enabled.contains(entry_key_for(id)));
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
            models: vec!["*".into()],
            tools: vec!["*".into()],
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
            token_id: token_id.into(),
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
