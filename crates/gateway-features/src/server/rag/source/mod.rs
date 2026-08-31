// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Remote file sources — the pluggable half of the RAG indexer.
//!
//! A **provider** is one way of reaching a tree of documents: a WebDAV
//! server (Nextcloud, ownCloud, OpenCloud, generic), Microsoft Graph
//! (OneDrive / SharePoint), Dropbox, S3. The indexer never names one; it
//! asks a [`FileProvider`] to enumerate directories and hand over bytes,
//! and asks [`FileProvider::capabilities`] which sync strategy is
//! available. Adding a provider is a new module plus one line in
//! [`ProviderRegistry::with_builtins`] — no change to the worker, the
//! chunker, the store, the tools, or the admin page.
//!
//! Three ideas carry the abstraction:
//!
//!   * **[`RemoteEntry::id`] is the identity, not the path.** Every serious
//!     file host has a stable per-file id that survives a rename or move
//!     (`oc:fileid`, a Graph `driveItem` id, a Dropbox `id:` handle). Keying
//!     on it means a moved folder of 400 documents is a path update, not a
//!     re-extraction of the lot. Providers with no such id
//!     ([`ProviderCapabilities::stable_ids`] false) fall back to the path.
//!
//!   * **[`RemoteEntry::version`] is opaque.** An etag, a ctag, a Dropbox
//!     `rev`, a content hash — the indexer only ever compares it for
//!     equality against what it stored last time. It never parses one.
//!
//!   * **Enumeration has two shapes.** Walking the tree works everywhere;
//!     [`FileProvider::delta`] is the cheap path where the provider offers a
//!     change feed (Graph `/delta`, Dropbox `list_folder/continue`). The
//!     worker picks by capability, so a provider that gains delta support
//!     later needs no caller change.
//!
//! Providers are described to the admin UI through
//! [`ProviderFactory::config_fields`], so `/rag` renders a credential form
//! for a provider it has never heard of. That is the difference between an
//! extensible design and one that merely has a trait in it.

pub mod gdrive;
pub mod tree;
pub mod webdav;

use std::collections::BTreeMap;
use std::sync::Arc;

use jiff::Timestamp;
use thiserror::Error;

/// What went wrong reaching a source. Split by *what the operator must do
/// about it*, which is what the `/rag` page shows next to a failed
/// collection — see `worker::friendly_error`.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Credentials were rejected. Re-enter them; retrying will not help.
    ///
    /// `hint` is written by the provider, which is the only code that knows
    /// what an operator should actually go and change. Keeping it a field
    /// rather than a match arm further up is what stops provider-specific
    /// advice leaking back into the worker.
    #[error("{provider} rejected the credentials (HTTP {status}). {hint}")]
    Unauthorized {
        provider: &'static str,
        status: u16,
        hint: &'static str,
    },

    /// Authenticated fine, but this account may not read that path.
    #[error("{provider} denied access to `{path}` (HTTP {status}). {hint}")]
    Forbidden {
        provider: &'static str,
        path: String,
        status: u16,
        hint: &'static str,
    },

    /// The path does not exist on the remote.
    #[error("{provider} has no such path: `{path}`. {hint}")]
    NotFound {
        provider: &'static str,
        path: String,
        hint: &'static str,
    },

    /// Reached the server; it answered with something we can't use.
    #[error("{provider} returned HTTP {status}: {body}")]
    Status {
        provider: &'static str,
        status: u16,
        body: String,
    },

    /// The response arrived but did not parse.
    #[error("malformed response from the source: {0}")]
    Malformed(String),

    /// Network / TLS / DNS.
    #[error("could not reach {provider}: {source}")]
    Transport {
        provider: &'static str,
        #[source]
        source: reqwest::Error,
    },

    /// Operator config is wrong (missing field, unparseable URL).
    #[error("{0}")]
    Config(String),

    /// The provider does not implement this optional capability.
    #[error("{provider} does not support {feature}")]
    Unsupported {
        provider: &'static str,
        feature: &'static str,
    },
}

/// Read a response body, refusing to buffer more than `max_bytes`.
///
/// `resp.bytes()` buffers the whole thing and only then lets the caller
/// compare — which is no bound at all against a server that understates (or
/// omits) `content-length`. Google Drive exports declare no size, so that was
/// the one path with nothing holding it back.
///
/// Stops the moment the budget is exceeded, so the peak is one chunk over the
/// limit rather than the whole body.
pub async fn read_capped(
    provider: &'static str,
    path: &str,
    resp: reqwest::Response,
    max_bytes: u64,
) -> Result<Vec<u8>, ProviderError> {
    use rama::futures::StreamExt as _;

    let mut out: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|source| ProviderError::Transport { provider, source })?;
        if out.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(ProviderError::Config(format!(
                "`{path}` is larger than the {max_bytes}-byte limit for indexed files"
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// File or directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
}

/// One item in a remote tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    /// Provider-native stable id. Survives rename/move where the provider
    /// supports it; otherwise the provider repeats `rel_path` here.
    pub id: String,
    /// Provider-native locator used to fetch or list this entry: a WebDAV
    /// href, a Graph item path, a Dropbox path. Never shown to the model.
    pub locator: String,
    /// Forward-slash path relative to the configured root. This is the
    /// provenance the model and the user see, and the store's fallback key.
    pub rel_path: String,
    pub kind: EntryKind,
    /// Opaque change token, compared for equality only. `None` when the
    /// source reports nothing that moves when the file does.
    ///
    /// `None` rather than a stand-in value: the planner reads it as "cannot
    /// tell, re-read", which is the honest answer. A provider that invented a
    /// stable filler — the filename, the path, the size — would pin the file
    /// as never-changing and lose every later edit, silently and permanently.
    /// That is a mistake each provider would otherwise have to avoid on its
    /// own; `Option` makes it unrepresentable.
    pub version: Option<String>,
    pub size_bytes: u64,
    pub mime: Option<String>,
    pub modified_at: Option<Timestamp>,
}

impl RemoteEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
}

/// A directory to enumerate. Carries the version the indexer last saw, so a
/// provider with [`ProviderCapabilities::subtree_pruning`] can answer
/// "unchanged" without listing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirRef {
    pub locator: String,
    pub rel_path: String,
    /// Version stored from the previous sync, if any.
    pub known_version: Option<String>,
}

impl DirRef {
    pub fn root(locator: impl Into<String>) -> Self {
        Self {
            locator: locator.into(),
            rel_path: String::new(),
            known_version: None,
        }
    }
}

/// Result of listing one directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirListing {
    /// The directory's version matches `known_version`, so nothing beneath
    /// it changed. The walker skips the whole subtree — the single biggest
    /// saving in a re-sync, and the reason `subtree_pruning` is a
    /// first-class capability rather than an implementation detail.
    Unchanged,
    /// Entries directly inside the directory, plus its own current version.
    Listed {
        entries: Vec<RemoteEntry>,
        version: Option<String>,
    },
}

/// One page of a provider-native change feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaPage {
    /// Entries created or modified since the cursor.
    pub changed: Vec<RemoteEntry>,
    /// Ids removed since the cursor.
    pub removed: Vec<String>,
    /// Cursor to pass to the next call. `None` ends the feed.
    pub next_cursor: Option<String>,
    /// True when the provider invalidated the cursor and the caller must
    /// fall back to a full walk. Graph and Dropbox both do this.
    pub reset_required: bool,
}

/// What a provider can do. The worker reads this instead of matching on a
/// provider name, which is what keeps `worker.rs` free of provider
/// knowledge.
/// The default is the pessimistic set: a provider that opts into nothing
/// still works, just with a full walk and path-keyed identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProviderCapabilities {
    /// A directory's version changes when anything beneath it changes, so an
    /// unchanged directory can be skipped without descending.
    pub subtree_pruning: bool,
    /// [`FileProvider::delta`] is implemented.
    pub delta: bool,
    /// [`RemoteEntry::id`] survives rename and move.
    pub stable_ids: bool,
}

/// What [`FileProvider::probe`] tells the operator after a "Test connection"
/// click, before they commit to a multi-hour first index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// Account or principal the credentials resolve to, when the provider
    /// can say.
    pub account: Option<String>,
    /// Entries directly under the configured root — enough to tell "wrong
    /// folder" from "empty folder".
    pub root_entries: usize,
    /// Server product/version, for the support case that follows.
    pub server: Option<String>,
}

/// One reachable tree of documents.
#[async_trait::async_trait]
pub trait FileProvider: Send + Sync + 'static {
    /// Stable identifier, stored in `rag_collections.source_kind`.
    fn kind(&self) -> &'static str;

    fn capabilities(&self) -> ProviderCapabilities;

    /// The configured root of the subtree to index.
    fn root(&self) -> DirRef;

    /// List one directory level.
    async fn list_dir(&self, dir: &DirRef) -> Result<DirListing, ProviderError>;

    /// Fetch one file's bytes. `max_bytes` lets a provider refuse early
    /// rather than stream a 4 GB video into memory.
    async fn fetch(&self, entry: &RemoteEntry, max_bytes: u64) -> Result<Vec<u8>, ProviderError>;

    /// Deep link into the provider's own web UI, for citations. `None` when
    /// the provider has no stable per-file URL.
    fn web_url(&self, _entry: &RemoteEntry) -> Option<String> {
        None
    }

    /// Credential + reachability check for the admin UI.
    async fn probe(&self) -> Result<ProbeReport, ProviderError>;

    /// Provider-native change feed. Only called when
    /// [`ProviderCapabilities::delta`] is set.
    ///
    /// **No consumer yet**, and deliberately no schema either: the cursor a
    /// real implementation needs is one `ALTER TABLE` away, and shipping the
    /// column before the code that writes it made it look wired when it was
    /// not. Wiring this up is a branch in `gather_remote`, a cursor threaded
    /// back out of `build_ref_incremental`, and teaching `sync::plan` a shape
    /// with no `TreeSnapshot` and no `is_complete()` — a delta page hands you
    /// removals directly.
    async fn delta(&self, _cursor: Option<&str>) -> Result<DeltaPage, ProviderError> {
        Err(ProviderError::Unsupported {
            provider: self.kind(),
            feature: "delta enumeration",
        })
    }
}

/// How a config field is rendered and validated on `/rag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    /// Rendered as a password input and sealed at rest.
    Secret,
    Url,
    Bool,
}

impl FieldKind {
    /// Wire name, so an API client can render the right control.
    ///
    /// The providers endpoint exists so a client can build a form for a
    /// provider it has no compiled-in knowledge of; publishing only
    /// "is it secret" would render the first provider with a verify-TLS
    /// checkbox as a text box for every such client.
    pub fn as_str(self) -> &'static str {
        match self {
            FieldKind::Text => "text",
            FieldKind::Secret => "secret",
            FieldKind::Url => "url",
            FieldKind::Bool => "bool",
        }
    }
}

/// One operator-supplied setting a provider needs. The admin page renders a
/// form from these, so a new provider needs no page code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigField {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: FieldKind,
    pub required: bool,
    /// Prefilled in the form when the operator has entered nothing.
    pub default: Option<&'static str>,
}

/// Operator-supplied provider settings, already decrypted.
///
/// A flat string map rather than a per-provider struct: the values arrive
/// from an HTML form and a JSON column, and the schema that validates them
/// is [`ProviderFactory::config_fields`]. Secrets are separated so a
/// `Debug` of the non-secret half is safe to log.
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    values: BTreeMap<String, String>,
    secrets: BTreeMap<String, String>,
}

impl ProviderConfig {
    pub fn new(values: BTreeMap<String, String>, secrets: BTreeMap<String, String>) -> Self {
        Self { values, secrets }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values
            .get(key)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }

    pub fn secret(&self, key: &str) -> Option<&str> {
        self.secrets
            .get(key)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }

    /// Required non-secret value, or a config error naming the field.
    pub fn require(&self, key: &str) -> Result<&str, ProviderError> {
        self.get(key)
            .ok_or_else(|| ProviderError::Config(format!("`{key}` is required but was left empty")))
    }

    pub fn bool(&self, key: &str, default: bool) -> bool {
        match self.get(key) {
            Some("true" | "1" | "on" | "yes") => true,
            Some("false" | "0" | "off" | "no") => false,
            _ => default,
        }
    }

    /// A copy with every unset field filled from its declared default, so
    /// the default in [`ConfigField`] is the single place it is written —
    /// the form and the provider read the same value.
    pub fn with_defaults(&self, fields: &[ConfigField]) -> ProviderConfig {
        let mut values = self.values.clone();
        for f in fields {
            let Some(default) = f.default else { continue };
            if f.kind == FieldKind::Secret {
                continue; // a default secret would be a backdoor
            }
            if values.get(f.key).is_none_or(|v| v.is_empty()) {
                values.insert(f.key.to_string(), default.to_string());
            }
        }
        ProviderConfig {
            values,
            secrets: self.secrets.clone(),
        }
    }
}

/// How an operator proves the gateway may read a source.
///
/// The admin form renders from this: `Fields` is the whole story for a
/// provider whose credentials are typed (a WebDAV app password), while
/// `OAuth2` needs a person to consent in a browser and leaves the gateway
/// holding a refresh token it can trade for access tokens forever after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthKind {
    /// Every credential is a [`ConfigField`] the operator fills in.
    Fields,
    /// Three-legged OAuth2. The operator registers a client with the
    /// provider (client id + secret are still `ConfigField`s), then clicks
    /// through a consent screen; the callback seals the refresh token into
    /// the source's secrets under [`REFRESH_TOKEN_KEY`].
    OAuth2 {
        authorize_url: &'static str,
        token_url: &'static str,
        scopes: &'static [&'static str],
        /// Which config key holds the client id, and which secret key holds
        /// the client secret.
        ///
        /// Named by the provider rather than assumed by the consent handler,
        /// because every vendor calls these something different — Dropbox
        /// says "App key", Graph says "Application (client) ID". A provider
        /// that declares `app_key` would otherwise compile, save, and then
        /// fail at Connect with a message telling the operator to fill in a
        /// field they had already filled in.
        client_id_key: &'static str,
        client_secret_key: &'static str,
    },
}

/// Where an OAuth2 provider's refresh token lives in a source's secrets.
///
/// Deliberately not a [`ConfigField`]: it is minted by the consent callback,
/// never typed, and rendering it as a password box would invite an operator
/// to paste something that cannot work.
pub const REFRESH_TOKEN_KEY: &str = "refresh_token";

/// True when this provider needs a browser consent that has not happened yet.
///
/// Asked of the factory rather than by naming a provider, so the next OAuth
/// source inherits the behaviour instead of adding a second special case.
/// Lives beside [`AuthKind`] rather than in either write surface because
/// both of them need it: an OAuth source must be *storable* before anyone
/// can consent (consent needs the saved client id), so both the admin form
/// and the JSON API have to skip their dry-run `build()` in this state or
/// the two deadlock against each other.
pub fn awaiting_consent(factory: &dyn ProviderFactory, secrets: &BTreeMap<String, String>) -> bool {
    matches!(factory.auth(), AuthKind::OAuth2 { .. }) && !secrets.contains_key(REFRESH_TOKEN_KEY)
}

/// Builds providers of one kind and describes their settings.
pub trait ProviderFactory: Send + Sync + 'static {
    fn kind(&self) -> &'static str;

    /// Shown in the source-kind picker.
    fn label(&self) -> &'static str;

    /// One line under the label.
    fn description(&self) -> &'static str;

    fn config_fields(&self) -> &'static [ConfigField];

    /// How this provider is authorised. Default: typed credentials.
    fn auth(&self) -> AuthKind {
        AuthKind::Fields
    }

    /// Which config keys hold secrets, and so must be sealed rather than
    /// stored in the clear. Derived from the declared fields — every caller
    /// that splits a submitted form needs this same answer.
    fn secret_keys(&self) -> Vec<&'static str> {
        self.config_fields()
            .iter()
            .filter(|f| f.kind == FieldKind::Secret)
            .map(|f| f.key)
            .collect()
    }

    fn build(
        &self,
        cfg: &ProviderConfig,
        http: reqwest::Client,
    ) -> Result<Arc<dyn FileProvider>, ProviderError>;

    /// Validate a config without building. Default: every `required` field
    /// present. Providers override to add cross-field rules.
    fn validate(&self, cfg: &ProviderConfig) -> Result<(), ProviderError> {
        for f in self.config_fields() {
            if !f.required {
                continue;
            }
            let present = match f.kind {
                FieldKind::Secret => cfg.secret(f.key).is_some(),
                _ => cfg.get(f.key).is_some(),
            };
            if !present {
                return Err(ProviderError::Config(format!(
                    "`{}` is required but was left empty",
                    f.label
                )));
            }
        }
        Ok(())
    }
}

/// Every provider the gateway knows about, by kind.
///
/// Held on the indexer. Registration is the single extension point: a new
/// provider is a module plus one `register` call.
#[derive(Default)]
pub struct ProviderRegistry {
    factories: Vec<Arc<dyn ProviderFactory>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The providers shipped in the binary.
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        reg.register(Arc::new(webdav::WebdavFactory));
        reg.register(Arc::new(gdrive::GoogleDriveFactory));
        reg
    }

    pub fn register(&mut self, factory: Arc<dyn ProviderFactory>) {
        if let Some(slot) = self
            .factories
            .iter_mut()
            .find(|f| f.kind() == factory.kind())
        {
            *slot = factory;
            return;
        }
        self.factories.push(factory);
    }

    pub fn get(&self, kind: &str) -> Option<&Arc<dyn ProviderFactory>> {
        self.factories.iter().find(|f| f.kind() == kind)
    }

    /// All registered factories, for rendering the source-kind picker.
    pub fn factories(&self) -> &[Arc<dyn ProviderFactory>] {
        &self.factories
    }

    pub fn build(
        &self,
        kind: &str,
        cfg: &ProviderConfig,
        http: reqwest::Client,
    ) -> Result<Arc<dyn FileProvider>, ProviderError> {
        let factory = self.get(kind).ok_or_else(|| {
            let known: Vec<&str> = self.factories.iter().map(|f| f.kind()).collect();
            ProviderError::Config(format!(
                "unknown source kind `{kind}` (known: {})",
                known.join(", ")
            ))
        })?;
        factory.validate(cfg)?;
        factory.build(cfg, http)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)], secrets: &[(&str, &str)]) -> ProviderConfig {
        ProviderConfig::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            secrets
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn empty_string_reads_as_absent() {
        let c = cfg(&[("base_url", "")], &[]);
        assert_eq!(c.get("base_url"), None);
        assert!(c.require("base_url").is_err());
    }

    #[test]
    fn bool_accepts_html_checkbox_spellings() {
        let c = cfg(&[("a", "on"), ("b", "false")], &[]);
        assert!(c.bool("a", false));
        assert!(!c.bool("b", true));
        assert!(c.bool("missing", true), "absent falls back to the default");
    }

    #[test]
    fn unknown_kind_names_the_known_ones() {
        let reg = ProviderRegistry::with_builtins();
        let msg = reg
            .build("dropbox", &cfg(&[], &[]), reqwest::Client::new())
            .map(|_| ())
            .expect_err("an unregistered kind cannot be built")
            .to_string();
        assert!(msg.contains("dropbox"), "{msg}");
        assert!(
            msg.contains("webdav"),
            "the error lists what IS known: {msg}"
        );
    }

    #[test]
    fn registering_the_same_kind_twice_replaces_rather_than_duplicates() {
        let mut reg = ProviderRegistry::with_builtins();
        let before = reg.factories().len();
        reg.register(Arc::new(webdav::WebdavFactory));
        assert_eq!(reg.factories().len(), before);
    }

    #[test]
    fn validate_rejects_a_missing_required_secret_by_label() {
        let reg = ProviderRegistry::with_builtins();
        let factory = reg.get("webdav").expect("webdav is built in");
        // base_url + username present, password (a required secret) missing.
        let err = factory
            .validate(&cfg(
                &[
                    ("base_url", "https://cloud.example.com"),
                    ("username", "svc"),
                ],
                &[],
            ))
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("password"),
            "the message names the field the operator has to fill: {err}"
        );
    }

    #[test]
    fn default_capabilities_are_the_pessimistic_ones() {
        let caps = ProviderCapabilities::default();
        assert!(!caps.subtree_pruning);
        assert!(!caps.delta);
        assert!(!caps.stable_ids);
    }
}
