// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-user MCP connection manager.
//!
//! Holds live connections to the remote MCP servers each user has connected
//! (keyed `(user_id, connector_key)`), lazily (re)establishing them with the
//! user's own OAuth access token — refreshing the token first when it's
//! expired. Connections are cached for [`CACHE_TTL`] so an active conversation
//! reuses one rather than re-handshaking every turn.
//!
//! Per request, [`McpConnectionManager::layer_for_user`] produces a
//! [`UserMcpLayer`] — a [`ToolSource`] overlay of the user's connected-connector
//! tools (minus the ones they set to `off`), which [`CompositeToolSource`]
//! unions on top of the static [`ToolRegistry`] for the tool-call runner.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use shared::api::ToolDef;
use tokio::sync::Mutex;

use super::{ConnectedServer, McpTool, connect_http_server};
use crate::server::auth::mcp_oauth;
use crate::server::crypto::Crypto;
use crate::server::db::Pool;
use crate::server::db::mcp_catalog::{self, Connector};
use crate::server::db::user_mcp::{self, Connection, ToolMode};
use crate::server::tools::{Tool, ToolRegistry, ToolSource};

/// How long a live connection (and its tool listing) is reused before a
/// refresh. Keeps active conversations warm without holding sockets forever.
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);

/// Refresh the access token when it expires within this window (or already
/// has), so a call never races the expiry.
const REFRESH_SKEW_SECS: i64 = 60;

/// Hard cap on live cached connections. Past this, idle/stale entries are
/// evicted (closing their sockets) before inserting a new one — bounds memory
/// + open MCP sessions on a long-running daemon.
const MAX_CACHE_ENTRIES: usize = 256;

struct Cached {
    tools: Vec<Arc<McpTool>>,
    fetched_at: Instant,
}

/// How `ask`-mode tools are treated when building a user's overlay.
#[derive(Clone, Copy)]
pub enum AskContext<'a> {
    /// Chat UI: no per-call approval surface yet, so `ask` tools are hidden
    /// (the user opts them in by setting them to `always` in the store).
    Chat,
    /// `/v1` API for a specific token: `ask` tools are exposed iff that
    /// token's policy allows them (`token_mcp_policy`).
    Api { token_id: &'a str },
}

/// Process-wide manager. Cheap to clone the `Arc` in `AppState`.
pub struct McpConnectionManager {
    db: Pool,
    crypto: Arc<Crypto>,
    http: reqwest::Client,
    cache: Mutex<HashMap<String, Cached>>,
    /// Per-`(user,connector)` refresh locks: serialize token refreshes so the
    /// background worker and a live request can't both spend the same refresh
    /// token (which, with rotation, would invalidate one of them).
    refresh_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl McpConnectionManager {
    pub fn new(db: Pool, crypto: Arc<Crypto>) -> Arc<Self> {
        Arc::new(Self {
            db,
            crypto,
            http: mcp_oauth::discovery_http(),
            cache: Mutex::new(HashMap::new()),
            refresh_locks: Mutex::new(HashMap::new()),
        })
    }

    /// Get-or-create the refresh lock for a `(user, connector)` pair.
    async fn refresh_lock(&self, user_id: &str, connector_key: &str) -> Arc<Mutex<()>> {
        let key = Self::cache_key(user_id, connector_key);
        let mut locks = self.refresh_locks.lock().await;
        locks
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn cache_key(user_id: &str, connector_key: &str) -> String {
        format!("{user_id}\u{1f}{connector_key}")
    }

    /// Drop any cached connection for a user+connector (e.g. on disconnect or
    /// after an auth error) so the next use reconnects fresh.
    pub async fn invalidate(&self, user_id: &str, connector_key: &str) {
        let mut cache = self.cache.lock().await;
        cache.remove(&Self::cache_key(user_id, connector_key));
    }

    /// Build the per-request tool overlay for `user_id`: every connected,
    /// enabled connector's tools. `off` tools are always excluded; how `ask`
    /// tools are treated depends on [`AskContext`] (hidden in chat — where
    /// there's no per-call approval UI yet — and gated by the per-token policy
    /// over the API). A connector that fails to connect/refresh is logged and
    /// skipped — it never fails the whole turn.
    pub async fn layer_for_user(
        &self,
        user_id: &str,
        role_ids: &[String],
        ask: AskContext<'_>,
    ) -> UserMcpLayer {
        let keys = user_mcp::connected_keys(&self.db, user_id)
            .await
            .unwrap_or_default();
        // Resolve every connector concurrently — one slow/unreachable server
        // can't serialise the whole turn behind it.
        let futs = keys.into_iter().map(|key| async move {
            let connector = match mcp_catalog::get(&self.db, &key).await {
                Ok(Some(c)) if c.enabled => c,
                // Connector gone or disabled by the admin → hide its tools.
                _ => return None,
            };
            // Re-check the RBAC gate at exposure time: a role removed (or a
            // `required_role` added) after connecting must drop the tools.
            if let Some(required) = &connector.required_role
                && !role_ids.iter().any(|r| r == required)
            {
                return None;
            }
            // Resolve whether `ask` tools are exposed for this connector.
            let allow_ask = match ask {
                AskContext::Chat => false,
                AskContext::Api { token_id } => matches!(
                    user_mcp::token_ask_policy(&self.db, token_id, &key)
                        .await
                        .unwrap_or(user_mcp::AskOverApi::Block),
                    user_mcp::AskOverApi::Allow
                ),
            };
            let modes = user_mcp::tool_modes(&self.db, user_id, &key)
                .await
                .unwrap_or_default();
            match self.ensure(user_id, &connector).await {
                Ok(tools) => Some((key, tools, modes, allow_ask)),
                Err(err) => {
                    tracing::warn!(user = %user_id, connector = %key, error = %err,
                        "MCP connector unavailable this turn");
                    None
                }
            }
        });
        let resolved = rama::futures::future::join_all(futs).await;
        let mut layer = UserMcpLayer::default();
        for (key, tools, modes, allow_ask) in resolved.into_iter().flatten() {
            layer.add(&key, &tools, &modes, allow_ask);
        }
        layer
    }

    /// Ensure a live connection for `(user, connector)`, returning its tools.
    /// Reuses the cache within [`CACHE_TTL`]; otherwise refreshes the token if
    /// needed and re-handshakes.
    async fn ensure(
        &self,
        user_id: &str,
        connector: &Connector,
    ) -> Result<Vec<Arc<McpTool>>, String> {
        let ck = Self::cache_key(user_id, &connector.key);
        {
            let cache = self.cache.lock().await;
            if let Some(c) = cache.get(&ck)
                && c.fetched_at.elapsed() < CACHE_TTL
            {
                return Ok(c.tools.clone());
            }
        }

        let conn = user_mcp::get_connection(&self.db, user_id, &connector.key)
            .await
            .map_err(|e| format!("loading connection: {e}"))?
            .ok_or_else(|| "not connected".to_string())?;

        let (access, refreshed) = self.access_token(user_id, connector, &conn).await?;
        let connected =
            match connect_http_server(&connector.key, &connector.url, Some(&access)).await {
                Ok(s) => s,
                // The server rejected a token we didn't think was expired
                // (revoked upstream, clock skew, rotated elsewhere). If we haven't
                // already refreshed this pass and a refresh token exists, force one
                // refresh + a single reconnect before giving up.
                Err(e) if !refreshed && conn.refresh_token_ct.is_some() => {
                    tracing::info!(user = %user_id, connector = %connector.key,
                    "MCP connect failed; forcing token refresh + one retry");
                    let new_access = self
                        .refresh(user_id, connector, true)
                        .await
                        .map_err(|re| format!("{e}; forced refresh also failed: {re}"))?;
                    connect_http_server(&connector.key, &connector.url, Some(&new_access))
                        .await
                        .map_err(|e2| format!("reconnect after refresh failed: {e2}"))?
                }
                Err(e) => return Err(e),
            };
        let ConnectedServer { conn: _live, tools } = connected;
        let tools: Vec<Arc<McpTool>> = tools.into_iter().map(Arc::new).collect();

        let mut cache = self.cache.lock().await;
        // Bound the cache: when full and this is a new key, evict stale entries
        // first (dropping them closes their MCP sockets), then the oldest.
        if cache.len() >= MAX_CACHE_ENTRIES && !cache.contains_key(&ck) {
            cache.retain(|_, c| c.fetched_at.elapsed() < CACHE_TTL);
            if cache.len() >= MAX_CACHE_ENTRIES
                && let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, c)| c.fetched_at)
                    .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            ck,
            Cached {
                tools: tools.clone(),
                fetched_at: Instant::now(),
            },
        );
        Ok(tools)
    }

    /// Decrypt the stored access token, refreshing it first when it's expired
    /// (or about to be). Returns `(token, refreshed)` — `refreshed` is true
    /// when a refresh ran, so the caller can avoid a redundant forced refresh
    /// on a subsequent connect failure. On refresh failure the connection is
    /// marked errored.
    async fn access_token(
        &self,
        user_id: &str,
        connector: &Connector,
        conn: &Connection,
    ) -> Result<(String, bool), String> {
        let fresh_enough = conn
            .token_expires_at
            .map(|exp| exp > Timestamp::now() + jiff::Span::new().seconds(REFRESH_SKEW_SECS))
            .unwrap_or(true); // no expiry recorded → assume usable
        if fresh_enough {
            return Ok((self.decrypt_access(conn)?, false));
        }
        // Needs refresh.
        match self.refresh(user_id, connector, false).await {
            Ok(token) => Ok((token, true)),
            Err(err) => {
                let _ = user_mcp::mark_error(&self.db, user_id, &connector.key, &err).await;
                Err(err)
            }
        }
    }

    /// Proactively refresh one connection (background worker entry point):
    /// runs the refresh under the per-connection lock and drops any cached live
    /// connection so the next use picks up the new token. Marks the connection
    /// errored on failure so the store surfaces "needs reconnect".
    pub async fn refresh_connection(
        &self,
        user_id: &str,
        connector_key: &str,
    ) -> Result<(), String> {
        let connector = mcp_catalog::get(&self.db, connector_key)
            .await
            .map_err(|e| format!("loading connector: {e}"))?
            .ok_or_else(|| "connector no longer in catalog".to_string())?;
        match self.refresh(user_id, &connector, true).await {
            Ok(_) => {
                self.invalidate(user_id, connector_key).await;
                Ok(())
            }
            Err(err) => {
                let _ = user_mcp::mark_error(&self.db, user_id, connector_key, &err).await;
                Err(err)
            }
        }
    }

    fn decrypt_access(&self, conn: &Connection) -> Result<String, String> {
        match (&conn.access_token_ct, &conn.access_token_nonce) {
            (Some(ct), Some(nonce)) => self
                .crypto
                .open_str(nonce, ct)
                .map_err(|e| format!("decrypting access token: {e}")),
            _ => Err("no access token stored".into()),
        }
    }

    /// Run the OAuth refresh flow and persist the new tokens. Serialized per
    /// `(user, connector)` so a concurrent refresh (background worker vs a live
    /// request) can't double-spend the refresh token. Reloads the connection
    /// under the lock; unless `force`, returns the existing token if another
    /// task already refreshed it while we waited.
    async fn refresh(
        &self,
        user_id: &str,
        connector: &Connector,
        force: bool,
    ) -> Result<String, String> {
        let lock = self.refresh_lock(user_id, &connector.key).await;
        let _held = lock.lock().await;
        // Reload under the lock so we see any refresh a concurrent task just did.
        let conn = user_mcp::get_connection(&self.db, user_id, &connector.key)
            .await
            .map_err(|e| format!("loading connection: {e}"))?
            .ok_or_else(|| "not connected".to_string())?;
        if !force
            && conn
                .token_expires_at
                .map(|exp| exp > Timestamp::now() + jiff::Span::new().seconds(REFRESH_SKEW_SECS))
                .unwrap_or(false)
        {
            // A concurrent refresh already produced a fresh token — reuse it.
            return self.decrypt_access(&conn);
        }
        let (rt_ct, rt_nonce) = match (&conn.refresh_token_ct, &conn.refresh_token_nonce) {
            (Some(ct), Some(nonce)) => (ct, nonce),
            _ => return Err("access token expired and no refresh token stored — reconnect".into()),
        };
        let refresh_token = self
            .crypto
            .open_str(rt_nonce, rt_ct)
            .map_err(|e| format!("decrypting refresh token: {e}"))?;

        // Reuse the token endpoint resolved + persisted at connect time; only
        // re-run discovery for older connections that predate persistence.
        // Avoids re-fetching (and re-trusting) the MCP server's discovery doc
        // on the long-lived refresh path.
        let token_url = match conn.token_url.clone() {
            Some(u) => u,
            None => {
                let ov = mcp_oauth::Overrides {
                    authorize_url: connector.authorize_url.clone(),
                    token_url: connector.token_url.clone(),
                    registration_url: connector.registration_url.clone(),
                };
                mcp_oauth::discover(&self.http, &connector.url, &ov)
                    .await
                    .map_err(|e| format!("discovery for refresh: {e}"))?
                    .token_url
            }
        };

        let (client_id, client_secret) = self.client_credentials(connector, &conn)?;
        let tokens = mcp_oauth::refresh(
            &self.http,
            &token_url,
            &refresh_token,
            &client_id,
            client_secret.as_deref(),
        )
        .await
        .map_err(|e| format!("token refresh: {e}"))?;

        let access_sealed = self
            .crypto
            .seal_str(&tokens.access_token)
            .map_err(|e| format!("sealing access token: {e}"))?;
        let refresh_sealed = match tokens.refresh_token.as_deref() {
            Some(rt) => Some(
                self.crypto
                    .seal_str(rt)
                    .map_err(|e| format!("sealing refresh token: {e}"))?,
            ),
            None => None,
        };
        user_mcp::update_tokens(
            &self.db,
            user_id,
            &connector.key,
            &access_sealed.ciphertext,
            &access_sealed.nonce,
            refresh_sealed.as_ref().map(|s| s.ciphertext.as_slice()),
            refresh_sealed.as_ref().map(|s| s.nonce.as_slice()),
            tokens.expires_at,
        )
        .await
        .map_err(|e| format!("persisting refreshed tokens: {e}"))?;

        Ok(tokens.access_token)
    }

    /// Resolve `(client_id, client_secret?)` for token requests: the
    /// per-connection DCR client when present, else the catalog's static
    /// client (with its secret decrypted).
    fn client_credentials(
        &self,
        connector: &Connector,
        conn: &Connection,
    ) -> Result<(String, Option<String>), String> {
        if let Some(dcr_id) = &conn.dcr_client_id {
            let secret = match (&conn.dcr_client_secret_ct, &conn.dcr_client_secret_nonce) {
                (Some(ct), Some(nonce)) => Some(
                    self.crypto
                        .open_str(nonce, ct)
                        .map_err(|e| format!("decrypting DCR client secret: {e}"))?,
                ),
                _ => None,
            };
            return Ok((dcr_id.clone(), secret));
        }
        let client_id = connector
            .client_id
            .clone()
            .ok_or_else(|| "connector has no client_id configured".to_string())?;
        let secret = self.decrypt_connector_secret(connector)?;
        Ok((client_id, secret))
    }

    /// Decrypt the catalog connector's static client secret, if any.
    pub fn decrypt_connector_secret(
        &self,
        connector: &Connector,
    ) -> Result<Option<String>, String> {
        match (&connector.client_secret_ct, &connector.client_secret_nonce) {
            (Some(ct), Some(nonce)) => self
                .crypto
                .open_str(nonce, ct)
                .map(Some)
                .map_err(|e| format!("decrypting connector client secret: {e}")),
            _ => Ok(None),
        }
    }

    /// Tool metadata for the connector store UI: every tool the connector
    /// exposes (regardless of the user's `off` choices), with the server's
    /// read-only hint and the user's current effective mode. Connects if
    /// needed (cache-warm otherwise).
    pub async fn connector_tool_infos(
        &self,
        user_id: &str,
        connector: &Connector,
    ) -> Result<Vec<ToolInfo>, String> {
        let tools = self.ensure(user_id, connector).await?;
        let modes = user_mcp::tool_modes(&self.db, user_id, &connector.key)
            .await
            .unwrap_or_default();
        Ok(tools
            .iter()
            .map(|t| {
                let default = default_mode(t.read_only(), t.destructive());
                let mode = modes.get(t.remote_name()).copied().unwrap_or(default);
                ToolInfo {
                    remote_name: t.remote_name().to_string(),
                    description: t.def().function.description.clone(),
                    read_only: t.read_only(),
                    mode,
                }
            })
            .collect())
    }

    /// Access to the shared crypto (for the OAuth handlers that seal tokens).
    pub fn crypto(&self) -> &Crypto {
        &self.crypto
    }

    /// The discovery/token HTTP client (shared with the OAuth handlers).
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }
}

/// Tool metadata for the connector store UI.
pub struct ToolInfo {
    pub remote_name: String,
    pub description: String,
    pub read_only: bool,
    pub mode: ToolMode,
}

/// Default permission tier for a tool when the user hasn't set one: only a
/// destructive non-read tool defaults to `ask`; everything else (reads,
/// queries, and un-annotated tools) defaults to `always`, so a connector the
/// user explicitly connected actually works in chat without pre-authorizing
/// every tool. Destructive tools stay gated (hidden in chat until set to
/// `always`).
fn default_mode(read_only: bool, destructive: bool) -> ToolMode {
    if destructive && !read_only {
        ToolMode::Ask
    } else {
        ToolMode::Always
    }
}

/// Whether a tool with effective `mode` is exposed to the model in a context
/// where `ask`-mode tools are permitted (`allow_ask`). `off` is never exposed;
/// `ask` only when permitted (API token policy — chat hides it for lack of a
/// per-call approval UI); `always` always.
fn expose(mode: ToolMode, allow_ask: bool) -> bool {
    match mode {
        ToolMode::Off => false,
        ToolMode::Ask => allow_ask,
        ToolMode::Always => true,
    }
}

/// Per-request overlay of a user's connected-connector MCP tools.
#[derive(Default)]
pub struct UserMcpLayer {
    tools: HashMap<String, Arc<dyn Tool>>,
    defs: Vec<ToolDef>,
    /// id → effective permission mode (`always` / `ask`; `off` tools are
    /// excluded entirely).
    modes: HashMap<String, ToolMode>,
    /// id → connector key (for the per-token /v1 ask policy).
    connector_of: HashMap<String, String>,
}

impl UserMcpLayer {
    fn add(
        &mut self,
        connector_key: &str,
        tools: &[Arc<McpTool>],
        modes: &HashMap<String, ToolMode>,
        allow_ask: bool,
    ) {
        for tool in tools {
            let mode = modes
                .get(tool.remote_name())
                .copied()
                .unwrap_or_else(|| default_mode(tool.read_only(), tool.destructive()));
            if !expose(mode, allow_ask) {
                continue;
            }
            let id = tool.def().function.name.clone();
            self.tools.insert(id.clone(), tool.clone() as Arc<dyn Tool>);
            self.defs.push(tool.def().clone());
            self.modes.insert(id.clone(), mode);
            self.connector_of.insert(id, connector_key.to_string());
        }
    }

    /// Effective permission mode for a tool id, if this layer owns it.
    pub fn mode_of(&self, id: &str) -> Option<ToolMode> {
        self.modes.get(id).copied()
    }

    /// Connector key a tool id belongs to (for the per-token /v1 policy).
    pub fn connector_of(&self, id: &str) -> Option<&str> {
        self.connector_of.get(id).map(String::as_str)
    }

    /// Tool ids this overlay provides.
    pub fn tool_ids(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// The overlay tool ids whose connector toggle key is in `enabled_keys` —
    /// i.e. the tools the conversation has actually turned on. This is the
    /// progressive-disclosure gate for per-user MCP on the chat path: a
    /// connected-but-not-enabled connector contributes nothing here (only its
    /// system-context advertisement), and enabling `mcp__<connector>` surfaces
    /// every tool that connector bridges (since `entry_key_for(mcp__x__*)` all
    /// collapse to `mcp__x`).
    pub fn enabled_tool_ids(
        &self,
        enabled_keys: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        use crate::server::tools::catalog::entry_key_for;
        self.tools
            .keys()
            .filter(|id| enabled_keys.contains(entry_key_for(id)))
            .cloned()
            .collect()
    }

    /// The distinct connector keys this overlay has tools for, sorted. Used by
    /// the chat driver to advertise connectable integrations in the system
    /// context (progressive disclosure for per-user MCP).
    pub fn connector_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .connector_of
            .values()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        keys.sort();
        keys
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl ToolSource for UserMcpLayer {
    fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(id).cloned()
    }

    fn defs_for(&self, allowed: &[String]) -> Vec<ToolDef> {
        allowed.iter().filter_map(|id| self.find_def(id)).collect()
    }

    fn ids(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    fn contains(&self, id: &str) -> bool {
        self.tools.contains_key(id)
    }
}

impl UserMcpLayer {
    fn find_def(&self, id: &str) -> Option<ToolDef> {
        self.defs.iter().find(|d| d.function.name == id).cloned()
    }
}

/// The static registry unioned with a per-request user MCP overlay. The
/// registry wins on id collisions (built-ins are authoritative).
pub struct CompositeToolSource<'a> {
    registry: &'a ToolRegistry,
    user: &'a UserMcpLayer,
}

impl<'a> CompositeToolSource<'a> {
    pub fn new(registry: &'a ToolRegistry, user: &'a UserMcpLayer) -> Self {
        Self { registry, user }
    }
}

impl ToolSource for CompositeToolSource<'_> {
    fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        ToolSource::get(self.registry, id).or_else(|| self.user.get(id))
    }

    fn defs_for(&self, allowed: &[String]) -> Vec<ToolDef> {
        let mut defs = ToolSource::defs_for(self.registry, allowed);
        // Only add user-overlay defs for ids the registry didn't already
        // provide, preserving `allowed` order for the overlay tail.
        for id in allowed {
            if !self.registry.contains(id)
                && let Some(def) = self.user.find_def(id)
            {
                defs.push(def);
            }
        }
        defs
    }

    fn ids(&self) -> Vec<String> {
        let mut ids = ToolSource::ids(self.registry);
        ids.extend(self.user.ids());
        ids
    }

    fn contains(&self, id: &str) -> bool {
        self.registry.contains(id) || self.user.contains(id)
    }
}

#[cfg(test)]
impl UserMcpLayer {
    /// Test-only: inject a tool overlay entry directly, bypassing a live MCP
    /// connection. Lets the composite/union contracts be tested without a
    /// server.
    fn insert_for_test(&mut self, tool: Arc<dyn Tool>, connector_key: &str, mode: ToolMode) {
        let def = tool.schema();
        let id = def.function.name.clone();
        self.defs.push(def);
        self.tools.insert(id.clone(), tool);
        self.modes.insert(id.clone(), mode);
        self.connector_of.insert(id, connector_key.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tools::echo::Echo;

    #[test]
    fn default_mode_only_gates_destructive_writes() {
        // read-only → always; destructive write → ask; everything else → always.
        assert_eq!(default_mode(true, false), ToolMode::Always);
        assert_eq!(default_mode(true, true), ToolMode::Always); // read-only wins
        assert_eq!(default_mode(false, true), ToolMode::Ask);
        assert_eq!(default_mode(false, false), ToolMode::Always); // un-annotated → usable
    }

    #[test]
    fn expose_matrix() {
        // off: never; ask: only when allowed; always: always.
        assert!(!expose(ToolMode::Off, true));
        assert!(!expose(ToolMode::Off, false));
        assert!(expose(ToolMode::Ask, true));
        assert!(!expose(ToolMode::Ask, false));
        assert!(expose(ToolMode::Always, true));
        assert!(expose(ToolMode::Always, false));
    }

    /// A fake MCP-bridged tool whose id is namespaced `mcp__demo__echo`, so its
    /// connector toggle key collapses to `mcp__demo` (like a real connector).
    struct FakeMcpTool;
    impl crate::server::tools::Tool for FakeMcpTool {
        fn id(&self) -> &str {
            "mcp__demo__echo"
        }
        fn schema(&self) -> shared::api::ToolDef {
            shared::api::ToolDef::function(
                "mcp__demo__echo",
                "demo",
                serde_json::json!({"type": "object"}),
            )
        }
        fn run<'a>(
            &'a self,
            _ctx: crate::server::tools::ToolContext,
            _args: serde_json::Value,
        ) -> crate::server::tools::ToolFuture<'a> {
            Box::pin(async { Ok(serde_json::json!({})) })
        }
    }

    #[test]
    fn enabled_tool_ids_gates_per_user_mcp_by_session_overlay() {
        // The core of "select an integration → its tools become available":
        // a connected connector's tools are exposed ONLY once its toggle key is
        // enabled in the conversation overlay.
        use std::collections::HashSet;
        let mut layer = UserMcpLayer::default();
        layer.insert_for_test(Arc::new(FakeMcpTool), "demo", ToolMode::Always);
        layer.insert_for_test(Arc::new(Echo), "demo", ToolMode::Always);

        // Nothing enabled → connector tools stay hidden (progressive disclosure).
        assert!(layer.enabled_tool_ids(&HashSet::new()).is_empty());

        // Enabling `mcp__demo` (what the composer writes when you pick the
        // integration) surfaces the connector's tool — and only it.
        let on: HashSet<String> = ["mcp__demo".to_string()].into_iter().collect();
        let ids = layer.enabled_tool_ids(&on);
        assert!(ids.contains(&"mcp__demo__echo".to_string()), "{ids:?}");
        assert!(!ids.contains(&"company_echo".to_string()), "{ids:?}");

        // A non-MCP tool keys on its own id, so `mcp__demo` doesn't pull it in.
        let echo_on: HashSet<String> = ["company_echo".to_string()].into_iter().collect();
        assert_eq!(
            layer.enabled_tool_ids(&echo_on),
            vec!["company_echo".to_string()]
        );
    }

    #[test]
    fn composite_unions_and_registry_wins_on_collision() {
        let registry = ToolRegistry::new().with(Echo); // id "company_echo"
        let mut layer = UserMcpLayer::default();
        // A distinct overlay tool + one that collides with the registry id.
        layer.insert_for_test(Arc::new(Echo), "gmail", ToolMode::Always); // collides: company_echo
        let composite = CompositeToolSource::new(&registry, &layer);

        // Registry wins on a colliding id (a connector can't shadow a built-in).
        assert!(composite.contains("company_echo"));
        // defs_for de-dups: the registry def is used, the overlay's collision
        // is dropped (registry provides it).
        let defs = composite.defs_for(&["company_echo".into()]);
        assert_eq!(defs.len(), 1);
        // get resolves registry first.
        assert!(ToolSource::get(&composite, "company_echo").is_some());
        // ids() includes both sources' ids.
        assert!(composite.ids().iter().any(|i| i == "company_echo"));
    }

    #[test]
    fn user_layer_tool_ids_reflect_inserts() {
        let mut layer = UserMcpLayer::default();
        assert!(layer.is_empty());
        layer.insert_for_test(Arc::new(Echo), "gmail", ToolMode::Always);
        assert!(!layer.is_empty());
        assert_eq!(layer.tool_ids(), vec!["company_echo".to_string()]);
        assert_eq!(layer.mode_of("company_echo"), Some(ToolMode::Always));
        assert_eq!(layer.connector_of("company_echo"), Some("gmail"));
    }
}
