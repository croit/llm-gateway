// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The admin-managed catalog of connectable MCP servers (the "connector
//! store"). Rows live in `mcp_catalog_connectors` (migration 0023). A
//! built-in default set is seeded at boot — all disabled — so the admin only
//! has to flip a switch (and, for connectors that need a deployment-specific
//! OAuth client, paste its credentials).
//!
//! This layer is crypto-agnostic: the client secret is stored as an opaque
//! AES-GCM `(nonce, ciphertext)` pair the caller (which holds [`crate::server::crypto::Crypto`])
//! produces and consumes. We never see plaintext here.

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::server::db::{DbError, Pool};

/// OAuth/auth scheme a connector uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// MCP-native OAuth 2.1 (the common case).
    OAuth2,
    /// No auth — an open MCP server.
    None,
    /// A single static bearer token (stored encrypted in `client_secret`).
    StaticBearer,
}

impl AuthKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthKind::OAuth2 => "oauth2",
            AuthKind::None => "none",
            AuthKind::StaticBearer => "static_bearer",
        }
    }
    pub fn parse(s: &str) -> AuthKind {
        match s {
            "none" => AuthKind::None,
            "static_bearer" => AuthKind::StaticBearer,
            _ => AuthKind::OAuth2,
        }
    }
}

/// A catalog connector row.
#[derive(Debug, Clone)]
pub struct Connector {
    pub key: String,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub category: Option<String>,
    pub url: String,
    pub auth: AuthKind,
    pub use_dcr: bool,
    pub client_id: Option<String>,
    /// Encrypted client secret (`nonce`, `ciphertext`) — `None` when unset.
    pub client_secret_ct: Option<Vec<u8>>,
    pub client_secret_nonce: Option<Vec<u8>>,
    pub authorize_url: Option<String>,
    pub token_url: Option<String>,
    pub registration_url: Option<String>,
    pub scopes: Vec<String>,
    pub required_role: Option<String>,
    pub enabled: bool,
    pub seeded: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Connector {
    /// Whether deployment-specific configuration still has to be supplied
    /// before this connector can be enabled. Two cases:
    /// - no server URL yet (e.g. a self-hosted connector whose endpoint is
    ///   deployment-specific, so it can't be seeded), or
    /// - OAuth2 without DCR and no client_id (a manual OAuth client is needed).
    pub fn needs_setup(&self) -> bool {
        self.url.trim().is_empty()
            || (self.auth == AuthKind::OAuth2 && !self.use_dcr && self.client_id.is_none())
    }
}

/// Fields an admin supplies when creating or editing a connector. The client
/// secret is passed pre-encrypted (or `None` to leave unchanged on update /
/// unset on create).
pub struct ConnectorInput {
    pub key: String,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub category: Option<String>,
    pub url: String,
    pub auth: AuthKind,
    pub use_dcr: bool,
    pub client_id: Option<String>,
    pub client_secret_ct: Option<Vec<u8>>,
    pub client_secret_nonce: Option<Vec<u8>>,
    pub authorize_url: Option<String>,
    pub token_url: Option<String>,
    pub registration_url: Option<String>,
    pub scopes: Vec<String>,
    pub required_role: Option<String>,
}

fn parse_ts(s: String, column: &'static str) -> Result<Timestamp, DbError> {
    s.parse().map_err(|e: jiff::Error| DbError::Decode {
        column,
        source: e.into(),
    })
}

fn map_row(row: &SqliteRow) -> Result<Connector, DbError> {
    let scopes_json: String = row.try_get("scopes_json")?;
    let scopes: Vec<String> = serde_json::from_str(&scopes_json).unwrap_or_default();
    Ok(Connector {
        key: row.try_get("key")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        icon: row.try_get("icon")?,
        category: row.try_get("category")?,
        url: row.try_get("url")?,
        auth: AuthKind::parse(&row.try_get::<String, _>("auth")?),
        use_dcr: row.try_get::<i64, _>("use_dcr")? != 0,
        client_id: row.try_get("client_id")?,
        client_secret_ct: row.try_get("client_secret_ct")?,
        client_secret_nonce: row.try_get("client_secret_nonce")?,
        authorize_url: row.try_get("authorize_url")?,
        token_url: row.try_get("token_url")?,
        registration_url: row.try_get("registration_url")?,
        scopes,
        required_role: row.try_get("required_role")?,
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        seeded: row.try_get::<i64, _>("seeded")? != 0,
        created_at: parse_ts(row.try_get("created_at")?, "created_at")?,
        updated_at: parse_ts(row.try_get("updated_at")?, "updated_at")?,
    })
}

const COLS: &str = "key, name, description, icon, category, url, auth, use_dcr, \
     client_id, client_secret_ct, client_secret_nonce, authorize_url, token_url, \
     registration_url, scopes_json, required_role, enabled, seeded, created_at, updated_at";

/// Every connector, alphabetical by display name (admin view).
pub async fn list_all(pool: &Pool) -> Result<Vec<Connector>, DbError> {
    let sql = format!("SELECT {COLS} FROM mcp_catalog_connectors ORDER BY name ASC, key ASC");
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    rows.iter().map(map_row).collect()
}

/// Only enabled connectors (the user-facing store shows these).
pub async fn list_enabled(pool: &Pool) -> Result<Vec<Connector>, DbError> {
    let sql = format!(
        "SELECT {COLS} FROM mcp_catalog_connectors WHERE enabled = 1 ORDER BY name ASC, key ASC"
    );
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    rows.iter().map(map_row).collect()
}

/// One connector by key.
pub async fn get(pool: &Pool, key: &str) -> Result<Option<Connector>, DbError> {
    let sql = format!("SELECT {COLS} FROM mcp_catalog_connectors WHERE key = ?");
    let row = sqlx::query(&sql).bind(key).fetch_optional(pool).await?;
    row.as_ref().map(map_row).transpose()
}

/// Insert a new admin-created connector (enabled = 0, seeded = 0).
pub async fn create(pool: &Pool, input: ConnectorInput) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    let scopes_json = serde_json::to_string(&input.scopes).unwrap_or_else(|_| "[]".into());
    sqlx::query(
        r#"INSERT INTO mcp_catalog_connectors
              (key, name, description, icon, category, url, auth, use_dcr,
               client_id, client_secret_ct, client_secret_nonce, authorize_url,
               token_url, registration_url, scopes_json, required_role,
               enabled, seeded, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, ?, ?)"#,
    )
    .bind(&input.key)
    .bind(&input.name)
    .bind(&input.description)
    .bind(&input.icon)
    .bind(&input.category)
    .bind(&input.url)
    .bind(input.auth.as_str())
    .bind(input.use_dcr as i64)
    .bind(&input.client_id)
    .bind(&input.client_secret_ct)
    .bind(&input.client_secret_nonce)
    .bind(&input.authorize_url)
    .bind(&input.token_url)
    .bind(&input.registration_url)
    .bind(&scopes_json)
    .bind(&input.required_role)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Update an existing connector's editable fields. When `client_secret_ct` is
/// `None` the stored secret is left untouched (so an edit that doesn't retype
/// the secret keeps it); pass `Some(empty-marker)` semantics are not used here.
pub async fn update(pool: &Pool, key: &str, input: ConnectorInput) -> Result<bool, DbError> {
    let now = Timestamp::now().to_string();
    let scopes_json = serde_json::to_string(&input.scopes).unwrap_or_else(|_| "[]".into());
    // Only overwrite the secret columns when a new secret was supplied.
    let set_secret = input.client_secret_ct.is_some();
    let sql = if set_secret {
        r#"UPDATE mcp_catalog_connectors SET
               name = ?, description = ?, icon = ?, category = ?, url = ?, auth = ?,
               use_dcr = ?, client_id = ?, client_secret_ct = ?, client_secret_nonce = ?,
               authorize_url = ?, token_url = ?, registration_url = ?, scopes_json = ?,
               required_role = ?, updated_at = ?
           WHERE key = ?"#
    } else {
        r#"UPDATE mcp_catalog_connectors SET
               name = ?, description = ?, icon = ?, category = ?, url = ?, auth = ?,
               use_dcr = ?, client_id = ?, authorize_url = ?, token_url = ?,
               registration_url = ?, scopes_json = ?, required_role = ?, updated_at = ?
           WHERE key = ?"#
    };
    let mut q = sqlx::query(sql)
        .bind(&input.name)
        .bind(&input.description)
        .bind(&input.icon)
        .bind(&input.category)
        .bind(&input.url)
        .bind(input.auth.as_str())
        .bind(input.use_dcr as i64)
        .bind(&input.client_id);
    if set_secret {
        q = q
            .bind(&input.client_secret_ct)
            .bind(&input.client_secret_nonce);
    }
    q = q
        .bind(&input.authorize_url)
        .bind(&input.token_url)
        .bind(&input.registration_url)
        .bind(&scopes_json)
        .bind(&input.required_role)
        .bind(&now)
        .bind(key);
    Ok(q.execute(pool).await?.rows_affected() > 0)
}

/// Flip a connector on/off.
pub async fn set_enabled(pool: &Pool, key: &str, enabled: bool) -> Result<bool, DbError> {
    let affected =
        sqlx::query("UPDATE mcp_catalog_connectors SET enabled = ?, updated_at = ? WHERE key = ?")
            .bind(enabled as i64)
            .bind(Timestamp::now().to_string())
            .bind(key)
            .execute(pool)
            .await?
            .rows_affected();
    Ok(affected > 0)
}

/// Delete a connector. (User connections to it cascade only via app logic —
/// they reference the key, not a FK, so a delete leaves orphaned connections
/// that simply stop resolving; the store hides them.)
pub async fn delete(pool: &Pool, key: &str) -> Result<bool, DbError> {
    let affected = sqlx::query("DELETE FROM mcp_catalog_connectors WHERE key = ?")
        .bind(key)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

/// A shipped default connector definition.
pub struct DefaultConnector {
    pub key: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub icon: &'static str,
    pub category: &'static str,
    pub url: &'static str,
    pub auth: AuthKind,
    pub use_dcr: bool,
    pub scopes: &'static [&'static str],
    /// Explicit OAuth endpoints for providers that don't publish RFC 8414
    /// metadata / don't do DCR (e.g. GitHub). `None` → resolved by discovery.
    pub authorize_url: Option<&'static str>,
    pub token_url: Option<&'static str>,
}

/// The built-in connector set seeded at boot, all disabled. URLs point at the
/// public remote MCP servers for Atlassian / GitHub / GitLab. `use_dcr = true`
/// connectors work as soon as an admin enables them; GitHub needs a deployment
/// OAuth client (no DCR) and surfaces as "needs setup" until supplied. Google
/// Workspace is DCR but self-hosted, so it ships with an empty URL the admin
/// fills in (also "needs setup" until then).
pub const DEFAULT_CONNECTORS: &[DefaultConnector] = &[
    DefaultConnector {
        key: "google_workspace",
        name: "Google Workspace",
        description: "Gmail, Calendar, Drive, Docs, Sheets, Slides, Tasks and more — one sign-in via a self-hosted Google Workspace MCP server (GA Google APIs, no developer preview).",
        icon: "google_workspace",
        category: "Google",
        // A self-hosted Google Workspace MCP server (e.g.
        // taylorwilsdon/google_workspace_mcp, streamable-HTTP at `/mcp` — no
        // trailing slash). The endpoint is deployment-specific, so the admin
        // sets the URL; the server is its own OAuth 2.1 / DCR provider (it holds
        // the Google OAuth client), so no client id is configured here. Google's
        // *hosted* MCP endpoints (gmailmcp/calendarmcp/drivemcp.googleapis.com)
        // are gated behind the Workspace Developer Preview Program and
        // intentionally not used — see docs/connectors.md.
        url: "",
        auth: AuthKind::OAuth2,
        use_dcr: true,
        // The server defaults to a base-only login (openid + email) and rejects
        // every tool call with "lack required scopes" unless the OAuth *client*
        // (this gateway) requests the service scopes up front. So we request a
        // sensible "one sign-in" read set + Gmail compose; the admin can trim
        // this on the connector. Changing it requires users to reconnect.
        scopes: &[
            "openid",
            "https://www.googleapis.com/auth/userinfo.email",
            "https://www.googleapis.com/auth/gmail.readonly",
            "https://www.googleapis.com/auth/gmail.compose",
            "https://www.googleapis.com/auth/calendar.readonly",
            "https://www.googleapis.com/auth/calendar.events",
            "https://www.googleapis.com/auth/drive.readonly",
            "https://www.googleapis.com/auth/drive.file",
            "https://www.googleapis.com/auth/documents.readonly",
            "https://www.googleapis.com/auth/spreadsheets.readonly",
            "https://www.googleapis.com/auth/presentations.readonly",
            "https://www.googleapis.com/auth/tasks.readonly",
        ],
        authorize_url: None,
        token_url: None,
    },
    DefaultConnector {
        key: "atlassian",
        name: "Atlassian (Jira & Confluence)",
        description: "Search and work with Jira issues and Confluence pages in the user's Atlassian sites.",
        icon: "atlassian",
        category: "Developer",
        // Streamable-HTTP endpoint (`/v1/mcp`). The `/v1/sse` endpoint speaks
        // the legacy HTTP+SSE transport, which our MCP client doesn't support.
        url: "https://mcp.atlassian.com/v1/mcp",
        auth: AuthKind::OAuth2,
        use_dcr: true,
        scopes: &[],
        authorize_url: None,
        token_url: None,
    },
    DefaultConnector {
        key: "github",
        name: "GitHub",
        description: "Browse repositories, issues, and pull requests in the user's GitHub account.",
        icon: "github",
        category: "Developer",
        url: "https://api.githubcopilot.com/mcp/",
        auth: AuthKind::OAuth2,
        // GitHub's authorization server publishes no RFC 8414 metadata and
        // offers no dynamic client registration, so we pin its OAuth endpoints
        // and require an admin-created GitHub OAuth App (client id/secret).
        use_dcr: false,
        scopes: &[
            "repo",
            "read:org",
            "read:user",
            "user:email",
            "read:project",
        ],
        authorize_url: Some("https://github.com/login/oauth/authorize"),
        token_url: Some("https://github.com/login/oauth/access_token"),
    },
    DefaultConnector {
        key: "gitlab",
        name: "GitLab (SaaS / Premium)",
        description: "Work with projects, issues, and merge requests on GitLab.com. GitLab's native MCP server is a Duo feature (Premium/Ultimate); for Community Edition use the self-managed connector below.",
        icon: "gitlab",
        category: "Developer",
        url: "https://gitlab.com/api/v4/mcp",
        auth: AuthKind::OAuth2,
        use_dcr: true,
        scopes: &[],
        authorize_url: None,
        token_url: None,
    },
    DefaultConnector {
        key: "gitlab_selfmanaged",
        name: "GitLab (self-managed / CE)",
        description: "Projects, issues, and merge requests on a self-managed GitLab — including Community Edition. Each user connects with their own GitLab personal access token.",
        icon: "gitlab",
        category: "Developer",
        // GitLab's *native* MCP (`/api/v4/mcp`) is a Duo feature requiring
        // Premium/Ultimate, so CE/Free can't use it. Instead point this at a
        // self-hosted community bridge (e.g. zereight/gitlab-mcp run with
        // STREAMABLE_HTTP=true + REMOTE_AUTHORIZATION=true against the
        // instance's /api/v4) which forwards each request's bearer to GitLab as
        // that user's PAT — so it's a static_bearer connector. The bridge URL is
        // deployment-specific → the admin sets it. See deploy/README.md.
        url: "",
        auth: AuthKind::StaticBearer,
        use_dcr: false,
        scopes: &[],
        authorize_url: None,
        token_url: None,
    },
];

/// Seed the built-in connectors idempotently. Existing rows (matched by key)
/// are never overwritten, so admin edits and the enabled flag survive
/// restarts and upgrades; only missing keys are inserted (disabled). Returns
/// the number of rows newly inserted.
pub async fn seed_defaults(pool: &Pool) -> Result<u64, DbError> {
    let now = Timestamp::now().to_string();
    let mut inserted = 0u64;
    for d in DEFAULT_CONNECTORS {
        let scopes_json = serde_json::to_string(d.scopes).unwrap_or_else(|_| "[]".into());
        let affected = sqlx::query(
            r#"INSERT INTO mcp_catalog_connectors
                  (key, name, description, icon, category, url, auth, use_dcr,
                   authorize_url, token_url, scopes_json, enabled, seeded, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 1, ?, ?)
               ON CONFLICT(key) DO NOTHING"#,
        )
        .bind(d.key)
        .bind(d.name)
        .bind(d.description)
        .bind(d.icon)
        .bind(d.category)
        .bind(d.url)
        .bind(d.auth.as_str())
        .bind(d.use_dcr as i64)
        .bind(d.authorize_url)
        .bind(d.token_url)
        .bind(&scopes_json)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?
        .rows_affected();
        inserted += affected;
    }
    if inserted > 0 {
        tracing::info!(inserted, "seeded default MCP connectors (all disabled)");
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;

    async fn pool() -> Pool {
        db::open(std::path::Path::new(":memory:")).await.unwrap()
    }

    #[tokio::test]
    async fn seed_is_idempotent_and_disabled() {
        let pool = pool().await;
        let first = seed_defaults(&pool).await.unwrap();
        assert_eq!(first as usize, DEFAULT_CONNECTORS.len());
        // Second run inserts nothing.
        let second = seed_defaults(&pool).await.unwrap();
        assert_eq!(second, 0);
        let all = list_all(&pool).await.unwrap();
        assert_eq!(all.len(), DEFAULT_CONNECTORS.len());
        assert!(all.iter().all(|c| !c.enabled), "seeded rows start disabled");
        // Enabled list is empty until an admin flips one.
        assert!(list_enabled(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn seed_preserves_admin_enable_state() {
        let pool = pool().await;
        seed_defaults(&pool).await.unwrap();
        // Atlassian is the DCR default (no manual client needed).
        assert!(set_enabled(&pool, "atlassian", true).await.unwrap());
        // Re-seed (simulates a restart) must not reset the flag.
        seed_defaults(&pool).await.unwrap();
        let atl = get(&pool, "atlassian").await.unwrap().unwrap();
        assert!(atl.enabled, "admin enable survives re-seed");
        assert!(atl.use_dcr);
        assert!(!atl.needs_setup(), "DCR connector never needs manual setup");
        // GitHub pins explicit OAuth endpoints + no DCR → needs a client id.
        let gh = get(&pool, "github").await.unwrap().unwrap();
        assert!(!gh.use_dcr);
        assert!(gh.needs_setup());
        assert_eq!(
            gh.authorize_url.as_deref(),
            Some("https://github.com/login/oauth/authorize")
        );
    }

    #[tokio::test]
    async fn google_workspace_needs_setup_until_url_set() {
        let pool = pool().await;
        seed_defaults(&pool).await.unwrap();
        let gw = get(&pool, "google_workspace").await.unwrap().unwrap();
        // DCR connector (the self-hosted server is its own OAuth provider)…
        assert!(gw.use_dcr);
        assert!(gw.client_id.is_none());
        // …but it ships with no URL, so it still needs the admin to point it at
        // their self-hosted Google Workspace MCP server before enabling.
        assert!(gw.url.is_empty());
        assert!(
            gw.needs_setup(),
            "self-hosted connector needs its server URL first"
        );
    }

    #[tokio::test]
    async fn dcr_connector_with_url_is_ready() {
        let pool = pool().await;
        seed_defaults(&pool).await.unwrap();
        // Atlassian is DCR *and* ships a fixed URL → ready to enable as-is.
        let atl = get(&pool, "atlassian").await.unwrap().unwrap();
        assert!(atl.use_dcr);
        assert!(!atl.url.is_empty());
        assert!(!atl.needs_setup());
    }

    #[tokio::test]
    async fn gitlab_seeds_both_saas_and_self_managed() {
        let pool = pool().await;
        seed_defaults(&pool).await.unwrap();
        // SaaS/Premium connector is kept (native MCP, OAuth + DCR).
        let saas = get(&pool, "gitlab").await.unwrap().unwrap();
        assert_eq!(saas.auth, AuthKind::OAuth2);
        assert!(saas.use_dcr);
        // CE / self-managed: user-supplied-token bridge, no seeded URL.
        let ce = get(&pool, "gitlab_selfmanaged").await.unwrap().unwrap();
        assert_eq!(ce.auth, AuthKind::StaticBearer);
        assert!(ce.url.is_empty());
        assert!(ce.needs_setup(), "needs the bridge URL before enabling");
    }

    #[tokio::test]
    async fn google_workspace_requests_service_scopes() {
        let pool = pool().await;
        seed_defaults(&pool).await.unwrap();
        let gw = get(&pool, "google_workspace").await.unwrap().unwrap();
        // Must request the service scopes up front: the server otherwise does a
        // base-only login and every tool call fails with "lack required scopes".
        assert!(
            gw.scopes.iter().any(|s| s.contains("gmail.readonly")),
            "google_workspace must seed the gmail.readonly scope"
        );
    }

    #[tokio::test]
    async fn create_update_delete_roundtrip() {
        let pool = pool().await;
        create(
            &pool,
            ConnectorInput {
                key: "custom".into(),
                name: "Custom".into(),
                description: None,
                icon: None,
                category: Some("Other".into()),
                url: "https://example.test/mcp".into(),
                auth: AuthKind::OAuth2,
                use_dcr: true,
                client_id: None,
                client_secret_ct: None,
                client_secret_nonce: None,
                authorize_url: None,
                token_url: None,
                registration_url: None,
                scopes: vec!["a".into(), "b".into()],
                required_role: None,
            },
        )
        .await
        .unwrap();
        let c = get(&pool, "custom").await.unwrap().unwrap();
        assert_eq!(c.scopes, vec!["a".to_string(), "b".to_string()]);
        assert!(!c.seeded);

        update(
            &pool,
            "custom",
            ConnectorInput {
                key: "custom".into(),
                name: "Renamed".into(),
                description: Some("now described".into()),
                icon: None,
                category: Some("Other".into()),
                url: "https://example.test/mcp".into(),
                auth: AuthKind::OAuth2,
                use_dcr: true,
                client_id: None,
                client_secret_ct: None,
                client_secret_nonce: None,
                authorize_url: None,
                token_url: None,
                registration_url: None,
                scopes: vec![],
                required_role: None,
            },
        )
        .await
        .unwrap();
        let c = get(&pool, "custom").await.unwrap().unwrap();
        assert_eq!(c.name, "Renamed");
        assert!(c.scopes.is_empty());

        assert!(delete(&pool, "custom").await.unwrap());
        assert!(get(&pool, "custom").await.unwrap().is_none());
    }
}
