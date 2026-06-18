// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Runtime configuration for the gateway.
//!
//! Loaded once at startup from a TOML file. Secrets (the upstream API key)
//! are sourced from environment variables — the file only names the env var.
//!
//! Lookup order:
//! 1. `$GATEWAY_CONFIG` (explicit path)
//! 2. `./gateway.toml`
//! 3. `/etc/gateway/config.toml`
//! 4. Built-in defaults (no upstream configured — proxy routes return 503).
//!
//! See `gateway.example.toml` at the repo root for the schema.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::server::rbac::config::{RbacConfig, RoleConfig};
use crate::server::upstreams::config::UpstreamPoolConfig;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("reading config file `{path}`")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing config file `{path}`")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub bind: BindConfig,
    pub db: DbConfig,
    /// Named upstream pools: `[upstream_pools.<name>]` blocks in TOML.
    /// Routes from model name → pool are *not* declared here; they're
    /// derived at runtime from each backend's `/models` response (see
    /// `upstreams::health`). Add a backend in the right kind of pool
    /// and any model it serves becomes routable automatically.
    #[serde(default)]
    pub upstream_pools: HashMap<String, UpstreamPoolConfig>,
    pub oidc: Option<OidcConfig>,
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub rbac: RbacConfig,
    #[serde(default, rename = "roles")]
    pub roles: Vec<RoleConfig>,
    /// Chat-page knobs that aren't routing-related — attachment
    /// storage + which model names are allowed to receive image
    /// content. Optional; defaults are conservative (S3 disabled,
    /// no vision models advertised so attachments error if S3
    /// isn't configured anyway).
    #[serde(default)]
    pub chat: ChatConfig,
    /// Typst-templated document generation. Optional — when unset,
    /// no `typst_*` tools register and the gateway boots fine.
    /// When set, the directory is scanned at startup for subdirs
    /// containing a `template.toml` manifest; each becomes one tool
    /// the model can call to produce a rendered PDF + PNG + .typ
    /// source from corporate-design templates.
    #[serde(default)]
    pub typst: Option<TypstConfig>,
    /// GeoIP (client-IP → coarse location) for the `get_user_location`
    /// tool. Optional — with no `[geoip]` block no database is loaded and
    /// the tool falls back to the browser-provided position (or reports
    /// the location as unknown). See `server::geoip`.
    #[serde(default)]
    pub geoip: Option<GeoipConfig>,
    /// External MCP (Model Context Protocol) servers to bridge. Optional —
    /// with no `[mcp]` block the gateway exposes only its built-in tools.
    /// Each `[[mcp.servers]]` is connected at startup and its tools appear
    /// in chat as `mcp__<server>__<tool>`. See `server::tools::mcp`.
    #[serde(default)]
    pub mcp: Option<McpConfig>,
    /// RAG indexer state directory + tuning. Optional — with no `[rag]`
    /// block the indexer falls back to `data/rag` relative to the
    /// gateway's CWD, which is fine for local dev but NOT for the
    /// container image (its rootfs is read-only). Operators MUST point
    /// `data_dir` at a writable path (typically a subdirectory of the
    /// same named volume that backs `[db].path`).
    #[serde(default)]
    pub rag: Option<RagConfig>,
}

/// RAG indexer state directory + tuning knobs.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RagConfig {
    /// Where the indexer stores its per-collection usearch files and
    /// the git clone cache. The gateway will `mkdir -p` this on
    /// startup, so the **parent** must already exist + be writable by
    /// the runtime user (uid 1000 in the container image). Default is
    /// `data/rag` relative to the gateway's working directory.
    #[serde(default = "default_rag_data_dir")]
    pub data_dir: PathBuf,
}

impl Default for RagConfig {
    fn default() -> Self {
        Self {
            data_dir: default_rag_data_dir(),
        }
    }
}

fn default_rag_data_dir() -> PathBuf {
    PathBuf::from("data/rag")
}

/// External MCP servers the gateway connects to as a client.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    /// One entry per server. Connected concurrently at startup; a server
    /// that fails to connect (or times out) is logged and skipped — it
    /// never blocks boot.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// One MCP server. Exactly one transport must be set: `command` (+ optional
/// `args` / `env`) spawns it over stdio; `url` connects over streamable HTTP.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Stable label. Namespaces the server's tools (`mcp__<name>__<tool>`)
    /// and keys its `/tools` toggle. Keep it short and `[a-z0-9_-]`.
    pub name: String,
    /// Set to false to declare a server without connecting to it.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// stdio transport: the executable to spawn (e.g. `npx`).
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the spawned process, layered on top of the
    /// gateway's own env (which the child inherits — so secrets already in
    /// the process environment reach the server without being repeated here).
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// HTTP transport: the server's streamable-HTTP endpoint URL.
    #[serde(default)]
    pub url: Option<String>,
    /// HTTP transport: name of an env var holding a bearer token. When set,
    /// the gateway sends `Authorization: Bearer <token>` on every request to
    /// this server. The secret stays in the environment, not the config file
    /// (same `*_env` convention as the rest of the config). Ignored for stdio.
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    /// HTTP transport: extra request headers sent verbatim — e.g. an
    /// `X-Api-Key` for servers that don't use bearer auth, or a tenant id.
    /// For a bearer secret prefer `bearer_token_env`. Ignored for stdio.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

impl McpServerConfig {
    /// Resolve the bearer token from its named env var, if configured and
    /// non-empty. Same env-indirection as [`GeoipConfig::update_token`].
    pub fn bearer_token(&self) -> Option<String> {
        self.bearer_token_env
            .as_ref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|v| !v.is_empty())
    }
}

fn default_true() -> bool {
    true
}

/// GeoIP settings. Points at an IP2Location LITE DB11 `.BIN` and,
/// optionally, names the env var holding an IP2Location download token
/// for the weekly auto-updater — same "the file holds the env-var NAME,
/// not the secret" pattern as [`S3Config`] / [`OidcConfig`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeoipConfig {
    /// Path to the IP2Location LITE DB11 BIN. A missing file is not an
    /// error — lookups simply return nothing until one appears. Changes
    /// are hot-reloaded (no restart).
    #[serde(default = "default_geoip_db_path")]
    pub db_path: PathBuf,
    /// Name of the env var holding the IP2Location download token. When
    /// set (and non-empty) a weekly background task refreshes `db_path`.
    /// Unset → no auto-update; operators can drop in their own BIN and it
    /// gets picked up by the hot-reload watcher.
    pub update_token_env: Option<String>,
}

fn default_geoip_db_path() -> PathBuf {
    PathBuf::from("data/ip2location/IP2LOCATION-LITE-DB11.BIN")
}

impl GeoipConfig {
    /// Resolve the download token from its named env var, if configured.
    pub fn update_token(&self) -> Option<String> {
        self.update_token_env
            .as_ref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|v| !v.is_empty())
    }
}

/// Typst document-rendering settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypstConfig {
    /// Root directory holding one subdirectory per template. Each
    /// subdir must contain `template.toml` (manifest) + `template.typ`
    /// (source). Co-located assets (logos, fonts) work because the
    /// typst compile is run with `--root` pointing at the subdir.
    pub templates_dir: PathBuf,
}

/// Chat-page settings. Attachments go to S3 (object storage) so the
/// DB doesn't bloat and the OpenAI API can fetch image URLs directly
/// from the bucket. We don't gate which model gets to receive
/// image content — operators are expected to wire only multi-modal
/// chat models into the gateway, and any capability mismatch
/// surfaces as the upstream's error on send.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatConfig {
    pub s3: Option<S3Config>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Config {
    /// S3 (or S3-compatible) endpoint. e.g.
    /// `https://s3.amazonaws.com`, `https://minio.local`. The
    /// gateway both uploads to this host AND hands presigned URLs
    /// rooted at this host to the upstream LLM, so it must be
    /// reachable from the LLM's network (not just the gateway's).
    pub endpoint: String,
    /// AWS region; for S3-compatible stores (MinIO/Backblaze)
    /// this is often a placeholder like `us-east-1`.
    pub region: String,
    pub bucket: String,
    /// Env var holding the AWS access key id. Same pattern as
    /// `session_key_env` — file holds the env-var NAME so secrets
    /// stay out of TOML.
    pub access_key_env: String,
    /// Env var holding the AWS secret access key.
    pub secret_key_env: String,
    /// Object-key prefix under which chat attachments live. Default
    /// `chat-attachments`. Useful when the bucket is shared with
    /// other workloads.
    #[serde(default = "default_s3_prefix")]
    pub key_prefix: String,
}

fn default_s3_prefix() -> String {
    "chat-attachments".to_string()
}

impl S3Config {
    pub fn access_key(&self) -> Option<String> {
        std::env::var(&self.access_key_env)
            .ok()
            .filter(|v| !v.is_empty())
    }
    pub fn secret_key(&self) -> Option<String> {
        std::env::var(&self.secret_key_env)
            .ok()
            .filter(|v| !v.is_empty())
    }
}

/// Knobs the gateway itself needs (separate from `bind` which only describes
/// where to listen).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// Public URL the gateway is reachable at (used to build OIDC redirect URI
    /// and CLI handoff URLs). E.g. `https://gateway.example.com`.
    pub public_url: String,
    /// How long a freshly minted gateway token is valid for. Default 90 days.
    pub token_ttl_days: i64,
    /// Cookie key for sessions. 64 hex chars (32 bytes). Generate with
    /// `openssl rand -hex 32` and put it in `$GATEWAY_SESSION_KEY` rather than
    /// in this file — the file is for the *name* of the env var.
    pub session_key_env: String,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            public_url: "http://localhost:8080".into(),
            token_ttl_days: 90,
            session_key_env: "GATEWAY_SESSION_KEY".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    /// Name of the env var holding the OIDC client secret. Never the secret
    /// itself.
    pub client_secret_env: String,
    /// Scopes to request, on top of `openid` which is always included.
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// OIDC claim that holds the user's role memberships (e.g. "groups").
    /// Mapped to internal roles in Phase 5.
    pub roles_claim: Option<String>,
}

fn default_scopes() -> Vec<String> {
    vec!["email".into(), "profile".into()]
}

impl OidcConfig {
    pub fn client_secret(&self) -> Option<String> {
        std::env::var(&self.client_secret_env)
            .ok()
            .filter(|v| !v.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbConfig {
    /// SQLite file path. `:memory:` (used in tests) gives an in-memory DB.
    pub path: PathBuf,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("gateway.sqlite"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindConfig {
    pub host: String,
    pub port: u16,
}

impl Default for BindConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
        }
    }
}

impl Config {
    /// Resolves the config file path and loads it. Missing files are not an
    /// error — we fall back to defaults so `mise run dev` can start without
    /// any setup.
    pub fn load() -> Result<Self, ConfigError> {
        match Self::resolve_path() {
            Some(path) => Self::from_path(&path),
            None => {
                tracing::warn!(
                    "no gateway.toml found; running with defaults. \
                     Proxy routes will return 503 until upstream is configured."
                );
                Ok(Self::default())
            }
        }
    }

    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    fn resolve_path() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("GATEWAY_CONFIG") {
            return Some(PathBuf::from(p));
        }
        for candidate in ["gateway.toml", "/etc/gateway/config.toml"] {
            let p = PathBuf::from(candidate);
            if p.exists() {
                return Some(p);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_no_upstreams_and_bind_to_localhost() {
        let c = Config::default();
        assert!(c.upstream_pools.is_empty());
        assert_eq!(c.bind.host, "127.0.0.1");
        assert_eq!(c.bind.port, 8080);
    }

    #[test]
    fn parses_full_config() {
        let toml = r#"
            [bind]
            host = "0.0.0.0"
            port = 9000

            [upstream_pools.local_chat]
            kind = "chat"
            strategy = "round_robin"

            [[upstream_pools.local_chat.backend]]
            name = "gpu-01"
            base_url = "http://gpu-01:8000/v1"
            api_key_env = "GPU01_KEY"

            [[upstream_pools.local_chat.backend]]
            name = "gpu-02"
            base_url = "http://gpu-02:8000/v1"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.bind.host, "0.0.0.0");
        assert_eq!(c.bind.port, 9000);
        let pool = &c.upstream_pools["local_chat"];
        assert_eq!(pool.backend.len(), 2);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let toml = r#"
            [upstream_pools.x]
            kind = "chat"
            mystery_field = true

            [[upstream_pools.x.backend]]
            name = "a"
            base_url = "http://a"
        "#;
        let err = toml::from_str::<Config>(toml).unwrap_err();
        assert!(err.to_string().contains("mystery_field"), "{err}");
    }
}
