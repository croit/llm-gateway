// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-user MCP state: a user's OAuth connection to a catalog connector
//! (`user_mcp_connections`), in-flight OAuth authorizations
//! (`pending_mcp_oauth`), tri-state per-tool permissions
//! (`user_mcp_tool_prefs`), and the per-token `/v1` approval policy
//! (`token_mcp_policy`). Migration 0023.
//!
//! Crypto-agnostic: token / secret columns are opaque AES-GCM `(nonce,
//! ciphertext)` pairs the caller (holding [`crate::server::crypto::Crypto`])
//! seals and opens. This module just persists the bytes.

use std::collections::HashMap;

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

use crate::server::db::{DbError, Pool};

/// Tri-state per-tool permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolMode {
    /// Run without asking.
    Always,
    /// Ask the user first (chat UI); over /v1, governed by the token policy.
    Ask,
    /// Never expose this tool.
    Off,
}

impl ToolMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolMode::Always => "always",
            ToolMode::Ask => "ask",
            ToolMode::Off => "off",
        }
    }
    pub fn parse(s: &str) -> Option<ToolMode> {
        match s {
            "always" => Some(ToolMode::Always),
            "ask" => Some(ToolMode::Ask),
            "off" => Some(ToolMode::Off),
            _ => None,
        }
    }
}

/// How `ask`-mode tools behave over the /v1 API (which can't pause for an
/// interactive approval).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskOverApi {
    /// Don't run; return a tool error telling the model approval is required.
    Block,
    /// Treat `ask` like `always`.
    Allow,
}

impl AskOverApi {
    pub fn as_str(self) -> &'static str {
        match self {
            AskOverApi::Block => "block",
            AskOverApi::Allow => "allow",
        }
    }
    pub fn parse(s: &str) -> AskOverApi {
        match s {
            "allow" => AskOverApi::Allow,
            _ => AskOverApi::Block,
        }
    }
}

/// A user's connection to one connector.
#[derive(Debug, Clone)]
pub struct Connection {
    pub id: String,
    pub user_id: String,
    pub connector_key: String,
    pub status: String,
    pub access_token_ct: Option<Vec<u8>>,
    pub access_token_nonce: Option<Vec<u8>>,
    pub refresh_token_ct: Option<Vec<u8>>,
    pub refresh_token_nonce: Option<Vec<u8>>,
    pub token_expires_at: Option<Timestamp>,
    pub scopes: Vec<String>,
    pub dcr_client_id: Option<String>,
    pub dcr_client_secret_ct: Option<Vec<u8>>,
    pub dcr_client_secret_nonce: Option<Vec<u8>>,
    /// Resolved OAuth token endpoint, persisted so refresh skips re-discovery.
    pub token_url: Option<String>,
    pub last_error: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Everything needed to persist a freshly-completed OAuth connection. Tokens
/// arrive pre-sealed.
pub struct NewConnection {
    pub user_id: String,
    pub connector_key: String,
    pub access_token_ct: Vec<u8>,
    pub access_token_nonce: Vec<u8>,
    pub refresh_token_ct: Option<Vec<u8>>,
    pub refresh_token_nonce: Option<Vec<u8>>,
    pub token_expires_at: Option<Timestamp>,
    pub scopes: Vec<String>,
    pub dcr_client_id: Option<String>,
    pub dcr_client_secret_ct: Option<Vec<u8>>,
    pub dcr_client_secret_nonce: Option<Vec<u8>>,
    pub token_url: Option<String>,
}

fn parse_ts(s: String, column: &'static str) -> Result<Timestamp, DbError> {
    s.parse().map_err(|e: jiff::Error| DbError::Decode {
        column,
        source: e.into(),
    })
}

fn parse_opt_ts(s: Option<String>, column: &'static str) -> Result<Option<Timestamp>, DbError> {
    s.map(|s| parse_ts(s, column)).transpose()
}

fn map_conn(row: &SqliteRow) -> Result<Connection, DbError> {
    let scopes_json: String = row.try_get("scopes_json")?;
    Ok(Connection {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        connector_key: row.try_get("connector_key")?,
        status: row.try_get("status")?,
        access_token_ct: row.try_get("access_token_ct")?,
        access_token_nonce: row.try_get("access_token_nonce")?,
        refresh_token_ct: row.try_get("refresh_token_ct")?,
        refresh_token_nonce: row.try_get("refresh_token_nonce")?,
        token_expires_at: parse_opt_ts(row.try_get("token_expires_at")?, "token_expires_at")?,
        scopes: serde_json::from_str(&scopes_json).unwrap_or_default(),
        dcr_client_id: row.try_get("dcr_client_id")?,
        dcr_client_secret_ct: row.try_get("dcr_client_secret_ct")?,
        dcr_client_secret_nonce: row.try_get("dcr_client_secret_nonce")?,
        token_url: row.try_get("token_url")?,
        last_error: row.try_get("last_error")?,
        created_at: parse_ts(row.try_get("created_at")?, "created_at")?,
        updated_at: parse_ts(row.try_get("updated_at")?, "updated_at")?,
    })
}

const CONN_COLS: &str = "id, user_id, connector_key, status, access_token_ct, \
     access_token_nonce, refresh_token_ct, refresh_token_nonce, token_expires_at, \
     scopes_json, dcr_client_id, dcr_client_secret_ct, dcr_client_secret_nonce, \
     token_url, last_error, created_at, updated_at";

/// Insert or replace a user's connection to a connector (one per pair). Marks
/// it `connected`.
pub async fn upsert_connection(pool: &Pool, new: NewConnection) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    let scopes_json = serde_json::to_string(&new.scopes).unwrap_or_else(|_| "[]".into());
    sqlx::query(
        r#"INSERT INTO user_mcp_connections
              (id, user_id, connector_key, status, access_token_ct, access_token_nonce,
               refresh_token_ct, refresh_token_nonce, token_expires_at, scopes_json,
               dcr_client_id, dcr_client_secret_ct, dcr_client_secret_nonce,
               token_url, last_error, created_at, updated_at)
           VALUES (?, ?, ?, 'connected', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)
           ON CONFLICT(user_id, connector_key) DO UPDATE SET
               status = 'connected',
               access_token_ct = excluded.access_token_ct,
               access_token_nonce = excluded.access_token_nonce,
               refresh_token_ct = excluded.refresh_token_ct,
               refresh_token_nonce = excluded.refresh_token_nonce,
               token_expires_at = excluded.token_expires_at,
               scopes_json = excluded.scopes_json,
               dcr_client_id = excluded.dcr_client_id,
               dcr_client_secret_ct = excluded.dcr_client_secret_ct,
               dcr_client_secret_nonce = excluded.dcr_client_secret_nonce,
               token_url = excluded.token_url,
               last_error = NULL,
               updated_at = excluded.updated_at"#,
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&new.user_id)
    .bind(&new.connector_key)
    .bind(&new.access_token_ct)
    .bind(&new.access_token_nonce)
    .bind(&new.refresh_token_ct)
    .bind(&new.refresh_token_nonce)
    .bind(new.token_expires_at.map(|t| t.to_string()))
    .bind(&scopes_json)
    .bind(&new.dcr_client_id)
    .bind(&new.dcr_client_secret_ct)
    .bind(&new.dcr_client_secret_nonce)
    .bind(&new.token_url)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Replace the access/refresh token after a refresh.
#[allow(clippy::too_many_arguments)]
pub async fn update_tokens(
    pool: &Pool,
    user_id: &str,
    connector_key: &str,
    access_ct: &[u8],
    access_nonce: &[u8],
    refresh_ct: Option<&[u8]>,
    refresh_nonce: Option<&[u8]>,
    expires_at: Option<Timestamp>,
) -> Result<(), DbError> {
    // When a refresh response omits a new refresh token, keep the existing one.
    let set_refresh = refresh_ct.is_some();
    let sql = if set_refresh {
        r#"UPDATE user_mcp_connections SET status = 'connected',
               access_token_ct = ?, access_token_nonce = ?,
               refresh_token_ct = ?, refresh_token_nonce = ?,
               token_expires_at = ?, last_error = NULL, updated_at = ?
           WHERE user_id = ? AND connector_key = ?"#
    } else {
        r#"UPDATE user_mcp_connections SET status = 'connected',
               access_token_ct = ?, access_token_nonce = ?,
               token_expires_at = ?, last_error = NULL, updated_at = ?
           WHERE user_id = ? AND connector_key = ?"#
    };
    let mut q = sqlx::query(sql).bind(access_ct).bind(access_nonce);
    if set_refresh {
        q = q.bind(refresh_ct).bind(refresh_nonce);
    }
    q.bind(expires_at.map(|t| t.to_string()))
        .bind(Timestamp::now().to_string())
        .bind(user_id)
        .bind(connector_key)
        .execute(pool)
        .await?;
    Ok(())
}

/// Mark a connection as errored (e.g. refresh failed, server rejected token).
pub async fn mark_error(
    pool: &Pool,
    user_id: &str,
    connector_key: &str,
    error: &str,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE user_mcp_connections SET status = 'error', last_error = ?, updated_at = ? \
         WHERE user_id = ? AND connector_key = ?",
    )
    .bind(error)
    .bind(Timestamp::now().to_string())
    .bind(user_id)
    .bind(connector_key)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_connection(
    pool: &Pool,
    user_id: &str,
    connector_key: &str,
) -> Result<Option<Connection>, DbError> {
    let sql = format!(
        "SELECT {CONN_COLS} FROM user_mcp_connections WHERE user_id = ? AND connector_key = ?"
    );
    let row = sqlx::query(&sql)
        .bind(user_id)
        .bind(connector_key)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(map_conn).transpose()
}

pub async fn list_connections(pool: &Pool, user_id: &str) -> Result<Vec<Connection>, DbError> {
    let sql = format!(
        "SELECT {CONN_COLS} FROM user_mcp_connections WHERE user_id = ? ORDER BY connector_key ASC"
    );
    let rows = sqlx::query(&sql).bind(user_id).fetch_all(pool).await?;
    rows.iter().map(map_conn).collect()
}

/// Connections the background worker should proactively refresh: `connected`,
/// holding a refresh token, and either expiring at/before `expiring_before`
/// or — when the provider gave no expiry — not refreshed since `stale_before`
/// (so we still exercise the refresh token to reset inactivity timers).
pub async fn connections_due_for_refresh(
    pool: &Pool,
    expiring_before: Timestamp,
    stale_before: Timestamp,
) -> Result<Vec<Connection>, DbError> {
    let sql = format!(
        "SELECT {CONN_COLS} FROM user_mcp_connections \
         WHERE status = 'connected' AND refresh_token_ct IS NOT NULL \
           AND ( (token_expires_at IS NOT NULL AND token_expires_at <= ?) \
                 OR (token_expires_at IS NULL AND updated_at <= ?) )"
    );
    let rows = sqlx::query(&sql)
        .bind(expiring_before.to_string())
        .bind(stale_before.to_string())
        .fetch_all(pool)
        .await?;
    rows.iter().map(map_conn).collect()
}

/// Keys of connectors the user has a `connected`-status connection to.
pub async fn connected_keys(pool: &Pool, user_id: &str) -> Result<Vec<String>, DbError> {
    let rows = sqlx::query(
        "SELECT connector_key FROM user_mcp_connections WHERE user_id = ? AND status = 'connected'",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            r.try_get::<String, _>("connector_key")
                .map_err(DbError::from)
        })
        .collect()
}

/// Disconnect: remove the user's connection (and its tokens) for a connector.
pub async fn delete_connection(
    pool: &Pool,
    user_id: &str,
    connector_key: &str,
) -> Result<bool, DbError> {
    let affected =
        sqlx::query("DELETE FROM user_mcp_connections WHERE user_id = ? AND connector_key = ?")
            .bind(user_id)
            .bind(connector_key)
            .execute(pool)
            .await?
            .rows_affected();
    Ok(affected > 0)
}

/// Remove every user's connection (incl. encrypted tokens) and per-tool prefs
/// for a connector — called when an admin deletes it from the catalog, so no
/// orphaned secrets linger. Returns the number of connection rows removed.
pub async fn delete_all_for_connector(pool: &Pool, connector_key: &str) -> Result<u64, DbError> {
    let removed = sqlx::query("DELETE FROM user_mcp_connections WHERE connector_key = ?")
        .bind(connector_key)
        .execute(pool)
        .await?
        .rows_affected();
    let _ = sqlx::query("DELETE FROM user_mcp_tool_prefs WHERE connector_key = ?")
        .bind(connector_key)
        .execute(pool)
        .await?;
    let _ = sqlx::query("DELETE FROM token_mcp_policy WHERE connector_key = ?")
        .bind(connector_key)
        .execute(pool)
        .await?;
    Ok(removed)
}

// ---- pending OAuth ---------------------------------------------------------

/// In-flight authorization, persisted between the redirect and the callback.
pub struct PendingOauth {
    pub state: String,
    pub user_id: String,
    pub connector_key: String,
    pub pkce_verifier: String,
    pub redirect_uri: String,
    pub token_url: String,
    pub resource: Option<String>,
    pub dcr_client_id: Option<String>,
    pub dcr_client_secret_ct: Option<Vec<u8>>,
    pub dcr_client_secret_nonce: Option<Vec<u8>>,
    pub return_to: Option<String>,
}

/// TTL for an in-flight authorization. Mirrors the OIDC `pending_logins` TTL.
const PENDING_TTL_SECS: i64 = 600;

pub async fn create_pending(pool: &Pool, p: &PendingOauth) -> Result<(), DbError> {
    let now = Timestamp::now();
    let expires = now + jiff::Span::new().seconds(PENDING_TTL_SECS);
    sqlx::query(
        r#"INSERT INTO pending_mcp_oauth
              (state, user_id, connector_key, pkce_verifier, redirect_uri, token_url,
               resource, dcr_client_id, dcr_client_secret_ct, dcr_client_secret_nonce,
               return_to, created_at, expires_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&p.state)
    .bind(&p.user_id)
    .bind(&p.connector_key)
    .bind(&p.pkce_verifier)
    .bind(&p.redirect_uri)
    .bind(&p.token_url)
    .bind(&p.resource)
    .bind(&p.dcr_client_id)
    .bind(&p.dcr_client_secret_ct)
    .bind(&p.dcr_client_secret_nonce)
    .bind(&p.return_to)
    .bind(now.to_string())
    .bind(expires.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch and consume (delete) a pending authorization by state. Returns `None`
/// if the state is unknown or already expired.
pub async fn take_pending(pool: &Pool, state: &str) -> Result<Option<PendingOauth>, DbError> {
    let row = sqlx::query(
        r#"SELECT user_id, connector_key, pkce_verifier, redirect_uri, token_url, resource,
                  dcr_client_id, dcr_client_secret_ct, dcr_client_secret_nonce, return_to,
                  expires_at
           FROM pending_mcp_oauth WHERE state = ?"#,
    )
    .bind(state)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    // One-shot: always delete the row we found.
    sqlx::query("DELETE FROM pending_mcp_oauth WHERE state = ?")
        .bind(state)
        .execute(pool)
        .await?;
    let expires = parse_ts(row.try_get("expires_at")?, "expires_at")?;
    if Timestamp::now() > expires {
        return Ok(None);
    }
    Ok(Some(PendingOauth {
        state: state.to_string(),
        user_id: row.try_get("user_id")?,
        connector_key: row.try_get("connector_key")?,
        pkce_verifier: row.try_get("pkce_verifier")?,
        redirect_uri: row.try_get("redirect_uri")?,
        token_url: row.try_get("token_url")?,
        resource: row.try_get("resource")?,
        dcr_client_id: row.try_get("dcr_client_id")?,
        dcr_client_secret_ct: row.try_get("dcr_client_secret_ct")?,
        dcr_client_secret_nonce: row.try_get("dcr_client_secret_nonce")?,
        return_to: row.try_get("return_to")?,
    }))
}

/// Delete expired pending rows (housekeeping; safe to call periodically).
pub async fn sweep_expired_pending(pool: &Pool) -> Result<u64, DbError> {
    let affected = sqlx::query("DELETE FROM pending_mcp_oauth WHERE expires_at < ?")
        .bind(Timestamp::now().to_string())
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected)
}

// ---- per-tool permission modes --------------------------------------------

/// Explicit per-tool modes for a user+connector. Tools without a row fall back
/// to the connector default (resolved by the caller from tool annotations).
pub async fn tool_modes(
    pool: &Pool,
    user_id: &str,
    connector_key: &str,
) -> Result<HashMap<String, ToolMode>, DbError> {
    let rows = sqlx::query(
        "SELECT tool_name, mode FROM user_mcp_tool_prefs WHERE user_id = ? AND connector_key = ?",
    )
    .bind(user_id)
    .bind(connector_key)
    .fetch_all(pool)
    .await?;
    let mut out = HashMap::new();
    for r in &rows {
        let name: String = r.try_get("tool_name")?;
        let mode: String = r.try_get("mode")?;
        if let Some(m) = ToolMode::parse(&mode) {
            out.insert(name, m);
        }
    }
    Ok(out)
}

pub async fn set_tool_mode(
    pool: &Pool,
    user_id: &str,
    connector_key: &str,
    tool_name: &str,
    mode: ToolMode,
) -> Result<(), DbError> {
    sqlx::query(
        r#"INSERT INTO user_mcp_tool_prefs (user_id, connector_key, tool_name, mode, updated_at)
           VALUES (?, ?, ?, ?, ?)
           ON CONFLICT(user_id, connector_key, tool_name) DO UPDATE SET
               mode = excluded.mode, updated_at = excluded.updated_at"#,
    )
    .bind(user_id)
    .bind(connector_key)
    .bind(tool_name)
    .bind(mode.as_str())
    .bind(Timestamp::now().to_string())
    .execute(pool)
    .await?;
    Ok(())
}

// ---- per-token /v1 ask policy ---------------------------------------------

/// Resolve how `ask` tools behave over the /v1 API for `token_id` +
/// `connector_key`: the connector-specific row wins, else the `'*'` default,
/// else [`AskOverApi::Block`].
pub async fn token_ask_policy(
    pool: &Pool,
    token_id: &str,
    connector_key: &str,
) -> Result<AskOverApi, DbError> {
    let row = sqlx::query(
        "SELECT connector_key, ask_over_api FROM token_mcp_policy \
         WHERE token_id = ? AND connector_key IN (?, '*')",
    )
    .bind(token_id)
    .bind(connector_key)
    .fetch_all(pool)
    .await?;
    // Prefer the exact connector match over the wildcard.
    let mut wildcard: Option<AskOverApi> = None;
    let mut exact: Option<AskOverApi> = None;
    for r in &row {
        let key: String = r.try_get("connector_key")?;
        let val = AskOverApi::parse(&r.try_get::<String, _>("ask_over_api")?);
        if key == "*" {
            wildcard = Some(val);
        } else {
            exact = Some(val);
        }
    }
    Ok(exact.or(wildcard).unwrap_or(AskOverApi::Block))
}

pub async fn set_token_policy(
    pool: &Pool,
    token_id: &str,
    connector_key: &str,
    policy: AskOverApi,
) -> Result<(), DbError> {
    sqlx::query(
        r#"INSERT INTO token_mcp_policy (token_id, connector_key, ask_over_api, updated_at)
           VALUES (?, ?, ?, ?)
           ON CONFLICT(token_id, connector_key) DO UPDATE SET
               ask_over_api = excluded.ask_over_api, updated_at = excluded.updated_at"#,
    )
    .bind(token_id)
    .bind(connector_key)
    .bind(policy.as_str())
    .bind(Timestamp::now().to_string())
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;

    async fn pool() -> Pool {
        let p = db::open(std::path::Path::new(":memory:")).await.unwrap();
        // A user row for the FK.
        sqlx::query("INSERT INTO users (id, email, roles_json, created_at, updated_at) VALUES ('u1','u@e','[]','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')")
            .execute(&p)
            .await
            .unwrap();
        p
    }

    fn new_conn() -> NewConnection {
        NewConnection {
            user_id: "u1".into(),
            connector_key: "gmail".into(),
            access_token_ct: vec![1, 2, 3],
            access_token_nonce: vec![4, 5, 6],
            refresh_token_ct: Some(vec![7]),
            refresh_token_nonce: Some(vec![8]),
            token_expires_at: Some("2030-01-01T00:00:00Z".parse().unwrap()),
            scopes: vec!["gmail.readonly".into()],
            dcr_client_id: Some("dcr-123".into()),
            dcr_client_secret_ct: None,
            dcr_client_secret_nonce: None,
            token_url: Some("https://gh/token".into()),
        }
    }

    #[tokio::test]
    async fn upsert_and_get_and_connected_keys() {
        let pool = pool().await;
        upsert_connection(&pool, new_conn()).await.unwrap();
        let c = get_connection(&pool, "u1", "gmail").await.unwrap().unwrap();
        assert_eq!(c.status, "connected");
        assert_eq!(c.access_token_ct, Some(vec![1, 2, 3]));
        assert_eq!(c.scopes, vec!["gmail.readonly".to_string()]);
        assert_eq!(c.dcr_client_id.as_deref(), Some("dcr-123"));
        assert_eq!(connected_keys(&pool, "u1").await.unwrap(), vec!["gmail"]);
    }

    #[tokio::test]
    async fn upsert_replaces_existing() {
        let pool = pool().await;
        upsert_connection(&pool, new_conn()).await.unwrap();
        let mut second = new_conn();
        second.access_token_ct = vec![9, 9, 9];
        upsert_connection(&pool, second).await.unwrap();
        let c = get_connection(&pool, "u1", "gmail").await.unwrap().unwrap();
        assert_eq!(c.access_token_ct, Some(vec![9, 9, 9]));
        // Still one row.
        assert_eq!(list_connections(&pool, "u1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_tokens_keeps_refresh_when_absent() {
        let pool = pool().await;
        upsert_connection(&pool, new_conn()).await.unwrap();
        update_tokens(
            &pool,
            "u1",
            "gmail",
            &[10],
            &[11],
            None,
            None,
            Some("2031-01-01T00:00:00Z".parse().unwrap()),
        )
        .await
        .unwrap();
        let c = get_connection(&pool, "u1", "gmail").await.unwrap().unwrap();
        assert_eq!(c.access_token_ct, Some(vec![10]));
        assert_eq!(
            c.refresh_token_ct,
            Some(vec![7]),
            "refresh kept when not re-sent"
        );
    }

    #[tokio::test]
    async fn due_for_refresh_selects_by_expiry() {
        let pool = pool().await;
        // new_conn() expires 2030 and carries a refresh token.
        upsert_connection(&pool, new_conn()).await.unwrap();
        let far: Timestamp = "2029-01-01T00:00:00Z".parse().unwrap();
        let near: Timestamp = "2031-01-01T00:00:00Z".parse().unwrap();
        let stale: Timestamp = "2000-01-01T00:00:00Z".parse().unwrap();
        // Window ends before expiry → not due.
        assert!(
            connections_due_for_refresh(&pool, far, stale)
                .await
                .unwrap()
                .is_empty()
        );
        // Window ends after expiry → due.
        let due = connections_due_for_refresh(&pool, near, stale)
            .await
            .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].connector_key, "gmail");
    }

    #[tokio::test]
    async fn due_for_refresh_skips_when_no_refresh_token() {
        let pool = pool().await;
        let mut c = new_conn();
        c.refresh_token_ct = None;
        c.refresh_token_nonce = None;
        c.token_expires_at = Some("2020-01-01T00:00:00Z".parse().unwrap()); // long expired
        upsert_connection(&pool, c).await.unwrap();
        let near: Timestamp = "2031-01-01T00:00:00Z".parse().unwrap();
        let stale: Timestamp = "2000-01-01T00:00:00Z".parse().unwrap();
        // Expired but no refresh token → not selectable (can't refresh it).
        assert!(
            connections_due_for_refresh(&pool, near, stale)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn delete_all_for_connector_removes_connections_and_prefs() {
        let pool = pool().await;
        upsert_connection(&pool, new_conn()).await.unwrap();
        set_tool_mode(&pool, "u1", "gmail", "create_draft", ToolMode::Off)
            .await
            .unwrap();
        let removed = delete_all_for_connector(&pool, "gmail").await.unwrap();
        assert_eq!(removed, 1);
        assert!(
            get_connection(&pool, "u1", "gmail")
                .await
                .unwrap()
                .is_none()
        );
        assert!(tool_modes(&pool, "u1", "gmail").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn disconnect_removes_row() {
        let pool = pool().await;
        upsert_connection(&pool, new_conn()).await.unwrap();
        assert!(delete_connection(&pool, "u1", "gmail").await.unwrap());
        assert!(
            get_connection(&pool, "u1", "gmail")
                .await
                .unwrap()
                .is_none()
        );
        assert!(connected_keys(&pool, "u1").await.unwrap().is_empty());
    }

    fn pending(state: &str) -> PendingOauth {
        PendingOauth {
            state: state.into(),
            user_id: "u1".into(),
            connector_key: "github".into(),
            pkce_verifier: "verifier".into(),
            redirect_uri: "https://gw/integrations/callback".into(),
            token_url: "https://gh/token".into(),
            resource: Some("https://api.githubcopilot.com/mcp/".into()),
            dcr_client_id: Some("c-1".into()),
            dcr_client_secret_ct: None,
            dcr_client_secret_nonce: None,
            return_to: None,
        }
    }

    #[tokio::test]
    async fn pending_take_is_one_shot() {
        let pool = pool().await;
        create_pending(&pool, &pending("st1")).await.unwrap();
        let p = take_pending(&pool, "st1").await.unwrap().unwrap();
        assert_eq!(p.connector_key, "github");
        assert_eq!(p.pkce_verifier, "verifier");
        // Consumed: a second take returns None.
        assert!(take_pending(&pool, "st1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tool_modes_roundtrip() {
        let pool = pool().await;
        assert!(tool_modes(&pool, "u1", "gmail").await.unwrap().is_empty());
        set_tool_mode(&pool, "u1", "gmail", "create_draft", ToolMode::Ask)
            .await
            .unwrap();
        set_tool_mode(&pool, "u1", "gmail", "search_threads", ToolMode::Always)
            .await
            .unwrap();
        // overwrite
        set_tool_mode(&pool, "u1", "gmail", "create_draft", ToolMode::Off)
            .await
            .unwrap();
        let modes = tool_modes(&pool, "u1", "gmail").await.unwrap();
        assert_eq!(modes.get("create_draft"), Some(&ToolMode::Off));
        assert_eq!(modes.get("search_threads"), Some(&ToolMode::Always));
    }

    #[tokio::test]
    async fn token_policy_exact_beats_wildcard_and_defaults_block() {
        let pool = pool().await;
        sqlx::query("INSERT INTO tokens (id, user_id, name, hash, created_at, expires_at) VALUES ('t1','u1','tok','h','2026-01-01T00:00:00Z','2030-01-01T00:00:00Z')")
            .execute(&pool)
            .await
            .unwrap();
        // Default with no rows = block.
        assert_eq!(
            token_ask_policy(&pool, "t1", "gmail").await.unwrap(),
            AskOverApi::Block
        );
        set_token_policy(&pool, "t1", "*", AskOverApi::Allow)
            .await
            .unwrap();
        assert_eq!(
            token_ask_policy(&pool, "t1", "gmail").await.unwrap(),
            AskOverApi::Allow
        );
        // Exact connector row overrides the wildcard.
        set_token_policy(&pool, "t1", "gmail", AskOverApi::Block)
            .await
            .unwrap();
        assert_eq!(
            token_ask_policy(&pool, "t1", "gmail").await.unwrap(),
            AskOverApi::Block
        );
    }
}
