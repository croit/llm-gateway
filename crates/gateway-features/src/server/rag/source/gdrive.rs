// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Google Drive as a [`FileProvider`].
//!
//! Three things make Drive unlike the WebDAV lineage, and each one is why a
//! piece of this file exists:
//!
//!   * **Consent, not a password.** There is no credential an operator can
//!     type that grants read access. A person clicks through Google's consent
//!     screen once; the gateway keeps the refresh token and trades it for a
//!     one-hour access token whenever it needs one. That is
//!     [`AuthKind::OAuth2`] plus [`GoogleDriveProvider::access_token`].
//!
//!   * **Native documents have no bytes.** A Google Doc is not a file you can
//!     download — `alt=media` on one is an error. It has to be *exported*,
//!     and the export format decides how much of the document survives. We
//!     ask for the Office formats (docx/xlsx/pptx) because the extraction
//!     ladder already reads those through the sandbox, tables and all, and a
//!     table is usually the part of an invoice or a status report worth
//!     answering from. See [`export_as`].
//!
//!   * **Names are not identity, and are not even unique.** Drive lets two
//!     files in one folder share a name, and lets a name contain `/`. The
//!     stable `id` is the identity ([`ProviderCapabilities::stable_ids`]);
//!     `rel_path` is a display and dedup key we have to *make* unique. See
//!     [`disambiguate`].
//!
//! Not supported yet, and deliberately: `changes.list`. Drive has a real
//! change feed and it is the right long-term answer for a large corpus, but
//! the worker has no delta consumer, so this provider reports
//! `delta: false` and re-walks. The walk is metadata-only and `sync::plan`
//! still skips fetch/extract/embed for every file whose `version` is
//! unchanged, so a re-sync costs listings rather than documents.

use std::collections::BTreeMap;
use std::sync::Arc;

use jiff::Timestamp;
use serde::Deserialize;

use super::{
    AuthKind, ConfigField, DirListing, DirRef, EntryKind, FieldKind, FileProvider, ProbeReport,
    ProviderCapabilities, ProviderConfig, ProviderError, ProviderFactory, REFRESH_TOKEN_KEY,
    RemoteEntry,
};

pub const KIND: &str = "gdrive";

const API: &str = "https://www.googleapis.com/drive/v3";
const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Read-only across the whole Drive. Narrower scopes exist (`drive.file`
/// only sees files the app itself created) but none of them can index a
/// corpus the operator already has.
const SCOPES: &[&str] = &["https://www.googleapis.com/auth/drive.readonly"];

/// Google's folder marker. Everything in Drive is a "file"; this mime is the
/// only thing that makes one a directory.
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// Fields to ask for on every listing. Drive returns almost nothing unless
/// asked, and a missing `version` would make every file look changed.
const LIST_FIELDS: &str = "nextPageToken,files(id,name,mimeType,size,modifiedTime,version)";

static FIELDS: &[ConfigField] = &[
    ConfigField {
        key: "client_id",
        label: "OAuth client ID",
        help: "From a Google Cloud project with the Drive API enabled. Create an \
               OAuth 2.0 Client ID of type 'Web application'.",
        kind: FieldKind::Text,
        required: true,
        default: None,
    },
    ConfigField {
        key: "client_secret",
        label: "OAuth client secret",
        help: "Issued alongside the client ID.",
        kind: FieldKind::Secret,
        required: true,
        default: None,
    },
    ConfigField {
        key: "root_folder_id",
        label: "Folder ID to index",
        help: "The folder's id from its Drive URL (the part after /folders/). \
               Leave as 'root' for the whole of My Drive. A shared drive works \
               here too — paste the drive's own id.",
        kind: FieldKind::Text,
        required: false,
        default: Some("root"),
    },
];

pub struct GoogleDriveFactory;

impl ProviderFactory for GoogleDriveFactory {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn label(&self) -> &'static str {
        "Google Drive"
    }

    fn description(&self) -> &'static str {
        "Indexes a Google Drive folder. Google Docs, Sheets and Slides are \
         exported to Office formats so their tables and structure survive; \
         reading those needs the document sandbox enabled."
    }

    fn config_fields(&self) -> &'static [ConfigField] {
        FIELDS
    }

    fn auth(&self) -> AuthKind {
        AuthKind::OAuth2 {
            authorize_url: AUTHORIZE_URL,
            token_url: TOKEN_URL,
            scopes: SCOPES,
            client_id_key: "client_id",
            client_secret_key: "client_secret",
        }
    }

    fn build(
        &self,
        cfg: &ProviderConfig,
        http: reqwest::Client,
    ) -> Result<Arc<dyn FileProvider>, ProviderError> {
        let cfg = cfg.with_defaults(FIELDS);
        Ok(Arc::new(GoogleDriveProvider {
            client_id: cfg.require("client_id")?.to_string(),
            client_secret: cfg
                .secret("client_secret")
                .ok_or_else(|| {
                    ProviderError::Config("`OAuth client secret` is required".to_string())
                })?
                .to_string(),
            refresh_token: cfg
                .secret(REFRESH_TOKEN_KEY)
                .ok_or_else(|| {
                    ProviderError::Config(
                        "this collection is not connected to Google yet — use Connect \
                         to grant access"
                            .to_string(),
                    )
                })?
                .to_string(),
            root_folder_id: cfg.get("root_folder_id").unwrap_or("root").to_string(),
            http,
            access: tokio::sync::Mutex::new(None),
        }) as Arc<dyn FileProvider>)
    }
}

/// An access token and when it stops being one.
struct CachedToken {
    token: String,
    expires_at: Option<Timestamp>,
}

pub struct GoogleDriveProvider {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    root_folder_id: String,
    http: reqwest::Client,
    /// Access tokens last an hour; an index build can last longer. Cached
    /// behind a mutex so a concurrent walk refreshes once, not once per
    /// listing task.
    access: tokio::sync::Mutex<Option<CachedToken>>,
}

impl GoogleDriveProvider {
    /// A usable access token, refreshing when the cached one is gone or
    /// nearly so.
    ///
    /// The minute of headroom is not superstition: a token that passes the
    /// check and then expires mid-flight turns into a 401 the walker reports
    /// as a failed directory, which makes the whole pass non-authoritative
    /// and blocks pruning.
    async fn access_token(&self) -> Result<String, ProviderError> {
        let mut guard = self.access.lock().await;
        if let Some(cached) = guard.as_ref()
            && cached
                .expires_at
                .is_none_or(|at| at > Timestamp::now() + jiff::Span::new().seconds(60))
        {
            return Ok(cached.token.clone());
        }
        let tokens = gateway_core::server::auth::mcp_oauth::refresh(
            &self.http,
            TOKEN_URL,
            &self.refresh_token,
            &self.client_id,
            Some(&self.client_secret),
        )
        .await
        .map_err(|_| ProviderError::Unauthorized {
            provider: KIND,
            status: 401,
            hint: "Google refused the stored refresh token. Someone may have revoked \
                   the gateway's access, or the client secret was rotated — reconnect \
                   the collection.",
        })
        .inspect_err(|_| tracing::warn!(provider = KIND, "refreshing the access token failed"))?;
        let token = tokens.access_token.clone();
        *guard = Some(CachedToken {
            token: token.clone(),
            expires_at: tokens.expires_at,
        });
        Ok(token)
    }

    async fn get(&self, url: reqwest::Url) -> Result<reqwest::Response, ProviderError> {
        let token = self.access_token().await?;
        let resp = self
            .http
            .get(url.clone())
            .bearer_auth(token)
            .send()
            .await
            .map_err(|source| ProviderError::Transport {
                provider: KIND,
                source,
            })?;
        let status = resp.status().as_u16();
        match status {
            200..=299 => Ok(resp),
            401 => Err(ProviderError::Unauthorized {
                provider: KIND,
                status,
                hint: "Google rejected the access token. Reconnect the collection.",
            }),
            403 => Err(ProviderError::Forbidden {
                provider: KIND,
                path: url.to_string(),
                status,
                hint: "Check that the Drive API is enabled on the Google Cloud project, \
                       that the consent granted the read-only Drive scope, and that the \
                       connected account can see this folder.",
            }),
            404 => Err(ProviderError::NotFound {
                provider: KIND,
                path: url.to_string(),
                hint: "The folder id may be wrong, or the connected account cannot see it.",
            }),
            _ => {
                let body = resp.text().await.unwrap_or_default();
                Err(ProviderError::Status {
                    provider: KIND,
                    status,
                    body: body.chars().take(500).collect(),
                })
            }
        }
    }

    /// One page of `files.list` for a folder.
    async fn list_page(
        &self,
        folder_id: &str,
        page_token: Option<&str>,
    ) -> Result<FileList, ProviderError> {
        let q = format!("'{}' in parents and trashed = false", escape_q(folder_id));
        let mut url = reqwest::Url::parse(&format!("{API}/files"))
            .map_err(|e| ProviderError::Config(e.to_string()))?;
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("q", &q)
                .append_pair("fields", LIST_FIELDS)
                .append_pair("pageSize", "1000")
                // Without both of these a shared drive lists as empty rather
                // than as an error, which is the worst possible failure: a
                // corpus that indexes cleanly and contains nothing.
                .append_pair("supportsAllDrives", "true")
                .append_pair("includeItemsFromAllDrives", "true")
                .append_pair("orderBy", "name");
            if let Some(t) = page_token {
                qp.append_pair("pageToken", t);
            }
        }
        let resp = self.get(url).await?;
        resp.json::<FileList>()
            .await
            .map_err(|e| ProviderError::Malformed(format!("listing was not valid JSON: {e}")))
    }
}

#[derive(Debug, Deserialize)]
struct FileList {
    #[serde(default)]
    files: Vec<DriveFile>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct DriveFile {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "mimeType")]
    mime_type: String,
    /// Absent for native Google files, and a decimal string when present.
    #[serde(default)]
    size: Option<String>,
    #[serde(default, rename = "modifiedTime")]
    modified_time: Option<String>,
    /// Monotonic per-file counter. The change token: it moves on any edit,
    /// including ones that leave `modifiedTime` alone.
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AboutResponse {
    #[serde(default)]
    user: Option<AboutUser>,
}

#[derive(Debug, Deserialize)]
struct AboutUser {
    #[serde(default, rename = "emailAddress")]
    email_address: Option<String>,
}

/// The export target for a native Google type, or `None` when the type has no
/// document in it worth indexing (Forms, Sites, shortcuts, Maps).
///
/// Office formats rather than `text/plain` on purpose: the extraction ladder
/// reads docx/xlsx/pptx through the sandbox and keeps tables, headings and
/// speaker notes, which is most of what makes a spreadsheet or a deck
/// answerable. A plain-text export flattens a budget into a wall of numbers.
fn export_target(mime: &str) -> Option<ExportTarget> {
    Some(match mime {
        "application/vnd.google-apps.document" => ExportTarget {
            mime: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            extension: "docx",
        },
        "application/vnd.google-apps.spreadsheet" => ExportTarget {
            mime: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            extension: "xlsx",
        },
        "application/vnd.google-apps.presentation" => ExportTarget {
            mime: "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            extension: "pptx",
        },
        // A drawing is an image, and the OCR rung of the ladder reads images.
        "application/vnd.google-apps.drawing" => ExportTarget {
            mime: "image/png",
            extension: "png",
        },
        _ => return None,
    })
}

/// What a native Google file turns into once exported.
///
/// One table rather than a mime match and a separate extension match: the two
/// must agree, and a mismatch is silent — the bytes arrive in one format
/// wearing another's extension and the ladder reads them with the wrong
/// parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExportTarget {
    /// Content type of the exported bytes. This is what goes on the
    /// `RemoteEntry`, because every consumer downstream — `classify`, and the
    /// OCR sidecar's multipart `Content-Type` — wants the type of the bytes
    /// it is about to read, not the type of the thing in Drive.
    mime: &'static str,
    /// Extension appended to `rel_path`, because `extract::classify` is
    /// extension-first and a Google Doc's name usually has none.
    extension: &'static str,
}

fn is_native_google(mime: &str) -> bool {
    mime.starts_with("application/vnd.google-apps.")
}

/// Quote a value for a Drive `q` expression.
///
/// Folder ids are opaque Google strings and have never contained a quote, but
/// this string is concatenated into a query language — the escape costs
/// nothing and removes the question.
fn escape_q(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Make a Drive name safe to use as one path segment.
///
/// Drive allows `/` in a name; the rest of the indexer treats `/` as the
/// path separator, so leaving it in would invent directories that do not
/// exist and break the pruning prefix match.
fn sanitize_segment(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' => '_',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `report.pdf` + `aaa` -> `report ~aaa.pdf`.
///
/// Not needed because Drive lacks a stable id — [`RemoteEntry::id`] is one,
/// and it is what `sync::plan` keys on. It is needed because the *store* is
/// still keyed on the path: `rag_files` is `UNIQUE (collection_id, path)` and
/// `upsert_file` conflicts on it. Every earlier provider made that safe by
/// construction (no filesystem or WebDAV folder holds two files with one
/// name); Drive is the first that can, so without this the second file
/// silently overwrites the first's row and the corpus quietly loses a
/// document.
fn suffixed(name: &str, id: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ~{id}.{ext}"),
        _ => format!("{name} ~{id}"),
    }
}

/// Turn one directory's worth of Drive metadata into entries.
///
/// Takes the *whole* directory, never a single page: name collisions are
/// resolved by counting, so a pair split across a `files.list` page boundary
/// would otherwise be counted as unique twice and collide on `rel_path`
/// anyway — defeating the one thing this is here for.
///
/// Every member of a colliding group is suffixed, not just the later ones,
/// because this keys off a *count* rather than a position: a path that
/// depended on ordering would shuffle whenever Drive returned the group in a
/// different order. Paths do still change when a group stops colliding, but
/// with a stable id that is a `rename` — one column update, no re-extraction.
fn to_entries(parent_rel: &str, files: Vec<DriveFile>) -> Vec<RemoteEntry> {
    // Decide each file's name *including* any export extension, then count
    // those. Counting the bare name would miss the collision that only
    // appears after exporting: a Google Doc called `report` and an uploaded
    // `report.docx` are distinct names that both become `report.docx`.
    let named: Vec<(DriveFile, Option<ExportTarget>, String)> = files
        .into_iter()
        .filter_map(|f| {
            let is_dir = f.mime_type == FOLDER_MIME;
            let export = export_target(&f.mime_type);
            // A native type with no document in it is not an unreadable
            // file to report, it is not a file at all.
            if !is_dir && is_native_google(&f.mime_type) && export.is_none() {
                return None;
            }
            let mut display = sanitize_segment(&f.name);
            if let Some(target) = export {
                display = format!("{display}.{}", target.extension);
            }
            Some((f, export, display))
        })
        .collect();

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, _, display) in &named {
        *counts.entry(display.as_str()).or_default() += 1;
    }
    let colliding: std::collections::HashSet<&str> = counts
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(name, _)| *name)
        .collect();
    let colliding: std::collections::HashSet<String> =
        colliding.into_iter().map(str::to_string).collect();

    named
        .into_iter()
        .map(|(f, export, display)| {
            let is_dir = f.mime_type == FOLDER_MIME;
            // `suffixed` inserts before the extension, so an exported name
            // still reads as `report ~<id>.docx`.
            let segment = if colliding.contains(&display) {
                suffixed(&display, &f.id)
            } else {
                display
            };
            let rel_path = if parent_rel.is_empty() {
                segment
            } else {
                format!("{parent_rel}/{segment}")
            };

            RemoteEntry {
                id: f.id.clone(),
                // Carries the export decision, so `fetch` can act on it while
                // `mime` stays free to describe the bytes that come back.
                // Provider-private and never shown to the model.
                locator: match export {
                    Some(t) => format!("export:{}:{}", t.mime, f.id),
                    None => f.id.clone(),
                },
                rel_path,
                kind: if is_dir {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                // `version` is always present in practice; falling back to
                // the id would pin the file as never-changing, so fall back
                // to the modified time and only then to the id.
                version: f
                    .version
                    .clone()
                    .or_else(|| f.modified_time.clone())
                    .unwrap_or_else(|| f.id.clone()),
                size_bytes: f.size.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0),
                // The type of the bytes `fetch` returns — the exported type
                // for a native file, not the `application/vnd.google-apps.*`
                // that has no bytes. `extract` hands this to the OCR sidecar
                // as a Content-Type, so a native mime here is a lie that
                // leaves the provider.
                mime: match (is_dir, export) {
                    (true, _) => None,
                    (false, Some(t)) => Some(t.mime.to_string()),
                    (false, None) => Some(f.mime_type.clone()),
                },
                modified_at: f
                    .modified_time
                    .as_deref()
                    .and_then(|t| t.parse::<Timestamp>().ok()),
            }
        })
        .collect()
}

/// Split a locator back into "export as this" plus the Drive id.
fn parse_locator(locator: &str) -> (Option<&str>, &str) {
    match locator.strip_prefix("export:") {
        // A mime type never contains `:`, so the first one ends it.
        Some(rest) => match rest.split_once(':') {
            Some((mime, id)) => (Some(mime), id),
            None => (None, rest),
        },
        None => (None, locator),
    }
}

#[async_trait::async_trait]
impl FileProvider for GoogleDriveProvider {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            // Drive folders carry no version that moves when a descendant
            // changes, so there is nothing to compare and no subtree to skip.
            // `changes.list` is the Drive answer to this and needs a delta
            // consumer in the worker first.
            subtree_pruning: false,
            delta: false,
            // A Drive id survives rename, move and re-upload.
            stable_ids: true,
        }
    }

    fn root(&self) -> DirRef {
        DirRef::root(self.root_folder_id.clone())
    }

    async fn list_dir(&self, dir: &DirRef) -> Result<DirListing, ProviderError> {
        // Every page first, then name them once: `to_entries` resolves name
        // collisions by counting, so feeding it one page at a time would miss
        // a colliding pair that straddles a page boundary.
        let mut files = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let page = self.list_page(&dir.locator, page_token.as_deref()).await?;
            files.extend(page.files);
            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }
        // No folder version to report: see `capabilities`.
        Ok(DirListing::Listed {
            entries: to_entries(&dir.rel_path, files),
            version: None,
        })
    }

    async fn fetch(&self, entry: &RemoteEntry, max_bytes: u64) -> Result<Vec<u8>, ProviderError> {
        if entry.size_bytes > max_bytes {
            return Err(ProviderError::Config(format!(
                "`{}` is {} bytes, over the {max_bytes}-byte limit for indexed files",
                entry.rel_path, entry.size_bytes
            )));
        }
        let (export, id) = parse_locator(&entry.locator);
        let mut url = reqwest::Url::parse(&match export {
            // A native Google file has no bytes to download; `export`
            // renders it into a format the extraction ladder can read.
            Some(_) => format!("{API}/files/{id}/export"),
            None => format!("{API}/files/{id}"),
        })
        .map_err(|e| ProviderError::Config(e.to_string()))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("supportsAllDrives", "true");
            match export {
                Some(mime) => q.append_pair("mimeType", mime),
                None => q.append_pair("alt", "media"),
            };
        }
        let resp = self.get(url).await?;
        // A native export declares no size at all, so the pre-check above
        // never fires for it — this is the only bound that path has, and it
        // has to hold while reading rather than after.
        super::read_capped(KIND, &entry.rel_path, resp, max_bytes).await
    }

    fn web_url(&self, entry: &RemoteEntry) -> Option<String> {
        // Drive redirects this to whichever editor or viewer owns the file,
        // so one form works for a Doc, a Sheet and a PDF alike.
        Some(format!("https://drive.google.com/open?id={}", entry.id))
    }

    async fn probe(&self) -> Result<ProbeReport, ProviderError> {
        let about_url = reqwest::Url::parse(&format!("{API}/about?fields=user(emailAddress)"))
            .map_err(|e| ProviderError::Config(e.to_string()))?;
        let about = self
            .get(about_url)
            .await?
            .json::<AboutResponse>()
            .await
            .map_err(|e| ProviderError::Malformed(format!("about was not valid JSON: {e}")))?;
        let page = self.list_page(&self.root_folder_id, None).await?;
        Ok(ProbeReport {
            account: about.user.and_then(|u| u.email_address),
            root_entries: page.files.len(),
            server: Some("Google Drive".to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, name: &str, mime: &str) -> DriveFile {
        DriveFile {
            id: id.to_string(),
            name: name.to_string(),
            mime_type: mime.to_string(),
            size: Some("100".to_string()),
            modified_time: Some("2026-01-01T00:00:00.000Z".to_string()),
            version: Some("7".to_string()),
        }
    }

    #[test]
    fn a_google_doc_is_exported_as_docx_so_the_office_rung_can_read_it() {
        let entries = to_entries("", vec![file("d1", "Q3 Report", DOC_MIME)]);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].rel_path, "Q3 Report.docx",
            "the export extension is on the path, because the extraction \
             ladder dispatches on extension and a Doc's name has none"
        );
    }

    #[test]
    fn each_native_type_maps_to_the_format_that_keeps_its_structure() {
        assert_eq!(export_target(DOC_MIME).unwrap().extension, "docx");
        assert_eq!(export_target(SHEET_MIME).unwrap().extension, "xlsx");
        assert_eq!(export_target(SLIDES_MIME).unwrap().extension, "pptx");
        // A drawing is an image; the OCR rung reads it.
        let drawing = export_target("application/vnd.google-apps.drawing").unwrap();
        assert_eq!(drawing.extension, "png");
        assert_eq!(drawing.mime, "image/png");
    }

    /// `RemoteEntry::mime` must describe the bytes `fetch` returns, not the
    /// Drive object. `extract` passes it to the OCR sidecar as the multipart
    /// `Content-Type`, so an exported drawing labelled
    /// `application/vnd.google-apps.drawing` sends PNG bytes under a content
    /// type no OCR backend can accept.
    #[test]
    fn an_exported_file_reports_the_type_of_the_bytes_that_come_back() {
        let entries = to_entries(
            "",
            vec![file("d1", "Sketch", "application/vnd.google-apps.drawing")],
        );
        assert_eq!(entries[0].mime.as_deref(), Some("image/png"));
        assert_eq!(entries[0].rel_path, "Sketch.png");

        // A file that is downloaded as-is keeps its own type.
        let entries = to_entries("", vec![file("p1", "x.pdf", "application/pdf")]);
        assert_eq!(entries[0].mime.as_deref(), Some("application/pdf"));
    }

    /// The export decision rides on the locator, so `fetch` can still tell
    /// export from download once `mime` describes the exported bytes.
    #[test]
    fn the_locator_carries_the_export_decision() {
        let entries = to_entries("", vec![file("d1", "Q3", DOC_MIME)]);
        let (export, id) = parse_locator(&entries[0].locator);
        assert_eq!(id, "d1");
        assert_eq!(
            export,
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
        );

        let entries = to_entries("", vec![file("p1", "x.pdf", "application/pdf")]);
        let (export, id) = parse_locator(&entries[0].locator);
        assert_eq!(id, "p1");
        assert_eq!(export, None, "a binary file is downloaded, not exported");
    }

    #[test]
    fn native_types_with_no_document_in_them_are_dropped_not_reported_as_unreadable() {
        let entries = to_entries(
            "",
            vec![
                file("f1", "Signup Form", "application/vnd.google-apps.form"),
                file("s1", "A shortcut", "application/vnd.google-apps.shortcut"),
                file("p1", "notes.pdf", "application/pdf"),
            ],
        );
        let paths: Vec<&str> = entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["notes.pdf"],
            "a Form is not an unreadable document, it is not a document"
        );
    }

    #[test]
    fn a_binary_file_keeps_its_own_name_and_is_downloaded_not_exported() {
        let entries = to_entries("docs", vec![file("p1", "invoice.pdf", "application/pdf")]);
        assert_eq!(entries[0].rel_path, "docs/invoice.pdf");
        assert!(export_target("application/pdf").is_none());
    }

    #[test]
    fn folders_are_directories_and_carry_no_mime() {
        let entries = to_entries("", vec![file("dir1", "Projects", FOLDER_MIME)]);
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[0].rel_path, "Projects");
        assert!(entries[0].mime.is_none());
    }

    /// Drive lets two files in one folder share a name; `rag_files` is unique
    /// on (collection, path). Without disambiguation the second silently
    /// replaces the first.
    #[test]
    fn two_files_with_one_name_get_distinct_paths() {
        let entries = to_entries(
            "",
            vec![
                file("aaa", "report.pdf", "application/pdf"),
                file("bbb", "report.pdf", "application/pdf"),
                file("ccc", "unique.pdf", "application/pdf"),
            ],
        );
        let mut paths: Vec<&str> = entries.iter().map(|e| e.rel_path.as_str()).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec!["report ~aaa.pdf", "report ~bbb.pdf", "unique.pdf"],
            "both colliding files are suffixed, so a path never depends on \
             the order Drive happened to list the group in"
        );
    }

    /// A collision that only exists *after* exporting must still be resolved.
    ///
    /// Counting the bare Drive name missed this: a Google Doc called `report`
    /// and an uploaded `report.docx` are two distinct names, and both become
    /// `report.docx` once the export extension is appended.
    #[test]
    fn a_doc_and_an_upload_that_export_to_the_same_name_are_separated() {
        let entries = to_entries(
            "",
            vec![
                file("doc1", "report", DOC_MIME),
                file(
                    "up1",
                    "report.docx",
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                ),
            ],
        );
        let mut paths: Vec<&str> = entries.iter().map(|e| e.rel_path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec!["report ~doc1.docx", "report ~up1.docx"]);
        assert_ne!(
            entries[0].rel_path, entries[1].rel_path,
            "`rag_files` is unique on path — equal paths silently drop a document"
        );
    }

    #[test]
    fn a_name_containing_a_slash_does_not_invent_a_directory() {
        let entries = to_entries("root", vec![file("x", "2025/2026 budget", SHEET_MIME)]);
        assert_eq!(entries[0].rel_path, "root/2025_2026 budget.xlsx");
        assert_eq!(
            entries[0].rel_path.matches('/').count(),
            1,
            "only the separator we added is a separator"
        );
    }

    #[test]
    fn the_version_is_the_change_token_and_falls_back_rather_than_pinning() {
        let mut f = file("a", "x.pdf", "application/pdf");
        f.version = None;
        let entries = to_entries("", vec![f]);
        assert_eq!(
            entries[0].version, "2026-01-01T00:00:00.000Z",
            "with no version, the modified time still moves when the file does"
        );
    }

    #[test]
    fn identity_is_the_drive_id_not_the_path() {
        let entries = to_entries("a/b", vec![file("stable-id", "x.pdf", "application/pdf")]);
        assert_eq!(entries[0].id, "stable-id");
        assert_eq!(entries[0].locator, "stable-id");
    }

    /// `list_dir` must hand `to_entries` the whole directory. Naming per page
    /// would count a colliding pair that straddles a `files.list` page
    /// boundary as unique twice, and the two would land on one `rel_path` —
    /// which `rag_files` resolves by silently dropping one of them.
    #[test]
    fn a_collision_split_across_pages_is_still_resolved() {
        let page_one = vec![file("aaa", "report.pdf", "application/pdf")];
        let page_two = vec![file("bbb", "report.pdf", "application/pdf")];

        let per_page: Vec<String> = to_entries("", page_one.clone())
            .into_iter()
            .chain(to_entries("", page_two.clone()))
            .map(|e| e.rel_path)
            .collect();
        assert_eq!(
            per_page[0], per_page[1],
            "sanity: naming page by page is exactly the collision to avoid"
        );

        let whole: Vec<String> = to_entries("", [page_one, page_two].concat())
            .into_iter()
            .map(|e| e.rel_path)
            .collect();
        assert_ne!(
            whole[0], whole[1],
            "given the whole directory, both are suffixed and distinct"
        );
    }

    #[test]
    fn capabilities_say_what_drive_can_actually_do() {
        let p = provider();
        let caps = p.capabilities();
        assert!(caps.stable_ids, "a Drive id survives rename and move");
        assert!(
            !caps.subtree_pruning,
            "Drive folders carry no propagating version, so claiming pruning \
             would skip subtrees that did change"
        );
    }

    #[test]
    fn a_query_value_cannot_break_out_of_the_expression() {
        assert_eq!(escape_q("it's"), "it\\'s");
        assert_eq!(escape_q("a\\b"), "a\\\\b");
    }

    #[test]
    fn an_empty_or_control_only_name_still_yields_a_usable_segment() {
        assert_eq!(sanitize_segment("   "), "untitled");
        assert_eq!(sanitize_segment("a\u{0}b"), "a b");
    }

    /// A half-configured Drive source must *save* — the operator has to be
    /// able to store the client id and secret before there is anything to
    /// consent with. Refusing at `validate` time would deadlock the flow:
    /// no save without consent, no consent without a saved client.
    #[test]
    fn a_client_without_consent_validates_but_cannot_yet_be_built() {
        let cfg = ProviderConfig::new(
            [("client_id".to_string(), "cid".to_string())]
                .into_iter()
                .collect(),
            [("client_secret".to_string(), "sec".to_string())]
                .into_iter()
                .collect(),
        );
        GoogleDriveFactory
            .validate(&cfg)
            .expect("the operator can save a client before connecting it");

        let Err(err) = GoogleDriveFactory.build(&cfg, reqwest::Client::new()) else {
            panic!("a provider with no refresh token cannot reach Drive");
        };
        assert!(
            err.to_string().contains("Connect"),
            "the message tells the operator what to click: {err}"
        );
    }

    #[test]
    fn the_factory_advertises_the_consent_flow_rather_than_a_password_field() {
        match GoogleDriveFactory.auth() {
            AuthKind::OAuth2 {
                authorize_url,
                scopes,
                ..
            } => {
                assert!(authorize_url.starts_with("https://accounts.google.com/"));
                assert!(scopes.iter().any(|s| s.ends_with("drive.readonly")));
            }
            AuthKind::Fields => panic!("Drive cannot be authorised by typed credentials"),
        }
        assert!(
            !GoogleDriveFactory
                .config_fields()
                .iter()
                .any(|f| f.key == REFRESH_TOKEN_KEY),
            "the refresh token is minted by the callback, never typed"
        );
    }

    const DOC_MIME: &str = "application/vnd.google-apps.document";
    const SHEET_MIME: &str = "application/vnd.google-apps.spreadsheet";
    const SLIDES_MIME: &str = "application/vnd.google-apps.presentation";

    fn provider() -> GoogleDriveProvider {
        GoogleDriveProvider {
            client_id: "cid".into(),
            client_secret: "sec".into(),
            refresh_token: "rt".into(),
            root_folder_id: "root".into(),
            http: reqwest::Client::new(),
            access: tokio::sync::Mutex::new(None),
        }
    }
}
