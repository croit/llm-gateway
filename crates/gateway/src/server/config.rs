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
    /// Agent Skills the gateway makes available to the chat model.
    /// Optional — with no `[skills]` block none are loaded and the
    /// `read_skill` tool is not registered. When set, `dir` is scanned at
    /// startup for skill bundles (a directory holding a `SKILL.md`, or a
    /// `*.skill` zip of one); each becomes an operator-managed capability
    /// the model can load on demand. RBAC-gated per role via the role's
    /// `skills` list, exactly like `tools`. See `server::skills`.
    #[serde(default)]
    pub skills: Option<SkillsConfig>,
    /// Usage accounting (per-user / per-backend request metrics). Always
    /// present with sane defaults — there's no way to mis-configure it into
    /// failing a request, since recording is fire-and-forget. Set
    /// `[usage] enabled = false` to turn measurement off entirely, or tune
    /// `retention_days` to bound the raw-event window. See `server::usage`.
    #[serde(default)]
    pub usage: UsageConfig,
    /// Code-execution sandbox. Optional — with no `[sandbox]` block the
    /// `run_in_sandbox` tool family is not registered and the gateway
    /// boots fine. When set, `runner_url` points at the standalone
    /// `sandbox-runner` service, which holds podman access and executes
    /// untrusted/LLM code in single-use sandboxes; the gateway only
    /// talks HTTP to it. See `server::tools::sandbox`.
    #[serde(default)]
    pub sandbox: Option<SandboxConfig>,
    /// Feedback widget. Optional — with no `[feedback]` block the floating
    /// feedback button stays hidden (the `/feedback/config` endpoint reports
    /// it unconfigured, so the client never reveals the FAB). When set,
    /// every signed-in user gets a floating button that opens a form — with
    /// optional voice-to-fields dictation — and files the submission as a
    /// GitHub issue. See `server::github` + `rama_server::pages::feedback`.
    #[serde(default)]
    pub feedback: Option<FeedbackConfig>,
}

/// Feedback-widget settings: where issues are filed and how the voice
/// transcript is turned into structured fields.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackConfig {
    /// GitHub repository owner (user or org) that receives the issues,
    /// e.g. `croit`.
    pub github_owner: String,
    /// GitHub repository name, e.g. `llm-gateway`.
    pub github_repo: String,
    /// GitHub API token (classic PAT or fine-grained) able to open issues
    /// (`issues:write`) and — when screenshots are attached — commit asset
    /// files (`contents:write`). Per request this may live directly in the
    /// config file. If you'd rather keep it in the environment, leave this
    /// unset and use `github_token_env`; the direct value wins when both
    /// are present.
    #[serde(default)]
    pub github_token: Option<String>,
    /// Alternative to `github_token`: the NAME of an env var holding the
    /// token (same `*_env` convention as the rest of the config). Consulted
    /// only when `github_token` is unset/empty.
    #[serde(default)]
    pub github_token_env: Option<String>,
    /// Labels applied to every created issue, in addition to the automatic
    /// `priority:<p>`. Default `["feedback"]`.
    #[serde(default = "default_feedback_labels")]
    pub labels: Vec<String>,
    /// Orphan branch holding embedded screenshot assets: each screenshot is
    /// committed as a file and linked into the issue body via its raw URL.
    /// Created off the default branch on first use if missing. Default
    /// `feedback-assets`.
    #[serde(default = "default_feedback_assets_branch")]
    pub assets_branch: String,
    /// Chat model id used to turn a voice transcript into the structured
    /// form fields ("text model"). Unset/empty → the gateway picks the first
    /// available chat model at request time. This is an operator choice, not
    /// the end user's — the form never exposes a model picker.
    #[serde(default)]
    pub extraction_model: Option<String>,
    /// Transcription model id used to turn the voice recording into text
    /// ("voice model"). Unset/empty → the gateway picks the first available
    /// transcription model at request time. Operator choice, not the user's.
    #[serde(default)]
    pub voice_model: Option<String>,
    /// GitHub REST API base URL. Default `https://api.github.com`; override
    /// for GitHub Enterprise (`https://github.example.com/api/v3`).
    #[serde(default = "default_github_api_base")]
    pub github_api_base: String,
}

fn default_feedback_labels() -> Vec<String> {
    vec!["feedback".to_string()]
}

fn default_feedback_assets_branch() -> String {
    "feedback-assets".to_string()
}

fn default_github_api_base() -> String {
    "https://api.github.com".to_string()
}

impl FeedbackConfig {
    /// Resolve the GitHub token: the inline `github_token` first, then the
    /// env var named by `github_token_env`. Empty strings count as unset.
    pub fn github_token(&self) -> Option<String> {
        if let Some(tok) = self.github_token.as_ref().filter(|v| !v.is_empty()) {
            return Some(tok.clone());
        }
        self.github_token_env
            .as_ref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|v| !v.is_empty())
    }

    /// True when enough is configured to actually open an issue: owner,
    /// repo, and a resolvable token.
    pub fn is_configured(&self) -> bool {
        !self.github_owner.is_empty()
            && !self.github_repo.is_empty()
            && self.github_token().is_some()
    }
}

/// Sandbox tool settings. The heavy lifting (isolation, warm pool, egress
/// allowlist) lives in the separate `sandbox-runner` service; the gateway
/// just needs to know where it is and how patient to be.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Master switch. `true` (default) registers the sandbox tools; set
    /// `false` to turn the whole feature off without deleting the block
    /// (e.g. to keep `runner_url` around while disabling it). Per-tool and
    /// per-user/-token control is separate (RBAC + the `/tools` toggles).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Base URL of the sandbox-runner service, e.g.
    /// `http://sandbox-runner:9000`. MUST be reachable only from the
    /// gateway (internal network / mTLS) — it executes arbitrary code.
    pub runner_url: String,
    /// HTTP timeout for a single `/run` call. Should exceed the runner's
    /// own per-job timeout plus sandbox cold-start margin.
    #[serde(default = "default_sandbox_timeout")]
    pub timeout_secs: u64,
    /// Largest single produced file the gateway will accept back from a
    /// run and store. Larger artifacts are dropped with a note in the
    /// tool result rather than bloating storage / the model context.
    #[serde(default = "default_sandbox_max_artifact")]
    pub max_artifact_bytes: u64,
}

fn default_sandbox_timeout() -> u64 {
    120
}

fn default_sandbox_max_artifact() -> u64 {
    25 * 1024 * 1024
}

/// Usage-metrics knobs. Recording is decoupled from the request path (a
/// bounded channel drained by a background batched writer), so these only
/// affect how much history is kept and whether measurement runs at all.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsageConfig {
    /// Master switch. When `false`, no `UsageRecord`s are emitted, the
    /// writer/maintenance tasks aren't spawned, and hot paths skip the
    /// record-building work entirely — a production kill switch if metrics
    /// ever cost too much. The `/usage` page still renders (with a "metrics
    /// disabled" notice). Default `true`.
    pub enabled: bool,
    /// How many days of raw `usage_events` rows to keep. Older rows are
    /// pruned hourly; the `usage_daily` rollups are kept forever regardless.
    /// Must comfortably exceed the longest UI period ("start of last month",
    /// ~62 days back) so those queries stay on the precise raw path. Default
    /// 90.
    pub retention_days: i64,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: 90,
        }
    }
}

/// Skills directory. Mirrors `[rag] data_dir` / `[typst] templates_dir`:
/// a single operator-owned folder scanned once at startup. No hot-reload —
/// restart to pick up new or changed skills.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsConfig {
    /// Root holding one skill per entry. An entry is either a directory
    /// containing a `SKILL.md` (optionally nested one level, e.g. the
    /// folder a `*.skill` archive unzips to) or a `*.skill` file (a zip of
    /// such a directory), which is extracted into `<dir>/.cache/` at
    /// startup. The gateway only reads this directory; operators populate
    /// it out-of-band (drop a bundle in and restart), just like Typst
    /// templates. Default is `data/skills` relative to the gateway's
    /// working directory.
    #[serde(default = "default_skills_dir")]
    pub dir: PathBuf,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            dir: default_skills_dir(),
        }
    }
}

fn default_skills_dir() -> PathBuf {
    PathBuf::from("data/skills")
}

/// RAG indexer state directory + tuning knobs.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RagConfig {
    /// Root for all per-collection RAG storage. Each collection gets its
    /// own self-contained folder `<data_dir>/<uuid>/` holding `rag.sqlite`
    /// (chunk text + FTS index), `index.usearch` (vectors), and `clone/`
    /// (the git working tree). This is the only heavy / regenerable state,
    /// so keep it separate from the precious central `[db].path` — e.g. on
    /// a larger or cheaper drive/mount. The gateway `mkdir -p`s this on
    /// startup, so the **parent** must already exist + be writable by the
    /// runtime user (uid 1000 in the container image). Default is
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
    /// Whether admins may impersonate other users from `/admin/users`.
    /// Default `false` (opt-in) — impersonation is a powerful, privileged
    /// capability, so it's off unless explicitly enabled. Set
    /// `allow_impersonation = true` to turn it on; the Impersonate buttons
    /// then appear and `POST /admin/users/impersonate` works. While disabled
    /// the buttons are hidden and that endpoint returns 403. Stopping an
    /// already-active impersonation (`/impersonate/stop`) always works, so
    /// nobody is trapped mid-session if the flag is flipped at runtime.
    #[serde(default)]
    pub allow_impersonation: bool,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            public_url: "http://localhost:8080".into(),
            token_ttl_days: 90,
            session_key_env: "GATEWAY_SESSION_KEY".into(),
            allow_impersonation: false,
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

    #[test]
    fn feedback_block_parses_with_inline_token_and_defaults() {
        let toml = r#"
            [feedback]
            github_owner = "croit"
            github_repo  = "llm-gateway"
            github_token = "ghp_inline"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        let f = c.feedback.expect("feedback block");
        assert!(f.is_configured());
        assert_eq!(f.github_token().as_deref(), Some("ghp_inline"));
        // Defaults applied.
        assert_eq!(f.labels, vec!["feedback"]);
        assert_eq!(f.assets_branch, "feedback-assets");
        assert_eq!(f.github_api_base, "https://api.github.com");
    }

    #[test]
    fn feedback_without_token_is_not_configured() {
        let toml = r#"
            [feedback]
            github_owner = "croit"
            github_repo  = "llm-gateway"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        let f = c.feedback.expect("feedback block");
        // No inline token and no env var named → not configured.
        assert!(f.github_token().is_none());
        assert!(!f.is_configured());
    }

    #[test]
    fn no_feedback_block_means_disabled() {
        let c: Config = toml::from_str("").unwrap();
        assert!(c.feedback.is_none());
    }
}
