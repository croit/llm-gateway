// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! WebDAV provider — Nextcloud, ownCloud, OpenCloud, and generic servers.
//!
//! One implementation covers the whole family because they differ only in
//! two places, both handled by configuration and capability detection
//! rather than by branching on a product name:
//!
//!   * **Where the DAV root lives.** `dav_path` is a template
//!     (`/remote.php/dav/files/{username}` for the ownCloud lineage,
//!     `/` or `/dav` elsewhere), so pointing at a different server is a
//!     settings change.
//!
//!   * **Whether the server carries the ownCloud extension properties.**
//!     `oc:fileid` and propagating collection etags are what make identity
//!     survive a move and make a re-sync cheap. A response carrying
//!     `oc:fileid` proves the extension is live, so the provider reports
//!     [`ProviderCapabilities::stable_ids`] and `subtree_pruning`. A plain
//!     RFC 4918 server reports neither and gets a correct, slower sync:
//!     paths as identity, no subtree skipping. Nothing else in the indexer
//!     changes.
//!
//! The pruning that capability enables is the reason a nightly re-sync of an
//! unchanged corpus costs a handful of requests instead of a full walk. On
//! the ownCloud lineage a collection's etag changes whenever anything
//! beneath it changes and the change propagates to the root, so an unchanged
//! etag proves an unchanged subtree. That is a guarantee of *those* servers,
//! not of WebDAV, which is exactly why it is gated behind detection.

use std::sync::Arc;
use std::sync::OnceLock;

use jiff::Timestamp;
use quick_xml::Reader;
use quick_xml::events::Event;

use super::{
    ConfigField, DirListing, DirRef, EntryKind, FieldKind, FileProvider, ProbeReport,
    ProviderCapabilities, ProviderConfig, ProviderError, ProviderFactory, RemoteEntry,
};

const KIND: &str = "webdav";

/// The PROPFIND body. Requests the ownCloud extension properties alongside
/// the standard ones; a server that doesn't know them answers with a 404
/// propstat for those, which the parser drops.
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:prop>
    <d:resourcetype/>
    <d:getetag/>
    <d:getcontentlength/>
    <d:getcontenttype/>
    <d:getlastmodified/>
    <oc:fileid/>
    <oc:permissions/>
    <oc:size/>
  </d:prop>
</d:propfind>"#;

const FIELDS: &[ConfigField] = &[
    ConfigField {
        key: "base_url",
        label: "Server URL",
        help: "Root of the server, e.g. https://cloud.example.com — no path.",
        kind: FieldKind::Url,
        required: true,
        default: None,
    },
    ConfigField {
        key: "username",
        label: "Account",
        help: "The account the gateway signs in as. It indexes exactly what \
               this account can see, so give it access to the folders you want \
               indexed and nothing else.",
        kind: FieldKind::Text,
        required: true,
        default: None,
    },
    ConfigField {
        key: "password",
        label: "App password",
        help: "Create a dedicated app password rather than using the account \
               password. Stored encrypted at rest.",
        kind: FieldKind::Secret,
        required: true,
        default: None,
    },
    ConfigField {
        key: "dav_path",
        label: "DAV path",
        help: "Path to the WebDAV endpoint. `{username}` is substituted. The \
               default is right for Nextcloud, ownCloud and OpenCloud; a plain \
               WebDAV server usually wants `/` or `/dav`.",
        kind: FieldKind::Text,
        required: false,
        default: Some("/remote.php/dav/files/{username}"),
    },
    ConfigField {
        key: "root",
        label: "Folder to index",
        help: "Subfolder to index, e.g. /Finance/Invoices. Leave empty for \
               everything the account can see.",
        kind: FieldKind::Text,
        required: false,
        default: None,
    },
    ConfigField {
        key: "web_url_template",
        label: "File link template",
        help: "Used to link an answer back to the original file. `{base_url}`, \
               `{fileid}` and `{path}` are substituted. Leave empty to use the \
               server default.",
        kind: FieldKind::Text,
        required: false,
        default: Some("{base_url}/f/{fileid}"),
    },
];

pub struct WebdavFactory;

impl ProviderFactory for WebdavFactory {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn label(&self) -> &'static str {
        "WebDAV (Nextcloud, ownCloud, OpenCloud)"
    }

    fn description(&self) -> &'static str {
        "Indexes a folder tree over WebDAV. Detects the ownCloud extension \
         properties automatically and uses them for cheap re-syncs and \
         move-proof file identity when the server offers them."
    }

    fn config_fields(&self) -> &'static [ConfigField] {
        FIELDS
    }

    fn build(
        &self,
        cfg: &ProviderConfig,
        http: reqwest::Client,
    ) -> Result<Arc<dyn FileProvider>, ProviderError> {
        Ok(Arc::new(WebdavProvider::from_config(cfg, http)?) as Arc<dyn FileProvider>)
    }
}

pub struct WebdavProvider {
    /// `https://host` with no trailing slash.
    base_url: String,
    /// Absolute DAV path with `{username}` substituted, no trailing slash.
    dav_root: String,
    /// Collection-relative root, `""` or `foo/bar` (no leading/trailing `/`).
    root: String,
    username: String,
    password: String,
    web_url_template: Option<String>,
    http: reqwest::Client,
    /// Whether the ownCloud extension properties are live on this server.
    /// Learned from the first response that carries (or doesn't carry) an
    /// `oc:fileid`, then latched — it is a property of the deployment, not
    /// of a request. Unset until that first response lands, which is why
    /// nothing may sample it before one has.
    extensions: OnceLock<bool>,
}

impl WebdavProvider {
    pub fn from_config(cfg: &ProviderConfig, http: reqwest::Client) -> Result<Self, ProviderError> {
        let cfg = &cfg.with_defaults(FIELDS);
        let base_url = cfg.require("base_url")?.trim_end_matches('/').to_string();
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(ProviderError::Config(format!(
                "Server URL must start with http:// or https:// (got `{base_url}`)"
            )));
        }
        let username = cfg.require("username")?.to_string();
        let password = cfg
            .secret("password")
            .ok_or_else(|| {
                ProviderError::Config("An app password is required but was left empty".into())
            })?
            .to_string();
        let dav_path = cfg.require("dav_path")?.replace("{username}", &username);
        // `/` is the documented value for a plain WebDAV server, and means
        // "the DAV tree is the site root" — i.e. no prefix at all. Rendering
        // it as `"/"` made every URL `https://host//docs/...` and made
        // `href_prefix()` `"//docs"`, which never matches an href the server
        // returns: the collection indexed zero files and said nothing.
        let trimmed = dav_path.trim_matches('/');
        let dav_root = if trimmed.is_empty() {
            String::new()
        } else {
            format!("/{trimmed}")
        };
        let root = normalize_rel(cfg.get("root").unwrap_or(""));
        let web_url_template = cfg.get("web_url_template").map(str::to_string);
        Ok(Self {
            base_url,
            dav_root,
            root,
            username,
            password,
            web_url_template,
            http,
            extensions: OnceLock::new(),
        })
    }

    /// Absolute URL for a collection-relative path.
    fn url_for(&self, rel: &str) -> String {
        let rel = normalize_rel(rel);
        let mut path = self.dav_root.clone();
        if !self.root.is_empty() {
            path.push('/');
            path.push_str(&encode_path(&self.root));
        }
        if !rel.is_empty() {
            path.push('/');
            path.push_str(&encode_path(&rel));
        }
        format!("{}{}", self.base_url, path)
    }

    /// The DAV path prefix every href under our root shares, used to turn an
    /// absolute href back into a collection-relative path.
    fn href_prefix(&self) -> String {
        let mut p = self.dav_root.clone();
        if !self.root.is_empty() {
            p.push('/');
            p.push_str(&self.root);
        }
        p
    }

    fn note_extensions(&self, saw_fileid: bool) {
        let _ = self.extensions.set(saw_fileid);
    }

    fn has_extensions(&self) -> bool {
        self.extensions.get().copied().unwrap_or(false)
    }

    async fn propfind(&self, url: &str, depth: &str) -> Result<String, ProviderError> {
        let resp = self
            .http
            .request(
                reqwest::Method::from_bytes(b"PROPFIND").expect("PROPFIND is a valid method"),
                url,
            )
            .basic_auth(&self.username, Some(&self.password))
            .header("Depth", depth)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/xml; charset=utf-8",
            )
            .body(PROPFIND_BODY)
            .send()
            .await
            .map_err(|source| ProviderError::Transport {
                provider: KIND,
                source,
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(self.status_error(status.as_u16(), url, resp.text().await));
        }
        resp.text()
            .await
            .map_err(|source| ProviderError::Transport {
                provider: KIND,
                source,
            })
    }

    fn status_error(
        &self,
        status: u16,
        url: &str,
        body: Result<String, reqwest::Error>,
    ) -> ProviderError {
        match status {
            401 => ProviderError::Unauthorized {
                provider: KIND,
                status,
                hint: "Check the account name and app password on the collection.",
            },
            403 => ProviderError::Forbidden {
                provider: KIND,
                path: url.to_string(),
                status,
                hint: "The account signed in but may not read that folder. Share the folder \
                       with the account, or point the collection at one it can see.",
            },
            404 => ProviderError::NotFound {
                provider: KIND,
                path: url.to_string(),
                hint: "Check the folder path — and the DAV path, which differs on servers \
                       outside the Nextcloud / ownCloud family.",
            },
            _ => ProviderError::Status {
                provider: KIND,
                status,
                body: body.unwrap_or_default().chars().take(400).collect(),
            },
        }
    }

    /// Turn parsed DAV responses into entries relative to our root, dropping
    /// the self-entry (the directory the PROPFIND was issued against).
    fn to_entries(&self, dir_rel: &str, responses: Vec<DavResponse>) -> Vec<RemoteEntry> {
        let prefix = self.href_prefix();
        let mut out = Vec::new();
        for r in responses {
            let path = decode_percent(href_path(&r.href));
            let Some(rel) = path.strip_prefix(&prefix) else {
                continue;
            };
            let rel = normalize_rel(rel);
            if rel == normalize_rel(dir_rel) {
                continue; // the directory itself
            }
            out.push(RemoteEntry {
                id: r.fileid.clone().unwrap_or_else(|| rel.clone()),
                locator: rel.clone(),
                rel_path: rel,
                kind: if r.is_collection {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                // A server that omits getetag leaves us with the modified
                // time; both are opaque change tokens to the caller.
                //
                // With neither, the filename used to stand in — and since
                // versions are only ever compared for equality, that pins the
                // file as never-changing: indexed once, and every later edit
                // invisible, permanently and silently. `weak_version` says
                // "we cannot tell", which costs a re-read per sync for the
                // few files affected instead of losing their updates.
                version: r
                    .etag
                    .clone()
                    .or_else(|| r.last_modified.clone())
                    .unwrap_or_else(|| weak_version(r.content_length)),
                size_bytes: r.content_length.unwrap_or(0),
                mime: r.content_type.clone(),
                modified_at: r.last_modified.as_deref().and_then(parse_http_date),
            });
        }
        out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        out
    }
}

#[async_trait::async_trait]
impl FileProvider for WebdavProvider {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn capabilities(&self) -> ProviderCapabilities {
        let ext = self.has_extensions();
        ProviderCapabilities {
            // Collection etags propagate on the ownCloud lineage only. RFC
            // 4918 says nothing about it, so a generic server gets a full
            // walk rather than a wrong answer.
            subtree_pruning: ext,
            delta: false,
            stable_ids: ext,
        }
    }

    fn root(&self) -> DirRef {
        DirRef::root(String::new())
    }

    async fn list_dir(&self, dir: &DirRef) -> Result<DirListing, ProviderError> {
        let url = self.url_for(&dir.locator);
        let xml = self.propfind(&url, "1").await?;
        let responses = parse_multistatus(&xml)?;
        self.note_extensions(responses.iter().any(|r| r.fileid.is_some()));

        // An href that does not start with our prefix is not ours to
        // interpret. Mapping the mismatch to `""` made the first unrelated
        // response look like the *root's* self-entry (whose locator is also
        // `""`), so its etag became the root's version and every later sync
        // answered `Unchanged` — pinning the collection permanently empty
        // while reporting `ready`.
        let prefix = self.href_prefix();
        let self_entry = responses.iter().find(|r| {
            let path = decode_percent(href_path(&r.href));
            match path.strip_prefix(&prefix) {
                Some(rest) => normalize_rel(rest) == normalize_rel(&dir.locator),
                None => false,
            }
        });
        let version = self_entry.and_then(|r| r.etag.clone());

        if self.capabilities().subtree_pruning
            && let (Some(known), Some(current)) = (dir.known_version.as_deref(), version.as_deref())
            && known == current
        {
            return Ok(DirListing::Unchanged);
        }

        Ok(DirListing::Listed {
            entries: self.to_entries(&dir.locator, responses),
            version,
        })
    }

    async fn fetch(&self, entry: &RemoteEntry, max_bytes: u64) -> Result<Vec<u8>, ProviderError> {
        if entry.size_bytes > max_bytes {
            return Err(ProviderError::Config(format!(
                "`{}` is {} bytes, over the {max_bytes}-byte limit for indexed files",
                entry.rel_path, entry.size_bytes
            )));
        }
        let url = self.url_for(&entry.locator);
        let resp = self
            .http
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .map_err(|source| ProviderError::Transport {
                provider: KIND,
                source,
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(self.status_error(status.as_u16(), &url, resp.text().await));
        }
        // Capped while reading, not after: a server that lied about
        // content-length (or a file that grew between listing and fetch)
        // would otherwise be fully buffered before anyone checked.
        super::read_capped(KIND, &entry.rel_path, resp, max_bytes).await
    }

    fn web_url(&self, entry: &RemoteEntry) -> Option<String> {
        let template = self.web_url_template.as_deref()?;
        // A path-keyed id is not a file id; a `{fileid}` link built from one
        // would 404. Fall back to no link rather than a broken one.
        if template.contains("{fileid}") && !self.has_extensions() {
            return None;
        }
        Some(
            template
                .replace("{base_url}", &self.base_url)
                .replace("{fileid}", &entry.id)
                .replace("{path}", &entry.rel_path),
        )
    }

    async fn probe(&self) -> Result<ProbeReport, ProviderError> {
        let url = self.url_for("");
        let xml = self.propfind(&url, "1").await?;
        let responses = parse_multistatus(&xml)?;
        self.note_extensions(responses.iter().any(|r| r.fileid.is_some()));
        let entries = self.to_entries("", responses);
        Ok(ProbeReport {
            account: Some(self.username.clone()),
            root_entries: entries.len(),
            server: self
                .has_extensions()
                .then(|| "WebDAV with ownCloud extensions".to_string()),
        })
    }
}

// ---- multistatus parsing -------------------------------------------------

/// One `<d:response>` from a PROPFIND multistatus, with only the properties
/// the indexer uses and only from propstat blocks that reported success.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DavResponse {
    pub href: String,
    pub is_collection: bool,
    pub etag: Option<String>,
    pub fileid: Option<String>,
    pub content_length: Option<u64>,
    pub content_type: Option<String>,
    pub last_modified: Option<String>,
}

/// Parse a `207 Multi-Status` body.
///
/// Namespace-prefix agnostic: servers spell the DAV namespace `d:`, `D:`,
/// `lp1:` or leave it as the default, so matching is on local names. A
/// property inside a propstat whose status is not 2xx is discarded — that is
/// how a server tells us `oc:fileid` doesn't exist, and treating it as
/// present would defeat the capability detection.
///
/// Character data is accumulated across events and applied when the element
/// closes, because the reader emits an entity reference as its own event:
/// a `getetag` of `&quot;abc&quot;` arrives as three events, and reading
/// only the first would silently truncate every quoted etag on servers that
/// escape them.
pub fn parse_multistatus(xml: &str) -> Result<Vec<DavResponse>, ProviderError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out: Vec<DavResponse> = Vec::new();
    let mut current: Option<DavResponse> = None;
    // Properties of the propstat block being read, held until its status
    // arrives (DAV puts `<status>` after `<prop>`).
    let mut pending = DavResponse::default();
    let mut in_propstat = false;
    let mut depth = 0usize;
    // Character data of the element currently open, accumulated across text
    // and entity-reference events and consumed when the element closes.
    let mut text = String::new();

    loop {
        match reader.read_event() {
            Err(e) => {
                return Err(ProviderError::Malformed(format!(
                    "XML parse error at byte {}: {e}",
                    reader.buffer_position()
                )));
            }
            Ok(Event::Eof) => {
                if depth != 0 {
                    return Err(ProviderError::Malformed(
                        "document ended with unclosed elements".into(),
                    ));
                }
                break;
            }
            Ok(Event::Start(e)) => {
                depth += 1;
                text.clear();
                match local_name(e.name().as_ref()).as_str() {
                    "response" => current = Some(DavResponse::default()),
                    "propstat" => {
                        in_propstat = true;
                        pending = DavResponse::default();
                    }
                    "collection" if in_propstat => pending.is_collection = true,
                    _ => {}
                }
            }
            // `<d:collection/>` is the usual spelling inside resourcetype.
            Ok(Event::Empty(e)) if in_propstat && local_name(e.name().as_ref()) == "collection" => {
                pending.is_collection = true;
            }
            Ok(Event::End(e)) => {
                depth = depth.saturating_sub(1);
                let name = local_name(e.name().as_ref());
                let value = text.trim().to_string();
                text.clear();
                match name.as_str() {
                    "propstat" => in_propstat = false,
                    "response" => {
                        if let Some(r) = current.take()
                            && !r.href.is_empty()
                        {
                            out.push(r);
                        }
                    }
                    _ => {
                        if value.is_empty() {
                            continue;
                        }
                        let Some(resp) = current.as_mut() else {
                            continue;
                        };
                        match name.as_str() {
                            "href" if !in_propstat => resp.href = value,
                            "status" if in_propstat => {
                                if is_success_status(&value) {
                                    merge(resp, std::mem::take(&mut pending));
                                } else {
                                    pending = DavResponse::default();
                                }
                            }
                            "getetag" if in_propstat => pending.etag = Some(strip_quotes(&value)),
                            "fileid" if in_propstat => pending.fileid = Some(value),
                            "getcontentlength" if in_propstat => {
                                pending.content_length = value.parse().ok();
                            }
                            "getcontenttype" if in_propstat => {
                                pending.content_type = Some(
                                    value.split(';').next().unwrap_or(&value).trim().to_string(),
                                );
                            }
                            "getlastmodified" if in_propstat => pending.last_modified = Some(value),
                            _ => {}
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                text.push_str(&t.xml10_content().unwrap_or_default());
            }
            Ok(Event::GeneralRef(r)) => {
                text.push_str(&resolve_entity(&r));
            }
            Ok(Event::CData(c)) => {
                text.push_str(&c.decode().unwrap_or_default());
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Resolve an entity reference to its text. Handles the five predefined XML
/// entities and numeric character references; anything else (a DTD-defined
/// entity, which no DAV server sends) contributes nothing rather than
/// leaking `&name;` into a filename.
fn resolve_entity(r: &quick_xml::events::BytesRef<'_>) -> String {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return c.to_string();
    }
    let name = r.decode().unwrap_or_default();
    match name.as_ref() {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        _ => String::new(),
    }
}

/// Fold a successful propstat's properties into the response.
fn merge(into: &mut DavResponse, from: DavResponse) {
    if from.is_collection {
        into.is_collection = true;
    }
    if from.etag.is_some() {
        into.etag = from.etag;
    }
    if from.fileid.is_some() {
        into.fileid = from.fileid;
    }
    if from.content_length.is_some() {
        into.content_length = from.content_length;
    }
    if from.content_type.is_some() {
        into.content_type = from.content_type;
    }
    if from.last_modified.is_some() {
        into.last_modified = from.last_modified;
    }
}

/// `HTTP/1.1 200 OK` → true. Anything non-2xx (typically `404 Not Found`
/// for properties the server doesn't implement) → false.
fn is_success_status(status_line: &str) -> bool {
    status_line
        .split_whitespace()
        .find_map(|tok| tok.parse::<u16>().ok())
        .is_some_and(|code| (200..300).contains(&code))
}

fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_ascii_lowercase(),
        None => s.to_ascii_lowercase(),
    }
}

fn strip_quotes(s: &str) -> String {
    s.trim_start_matches("W/").trim_matches('"').to_string()
}

/// Path component of an href, which servers return either absolute
/// (`/remote.php/dav/...`) or as a full URL.
fn href_path(href: &str) -> &str {
    if let Some(rest) = href.split_once("://") {
        match rest.1.find('/') {
            Some(idx) => &rest.1[idx..],
            None => "/",
        }
    } else {
        href
    }
}

/// Percent-decode a path. Hand-rolled rather than pulling a crate: hrefs are
/// the only place we need it and the rule is three lines of hex.
fn decode_percent(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a path for a request URL, leaving separators alone.
/// Deliberately conservative: anything outside the unreserved set plus `/`
/// is escaped, so spaces, umlauts and `#` in filenames all survive.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Trim leading/trailing slashes and collapse the empty case, so `""`,
/// `"/"` and `"/a/b/"` normalise to `""` and `"a/b"`.
/// A change token for an entry the server gave no change token for.
///
/// Deliberately different on every walk: `sync::plan` compares versions for
/// equality, so any *stable* stand-in (the filename, say) declares the file
/// unchanged forever. Re-reading it each sync is the honest cost of a server
/// that reports neither an etag nor a modification time. The size is folded
/// in only so the value is informative in a log.
fn weak_version(content_length: Option<u64>) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "weak:{}:{}",
        content_length.unwrap_or(0),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn normalize_rel(s: &str) -> String {
    s.trim_matches('/').to_string()
}

/// `Tue, 04 Nov 2025 10:00:00 GMT` → a timestamp.
///
/// The weekday is parsed leniently on purpose. It is redundant with the date
/// and servers do get it wrong; refusing the whole value over a disagreeing
/// day name would throw away a timestamp we can read perfectly well.
/// Anything genuinely unparseable costs a sort key, not the file.
fn parse_http_date(raw: &str) -> Option<Timestamp> {
    static PARSER: jiff::fmt::rfc2822::DateTimeParser =
        jiff::fmt::rfc2822::DateTimeParser::new().relaxed_weekday(true);
    PARSER.parse_timestamp(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const NEXTCLOUD_LISTING: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/svc/Finance/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:getetag>&quot;65f0a1b2c3d4e&quot;</d:getetag>
        <oc:fileid>1234</oc:fileid>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
    <d:propstat>
      <d:prop><d:getcontentlength/><d:getcontenttype/></d:prop>
      <d:status>HTTP/1.1 404 Not Found</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/svc/Finance/Rechnung%20M%C3%BCller.pdf</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype/>
        <d:getetag>"abc123"</d:getetag>
        <d:getcontentlength>84213</d:getcontentlength>
        <d:getcontenttype>application/pdf</d:getcontenttype>
        <d:getlastmodified>Tue, 04 Nov 2025 10:00:00 GMT</d:getlastmodified>
        <oc:fileid>5678</oc:fileid>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/svc/Finance/2025/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:getetag>"dir2025"</d:getetag>
        <oc:fileid>9012</oc:fileid>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    /// A server with no ownCloud extensions, uppercase prefix, full-URL
    /// hrefs — all three shapes seen in the wild.
    const GENERIC_LISTING: &str = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>https://dav.example.com/dav/docs/</D:href>
    <D:propstat>
      <D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>https://dav.example.com/dav/docs/report.txt</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getetag>W/"v7"</D:getetag>
        <D:getcontentlength>12</D:getcontentlength>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

    fn provider(pairs: &[(&str, &str)]) -> WebdavProvider {
        let mut values: BTreeMap<String, String> = [
            ("base_url", "https://cloud.example.com"),
            ("username", "svc"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in pairs {
            values.insert(k.to_string(), v.to_string());
        }
        let secrets = [("password".to_string(), "app-pw".to_string())]
            .into_iter()
            .collect();
        WebdavProvider::from_config(
            &ProviderConfig::new(values, secrets),
            reqwest::Client::new(),
        )
        .expect("valid config")
    }

    #[test]
    fn parses_nextcloud_listing_with_extensions() {
        let rs = parse_multistatus(NEXTCLOUD_LISTING).unwrap();
        assert_eq!(rs.len(), 3);
        let file = &rs[1];
        assert_eq!(file.fileid.as_deref(), Some("5678"));
        assert_eq!(file.etag.as_deref(), Some("abc123"), "quotes are stripped");
        assert_eq!(file.content_length, Some(84213));
        assert_eq!(file.content_type.as_deref(), Some("application/pdf"));
        assert!(!file.is_collection);
        assert!(rs[0].is_collection);
    }

    #[test]
    fn a_404_propstat_does_not_contribute_properties() {
        let rs = parse_multistatus(NEXTCLOUD_LISTING).unwrap();
        // The directory's second propstat is a 404 for length/type. If it
        // leaked through, capability detection and size limits would both
        // read garbage.
        assert_eq!(rs[0].content_length, None);
        assert_eq!(rs[0].content_type, None);
    }

    #[test]
    fn parses_uppercase_prefix_and_full_url_hrefs() {
        let rs = parse_multistatus(GENERIC_LISTING).unwrap();
        assert_eq!(rs.len(), 2);
        assert!(rs[0].is_collection);
        assert_eq!(
            rs[1].etag.as_deref(),
            Some("v7"),
            "weak etag prefix stripped"
        );
        assert!(rs[1].fileid.is_none());
    }

    #[test]
    fn malformed_xml_is_an_error_not_a_silent_empty_listing() {
        let err = parse_multistatus("<d:multistatus><d:response>").unwrap_err();
        assert!(matches!(err, ProviderError::Malformed(_)), "{err}");
    }

    #[test]
    fn entries_are_relative_to_the_root_and_exclude_the_directory_itself() {
        let p = provider(&[("root", "/Finance")]);
        p.note_extensions(true);
        let entries = p.to_entries("", parse_multistatus(NEXTCLOUD_LISTING).unwrap());
        let paths: Vec<&str> = entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert_eq!(paths, vec!["2025", "Rechnung Müller.pdf"]);
    }

    #[test]
    fn percent_encoded_umlauts_round_trip() {
        let p = provider(&[("root", "/Finance")]);
        let entries = p.to_entries("", parse_multistatus(NEXTCLOUD_LISTING).unwrap());
        let pdf = entries
            .iter()
            .find(|e| e.rel_path.ends_with(".pdf"))
            .expect("the pdf is listed");
        assert_eq!(pdf.rel_path, "Rechnung Müller.pdf");
        assert_eq!(
            p.url_for(&pdf.locator),
            "https://cloud.example.com/remote.php/dav/files/svc/Finance/Rechnung%20M%C3%BCller.pdf",
            "the fetch URL re-encodes what the href decoded"
        );
    }

    #[test]
    fn identity_is_the_fileid_when_the_server_offers_one() {
        let p = provider(&[("root", "/Finance")]);
        let entries = p.to_entries("", parse_multistatus(NEXTCLOUD_LISTING).unwrap());
        let pdf = entries
            .iter()
            .find(|e| e.rel_path.ends_with(".pdf"))
            .unwrap();
        assert_eq!(pdf.id, "5678", "a move must not change identity");
    }

    #[test]
    fn identity_falls_back_to_the_path_without_extensions() {
        let p = provider(&[
            ("dav_path", "/dav"),
            ("root", "/docs"),
            ("base_url", "https://dav.example.com"),
        ]);
        let entries = p.to_entries("", parse_multistatus(GENERIC_LISTING).unwrap());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "report.txt");
    }

    /// `dav_path = "/"` is the documented value for a plain WebDAV server and
    /// must produce no path prefix at all.
    ///
    /// Regression: it rendered as `"/"`, so every URL became
    /// `https://host//docs/...` and `href_prefix()` became `"//docs"` — which
    /// matches no href any server returns. The collection indexed zero files
    /// and logged nothing.
    #[test]
    fn a_root_dav_path_adds_no_prefix() {
        let p = provider(&[
            ("dav_path", "/"),
            ("root", "docs"),
            ("base_url", "https://dav.example.com"),
        ]);
        assert_eq!(p.url_for(""), "https://dav.example.com/docs");
        assert_eq!(p.url_for("a.txt"), "https://dav.example.com/docs/a.txt");
        assert_eq!(
            p.href_prefix(),
            "/docs",
            "the prefix must be what the server's own hrefs start with"
        );
    }

    /// An href outside our prefix must never be mistaken for the directory
    /// being listed.
    ///
    /// Regression: a failed `strip_prefix` mapped to `""`, and the root's
    /// locator is also `""` — so the first unrelated response was accepted as
    /// the root's self-entry and its etag stored as the root's version. Every
    /// later sync then answered `Unchanged`, pinning the collection
    /// permanently empty while reporting `ready`.
    #[test]
    fn an_href_outside_our_prefix_is_not_the_root() {
        let p = provider(&[
            ("dav_path", "/remote.php/dav/files/svc"),
            ("root", "docs"),
            ("base_url", "https://dav.example.com"),
        ]);
        let responses = parse_multistatus(FOREIGN_PREFIX_LISTING).unwrap();
        let prefix = p.href_prefix();
        let matched = responses.iter().any(|r| {
            let path = decode_percent(href_path(&r.href));
            match path.strip_prefix(&prefix) {
                Some(rest) => normalize_rel(rest) == normalize_rel(""),
                None => false,
            }
        });
        assert!(
            !matched,
            "nothing in a listing from a different prefix is our root"
        );
    }

    /// A file with no etag and no modification time must not be pinned as
    /// never-changing.
    ///
    /// Regression: its filename stood in as the change token, and versions
    /// are compared for equality — so the file was indexed once and every
    /// later edit was invisible, permanently and silently.
    #[test]
    fn a_file_with_no_change_token_is_re_read_rather_than_pinned() {
        let p = provider(&[
            ("dav_path", "/dav"),
            ("root", ""),
            ("base_url", "https://dav.example.com"),
        ]);
        let first = p.to_entries("", parse_multistatus(NO_TOKEN_LISTING).unwrap());
        let second = p.to_entries("", parse_multistatus(NO_TOKEN_LISTING).unwrap());
        assert_eq!(first.len(), 1);
        assert_ne!(
            first[0].version, second[0].version,
            "with nothing to compare, the honest answer is `changed` — a \
             stable stand-in would silently drop every future edit"
        );
        assert_ne!(
            first[0].version, first[0].rel_path,
            "and it is never the filename"
        );
    }

    /// A listing whose hrefs sit under a completely different DAV prefix.
    const FOREIGN_PREFIX_LISTING: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/some/other/tree/</d:href>
    <d:propstat><d:prop><d:getetag>"aaa"</d:getetag>
      <d:resourcetype><d:collection/></d:resourcetype>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;

    /// A server that reports neither `getetag` nor `getlastmodified`.
    const NO_TOKEN_LISTING: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/</d:href>
    <d:propstat><d:prop>
      <d:resourcetype><d:collection/></d:resourcetype>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/notes.txt</d:href>
    <d:propstat><d:prop>
      <d:getcontentlength>12</d:getcontentlength>
      <d:resourcetype/>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn capabilities_follow_detection_not_configuration() {
        let p = provider(&[]);
        assert!(
            !p.capabilities().subtree_pruning,
            "nothing is claimed before the first response is seen"
        );
        p.note_extensions(true);
        let caps = p.capabilities();
        assert!(caps.subtree_pruning);
        assert!(caps.stable_ids);
    }

    #[test]
    fn a_generic_server_never_claims_pruning_or_stable_ids() {
        let p = provider(&[]);
        p.note_extensions(false);
        let caps = p.capabilities();
        assert!(
            !caps.subtree_pruning,
            "RFC 4918 does not promise propagating collection etags"
        );
        assert!(!caps.stable_ids);
    }

    #[test]
    fn detection_latches_so_one_odd_response_cannot_flip_it() {
        let p = provider(&[]);
        p.note_extensions(true);
        p.note_extensions(false);
        assert!(p.capabilities().stable_ids);
    }

    #[test]
    fn web_url_uses_the_file_id_and_is_withheld_when_there_is_none() {
        let p = provider(&[]);
        let entry = RemoteEntry {
            id: "5678".into(),
            locator: "a.pdf".into(),
            rel_path: "a.pdf".into(),
            kind: EntryKind::File,
            version: "v".into(),
            size_bytes: 1,
            mime: None,
            modified_at: None,
        };
        p.note_extensions(false);
        assert_eq!(
            p.web_url(&entry),
            None,
            "a {{fileid}} link built from a path id would 404"
        );

        let p2 = provider(&[]);
        p2.note_extensions(true);
        assert_eq!(
            p2.web_url(&entry).as_deref(),
            Some("https://cloud.example.com/f/5678")
        );
    }

    #[test]
    fn dav_path_template_substitutes_the_account() {
        let p = provider(&[]);
        assert_eq!(
            p.url_for(""),
            "https://cloud.example.com/remote.php/dav/files/svc"
        );
    }

    #[test]
    fn a_plain_webdav_server_can_be_pointed_at_any_dav_root() {
        let p = provider(&[("dav_path", "/dav"), ("root", "docs")]);
        assert_eq!(
            p.url_for("a/b.txt"),
            "https://cloud.example.com/dav/docs/a/b.txt"
        );
    }

    #[test]
    fn a_url_without_a_scheme_is_rejected_with_an_actionable_message() {
        let values = [
            ("base_url".to_string(), "cloud.example.com".to_string()),
            ("username".to_string(), "svc".to_string()),
        ]
        .into_iter()
        .collect();
        let secrets = [("password".to_string(), "pw".to_string())]
            .into_iter()
            .collect();
        let err = WebdavProvider::from_config(
            &ProviderConfig::new(values, secrets),
            reqwest::Client::new(),
        )
        .map(|_| ())
        .expect_err("a schemeless URL is rejected");
        assert!(err.to_string().contains("https://"), "{err}");
    }

    #[test]
    fn http_dates_become_timestamps() {
        let ts = parse_http_date("Tue, 04 Nov 2025 10:00:00 GMT").expect("RFC 1123 parses");
        assert_eq!(ts.to_string(), "2025-11-04T10:00:00Z");
        assert!(parse_http_date("not a date").is_none());
    }

    #[test]
    fn a_wrong_weekday_does_not_cost_us_the_timestamp() {
        // 2025-11-04 is a Tuesday. Servers do send the wrong day name, and
        // the date is unambiguous without it.
        let ts = parse_http_date("Wed, 04 Nov 2025 10:00:00 GMT")
            .expect("a disagreeing weekday is not a reason to drop the date");
        assert_eq!(ts.to_string(), "2025-11-04T10:00:00Z");
    }
}
