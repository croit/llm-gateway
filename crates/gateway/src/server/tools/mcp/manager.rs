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

use serde_json::Value;

use super::{ConnectedServer, McpTool, connect_http_server};
use crate::server::auth::mcp_oauth;
use crate::server::crypto::Crypto;
use crate::server::db::Pool;
use crate::server::db::mcp_audit;
use crate::server::db::mcp_catalog::{self, Connector};
use crate::server::db::user_mcp::{self, Connection, ToolMode};
use crate::server::tools::{Tool, ToolContext, ToolFuture, ToolRegistry, ToolSource};

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

    /// Cache key for a `global` connector: keyed by the connector alone (no
    /// user) so the single shared connection is reused across every user. The
    /// leading unit separator can't collide with a per-user key, which always
    /// starts with a (non-empty) user id.
    fn global_cache_key(connector_key: &str) -> String {
        format!("\u{1f}global\u{1f}{connector_key}")
    }

    /// Drop any cached connection for a user+connector (e.g. on disconnect or
    /// after an auth error) so the next use reconnects fresh.
    pub async fn invalidate(&self, user_id: &str, connector_key: &str) {
        let mut cache = self.cache.lock().await;
        cache.remove(&Self::cache_key(user_id, connector_key));
    }

    /// Return the cached tools for `ck` if still within [`CACHE_TTL`].
    async fn cache_lookup(&self, ck: &str) -> Option<Vec<Arc<McpTool>>> {
        let cache = self.cache.lock().await;
        cache
            .get(ck)
            .filter(|c| c.fetched_at.elapsed() < CACHE_TTL)
            .map(|c| c.tools.clone())
    }

    /// Insert `tools` under `ck`, bounding the cache: when full and this is a
    /// new key, evict stale entries first (dropping them closes their MCP
    /// sockets), then the oldest.
    async fn cache_store(&self, ck: String, tools: Vec<Arc<McpTool>>) {
        let mut cache = self.cache.lock().await;
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
                tools,
                fetched_at: Instant::now(),
            },
        );
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
        // Per-user connectors the user has connected. Resolve concurrently —
        // one slow/unreachable server can't serialise the whole turn behind it.
        let futs = keys.into_iter().map(|key| async move {
            let connector = match mcp_catalog::get(&self.db, &key).await {
                Ok(Some(c)) if c.enabled && !c.is_global() => c,
                // Connector gone, disabled by the admin, or flipped to global
                // (no per-user connection applies) → hide its tools.
                _ => return None,
            };
            if !self.role_allows(&connector, role_ids) {
                return None;
            }
            let (allow_ask, modes) = self.ask_and_modes(user_id, &key, ask).await;
            match self.ensure(user_id, &connector).await {
                Ok(tools) => Some((key, tools, modes, allow_ask, connector.audit)),
                Err(err) => {
                    tracing::warn!(user = %user_id, connector = %key, error = %err,
                        "MCP connector unavailable this turn");
                    None
                }
            }
        });

        // Global connectors: one shared identity for the whole gateway, exposed
        // to everyone RBAC allows with no per-user connection step. Their tools
        // still respect the user's own always/ask/off prefs and the per-token
        // ask policy, so they slot into the same overlay.
        let globals: Vec<Connector> = mcp_catalog::list_enabled(&self.db)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.is_global())
            .collect();
        let global_futs = globals.into_iter().map(|connector| async move {
            let key = connector.key.clone();
            if !self.role_allows(&connector, role_ids) {
                return None;
            }
            let (allow_ask, modes) = self.ask_and_modes(user_id, &key, ask).await;
            match self.ensure_global(&connector).await {
                Ok(tools) => Some((key, tools, modes, allow_ask, connector.audit)),
                Err(err) => {
                    tracing::warn!(connector = %key, error = %err,
                        "global MCP connector unavailable this turn");
                    None
                }
            }
        });

        let (resolved, global_resolved) = rama::futures::future::join(
            rama::futures::future::join_all(futs),
            rama::futures::future::join_all(global_futs),
        )
        .await;
        let mut layer = UserMcpLayer::default();
        for (key, tools, modes, allow_ask, audit) in resolved
            .into_iter()
            .flatten()
            .chain(global_resolved.into_iter().flatten())
        {
            let audit_db = audit.then(|| self.db.clone());
            layer.add(&key, &tools, &modes, allow_ask, audit_db.as_ref());
        }
        layer
    }

    /// RBAC gate at exposure time: a connector with a `required_role` is only
    /// exposed to users holding that role (re-checked every turn, so revoking a
    /// role or adding a `required_role` drops the tools immediately).
    fn role_allows(&self, connector: &Connector, role_ids: &[String]) -> bool {
        match &connector.required_role {
            Some(required) => role_ids.iter().any(|r| r == required),
            None => true,
        }
    }

    /// Resolve, for one connector, whether `ask`-mode tools are exposed in this
    /// context and the user's per-tool mode overrides.
    async fn ask_and_modes(
        &self,
        user_id: &str,
        key: &str,
        ask: AskContext<'_>,
    ) -> (bool, HashMap<String, ToolMode>) {
        let allow_ask = match ask {
            AskContext::Chat => false,
            AskContext::Api { token_id } => matches!(
                user_mcp::token_ask_policy(&self.db, token_id, key)
                    .await
                    .unwrap_or(user_mcp::AskOverApi::Block),
                user_mcp::AskOverApi::Allow
            ),
        };
        let modes = user_mcp::tool_modes(&self.db, user_id, key)
            .await
            .unwrap_or_default();
        (allow_ask, modes)
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
        if let Some(tools) = self.cache_lookup(&ck).await {
            return Ok(tools);
        }

        let conn = user_mcp::get_connection(&self.db, user_id, &connector.key)
            .await
            .map_err(|e| format!("loading connection: {e}"))?
            .ok_or_else(|| "not connected".to_string())?;

        // Open connectors carry no credentials at all — the user's "connected"
        // row exists purely so they can opt the tools in/out, not for auth.
        let connected = if connector.auth == mcp_catalog::AuthKind::None {
            connect_http_server(&connector.key, &connector.url, None).await?
        } else {
            let (access, refreshed) = self.access_token(user_id, connector, &conn).await?;
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
            }
        };
        let ConnectedServer { conn: _live, tools } = connected;
        let tools: Vec<Arc<McpTool>> = tools.into_iter().map(Arc::new).collect();
        self.cache_store(ck, tools.clone()).await;
        Ok(tools)
    }

    /// Ensure the single shared connection for a `global` connector, returning
    /// its tools. Unlike [`Self::ensure`] there is no per-user connection row
    /// and the connection is cached per-connector (shared across users). Auth
    /// comes from the connector row: `None` sends no credentials (e.g. Discord,
    /// whose bot token is baked into the sidecar behind a loopback endpoint);
    /// `StaticBearer` sends the shared token stored (encrypted) on the row.
    /// `OAuth2` is inherently per-user and rejected here (and at admin save).
    async fn ensure_global(&self, connector: &Connector) -> Result<Vec<Arc<McpTool>>, String> {
        let ck = Self::global_cache_key(&connector.key);
        if let Some(tools) = self.cache_lookup(&ck).await {
            return Ok(tools);
        }
        let connected = match connector.auth {
            mcp_catalog::AuthKind::None => {
                connect_http_server(&connector.key, &connector.url, None).await?
            }
            mcp_catalog::AuthKind::StaticBearer => {
                let token = self.decrypt_connector_secret(connector)?.ok_or_else(|| {
                    "global static_bearer connector has no bearer token configured".to_string()
                })?;
                connect_http_server(&connector.key, &connector.url, Some(&token)).await?
            }
            mcp_catalog::AuthKind::OAuth2 => {
                return Err("OAuth2 is per-user and not valid for a global connector".to_string());
            }
        };
        let ConnectedServer { conn: _live, tools } = connected;
        let tools: Vec<Arc<McpTool>> = tools.into_iter().map(Arc::new).collect();
        self.cache_store(ck, tools.clone()).await;
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
        let tools = if connector.is_global() {
            self.ensure_global(connector).await?
        } else {
            self.ensure(user_id, connector).await?
        };
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

/// A [`Tool`] wrapper that records every call to [`mcp_audit`]. Used for
/// connectors whose `audit` flag is on — the model-facing schema and behaviour
/// are unchanged; a best-effort audit row (acting user, connector, tool,
/// truncated args, outcome) is written after the inner tool runs. A failed
/// audit write is logged, never propagated — auditing must not break the call.
struct AuditedTool {
    inner: Arc<dyn Tool>,
    connector_key: String,
    db: Pool,
}

impl Tool for AuditedTool {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn schema(&self) -> ToolDef {
        self.inner.schema()
    }

    fn max_duration(&self) -> Option<Duration> {
        self.inner.max_duration()
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        let db = self.db.clone();
        let connector = self.connector_key.clone();
        let tool_id = self.inner.id().to_string();
        let user_id = ctx.user_id.clone();
        let session = ctx.session_id.clone();
        let args_summary = serde_json::to_string(&args).ok();
        let inner = self.inner.clone();
        Box::pin(async move {
            let res = inner.run(ctx, args).await;
            let (outcome, error) = match &res {
                Ok(_) => ("ok", None),
                Err(e) => ("error", Some(e.to_string())),
            };
            if let Err(e) = mcp_audit::record(
                &db,
                &user_id,
                &connector,
                &tool_id,
                args_summary.as_deref(),
                outcome,
                error.as_deref(),
                session.as_deref(),
            )
            .await
            {
                tracing::warn!(error = %e, connector = %connector, tool = %tool_id,
                    "MCP tool audit write failed");
            }
            res
        })
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
        audit_db: Option<&Pool>,
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
            // When the connector is audited, store an audit-wrapping tool in
            // place of the raw one; it delegates everything and records the call.
            let stored: Arc<dyn Tool> = match audit_db {
                Some(db) => Arc::new(AuditedTool {
                    inner: tool.clone() as Arc<dyn Tool>,
                    connector_key: connector_key.to_string(),
                    db: db.clone(),
                }),
                None => tool.clone() as Arc<dyn Tool>,
            };
            self.tools.insert(id.clone(), stored);
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
    use crate::server::crypto::Crypto;
    use crate::server::db;
    use crate::server::db::mcp_catalog::{AuthKind, Scope};
    use crate::server::tools::echo::Echo;

    async fn manager() -> Arc<McpConnectionManager> {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        McpConnectionManager::new(pool, Arc::new(Crypto::from_key([7u8; 32])))
    }

    fn global_connector(auth: AuthKind) -> Connector {
        let now = Timestamp::now();
        Connector {
            key: "discord".into(),
            name: "Discord".into(),
            description: None,
            icon: None,
            category: None,
            url: "http://127.0.0.1:1/mcp".into(),
            auth,
            scope: Scope::Global,
            audit: false,
            use_dcr: false,
            client_id: None,
            client_secret_ct: None,
            client_secret_nonce: None,
            authorize_url: None,
            token_url: None,
            registration_url: None,
            scopes: vec![],
            required_role: None,
            enabled: true,
            seeded: true,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn global_oauth2_is_rejected_without_touching_network() {
        // OAuth2 is per-user; a global connector can't use it. ensure_global
        // must reject it up front (defensive — admin save also blocks it).
        let mgr = manager().await;
        let err = match mgr.ensure_global(&global_connector(AuthKind::OAuth2)).await {
            Ok(_) => panic!("expected OAuth2 global to be rejected"),
            Err(e) => e,
        };
        assert!(err.contains("OAuth2"), "{err}");
    }

    #[tokio::test]
    async fn global_static_bearer_without_token_errors() {
        // StaticBearer global connector needs its shared token on the row.
        let mgr = manager().await;
        let err = match mgr
            .ensure_global(&global_connector(AuthKind::StaticBearer))
            .await
        {
            Ok(_) => panic!("expected static_bearer-without-token to error"),
            Err(e) => e,
        };
        assert!(err.contains("bearer token"), "{err}");
    }

    #[tokio::test]
    async fn role_allows_gates_on_required_role() {
        let mgr = manager().await;
        let mut c = global_connector(AuthKind::None);
        assert!(mgr.role_allows(&c, &[]), "no required_role → everyone");
        c.required_role = Some("staff".into());
        assert!(!mgr.role_allows(&c, &["user".into()]));
        assert!(mgr.role_allows(&c, &["user".into(), "staff".into()]));
    }

    #[tokio::test]
    async fn audited_tool_records_ok_and_error() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let ctx = |db: Pool| ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db,
            s3: None,
            assistant_turn_id: None,
            session_id: Some("sess".into()),
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
            image_gen: None,
        };
        let audited = AuditedTool {
            inner: Arc::new(Echo) as Arc<dyn Tool>,
            connector_key: "discord".into(),
            db: pool.clone(),
        };
        // Success path (Echo returns the message).
        audited
            .run(ctx(pool.clone()), serde_json::json!({"message": "hi"}))
            .await
            .unwrap();
        // Error path (Echo rejects a missing message) — still audited.
        let _ = audited.run(ctx(pool.clone()), serde_json::json!({})).await;

        let ev = mcp_audit::recent(&pool, 10).await.unwrap();
        assert_eq!(ev.len(), 2, "both calls recorded");
        assert!(ev.iter().any(|e| e.outcome == "ok"));
        assert!(ev.iter().any(|e| e.outcome == "error" && e.error.is_some()));
        assert!(ev.iter().all(|e| e.connector_key == "discord"
            && e.user_id == "u"
            && e.tool_id == "company_echo"
            && e.session_id.as_deref() == Some("sess")));
    }

    #[test]
    fn global_cache_key_is_user_independent() {
        // The shared connection is keyed by connector alone, and can't collide
        // with any per-user key (those start with a non-empty user id).
        assert_eq!(
            McpConnectionManager::global_cache_key("discord"),
            McpConnectionManager::global_cache_key("discord")
        );
        assert_ne!(
            McpConnectionManager::global_cache_key("discord"),
            McpConnectionManager::cache_key("discord", "discord")
        );
    }

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
