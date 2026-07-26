// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The document canvas store — long-form documents the model builds up
//! and edits incrementally across turns (the `create_document` /
//! `edit_document` / `read_document` / `list_documents` tools and the
//! chat-page canvas panel).
//!
//! Generalises the per-template Typst data-document pattern into a
//! freeform, format-agnostic store. A [`Document`] is titled content with
//! a [`DocumentFormat`]; every edit appends an immutable [`DocumentVersion`]
//! and bumps `current_ver`, so the canvas keeps a full, scrubbable history
//! and the model can change one passage without resending the whole thing.
//!
//! Scoped to a chat session: every query is keyed by `session_id`, so a
//! tool call can only ever touch documents from its own conversation.
//!
//! Schema lives in `migrations/0027_documents.sql`.

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

use super::{DbError, Pool};

/// The content type of a document. Drives both how the model edits it
/// (see [`DocumentFormat::edit_kind`]) and how the canvas panel renders
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentFormat {
    Markdown,
    Text,
    Html,
    Json,
    Toml,
    /// Typst source — drafted in the canvas, rendered via `render_typst`
    /// / `export_document`. Text-edited; sections anchor on `=` headings.
    Typst,
    /// YAML — deliberately TEXT-edited, not structured: a parse→patch→
    /// reserialise round-trip would strip comments, anchors, and key
    /// order, which is exactly what humans keep in YAML configs.
    Yaml,
}

/// How edits address a document of a given format: anchored find/replace
/// for free text, RFC 6902 JSON Patch for structured data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// `markdown` / `text` / `html` / `typst` / `yaml` — edited with
    /// anchored find/replace (yaml deliberately so, to preserve comments).
    Text,
    /// `json` / `toml` — edited with an RFC 6902 JSON Patch (TOML is
    /// parsed to JSON, patched, then reserialised).
    Structured,
}

impl DocumentFormat {
    /// Iteration/listing order.
    pub const ALL: [DocumentFormat; 7] = [
        DocumentFormat::Markdown,
        DocumentFormat::Text,
        DocumentFormat::Html,
        DocumentFormat::Json,
        DocumentFormat::Toml,
        DocumentFormat::Typst,
        DocumentFormat::Yaml,
    ];

    /// Stable string stored in the DB column + accepted from tool args.
    pub fn as_str(self) -> &'static str {
        match self {
            DocumentFormat::Markdown => "markdown",
            DocumentFormat::Text => "text",
            DocumentFormat::Html => "html",
            DocumentFormat::Json => "json",
            DocumentFormat::Toml => "toml",
            DocumentFormat::Typst => "typst",
            DocumentFormat::Yaml => "yaml",
        }
    }

    /// Parse a caller-supplied format, rejecting anything unknown. `md`
    /// is accepted as an alias for `markdown`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "markdown" | "md" => Some(DocumentFormat::Markdown),
            "text" | "txt" | "plain" => Some(DocumentFormat::Text),
            "html" => Some(DocumentFormat::Html),
            "json" => Some(DocumentFormat::Json),
            "toml" => Some(DocumentFormat::Toml),
            "typst" | "typ" => Some(DocumentFormat::Typst),
            "yaml" | "yml" => Some(DocumentFormat::Yaml),
            _ => None,
        }
    }

    /// Parse a value read back from the DB, defaulting to `Text` for
    /// anything unexpected — a stray row should never fail a listing.
    fn from_db(s: &str) -> Self {
        Self::parse(s).unwrap_or(DocumentFormat::Text)
    }

    /// Conventional file extension for materialising a document of this
    /// format on disk (sandbox staging, exports).
    pub fn file_ext(self) -> &'static str {
        match self {
            DocumentFormat::Markdown => "md",
            DocumentFormat::Text => "txt",
            DocumentFormat::Html => "html",
            DocumentFormat::Json => "json",
            DocumentFormat::Toml => "toml",
            DocumentFormat::Typst => "typ",
            DocumentFormat::Yaml => "yaml",
        }
    }

    /// How this format is edited.
    pub fn edit_kind(self) -> EditKind {
        match self {
            DocumentFormat::Markdown
            | DocumentFormat::Text
            | DocumentFormat::Html
            | DocumentFormat::Typst
            | DocumentFormat::Yaml => EditKind::Text,
            DocumentFormat::Json | DocumentFormat::Toml => EditKind::Structured,
        }
    }
}

/// A document's metadata (without content). Content lives in
/// [`DocumentVersion`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub id: String,
    pub session_id: String,
    pub title: String,
    pub format: DocumentFormat,
    pub current_ver: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// `Some` once the document has been soft-deleted (see [`soft_delete`]).
    /// A deleted document is hidden from listings and from the canvas panel
    /// but still resolves by id, so it stays readable and can be restored
    /// with its version history intact.
    pub deleted_at: Option<Timestamp>,
}

impl Document {
    /// Convenience for the common check: is this document in the bin?
    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }
}

/// One immutable revision of a document's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentVersion {
    pub document_id: String,
    pub version: i64,
    pub content: String,
    pub summary: Option<String>,
    pub turn_id: Option<String>,
    pub created_at: Timestamp,
}

fn parse_ts(s: &str, column: &'static str) -> Result<Timestamp, DbError> {
    s.parse().map_err(|e: jiff::Error| DbError::Decode {
        column,
        source: e.into(),
    })
}

/// The column list every `Document`-returning query selects. Kept in one
/// place so adding a field can't leave one query behind returning a row
/// [`map_doc`] then fails to decode.
const DOC_COLUMNS: &str =
    "id, session_id, title, format, current_ver, created_at, updated_at, deleted_at";

fn map_doc(row: &SqliteRow) -> Result<Document, DbError> {
    let format: String = row.try_get("format")?;
    Ok(Document {
        id: row.try_get("id")?,
        session_id: row.try_get("session_id")?,
        title: row.try_get("title")?,
        format: DocumentFormat::from_db(&format),
        current_ver: row.try_get("current_ver")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?, "created_at")?,
        updated_at: parse_ts(&row.try_get::<String, _>("updated_at")?, "updated_at")?,
        deleted_at: row
            .try_get::<Option<String>, _>("deleted_at")?
            .as_deref()
            .map(|s| parse_ts(s, "deleted_at"))
            .transpose()?,
    })
}

fn map_version(row: &SqliteRow) -> Result<DocumentVersion, DbError> {
    Ok(DocumentVersion {
        document_id: row.try_get("document_id")?,
        version: row.try_get("version")?,
        content: row.try_get("content")?,
        summary: row.try_get("summary")?,
        turn_id: row.try_get("turn_id")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?, "created_at")?,
    })
}

/// Generate a fresh document id.
pub fn new_id() -> String {
    format!("doc_{}", Uuid::new_v4().simple())
}

/// Create a new document with version 1. Returns the stored metadata.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &Pool,
    id: &str,
    session_id: &str,
    user_id: &str,
    title: &str,
    format: DocumentFormat,
    content: &str,
    turn_id: Option<&str>,
) -> Result<Document, DbError> {
    let now = Timestamp::now();
    let now_s = now.to_string();

    sqlx::query(
        r#"INSERT INTO documents
               (id, session_id, user_id, title, format, current_ver, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, 1, ?, ?)"#,
    )
    .bind(id)
    .bind(session_id)
    .bind(user_id)
    .bind(title)
    .bind(format.as_str())
    .bind(&now_s)
    .bind(&now_s)
    .execute(pool)
    .await?;

    sqlx::query(
        r#"INSERT INTO document_versions
               (document_id, version, content, summary, turn_id, created_at)
           VALUES (?, 1, ?, 'Created', ?, ?)"#,
    )
    .bind(id)
    .bind(content)
    .bind(turn_id)
    .bind(&now_s)
    .execute(pool)
    .await?;

    Ok(Document {
        id: id.to_string(),
        session_id: session_id.to_string(),
        title: title.to_string(),
        format,
        current_ver: 1,
        created_at: now,
        updated_at: now,
        deleted_at: None,
    })
}

/// Fetch a document's metadata, scoped to its session. `None` if it
/// doesn't exist or belongs to another conversation.
///
/// Soft-deleted documents **are** returned — deletion hides a document from
/// listings, it doesn't make it unresolvable, which is what lets it be read
/// and restored afterwards. Callers that mutate must check
/// [`Document::is_deleted`] themselves.
pub async fn get(pool: &Pool, session_id: &str, id: &str) -> Result<Option<Document>, DbError> {
    let row = sqlx::query(&format!(
        "SELECT {DOC_COLUMNS} FROM documents WHERE id = ? AND session_id = ?"
    ))
    .bind(id)
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_doc).transpose()
}

/// Soft-delete a document: hide it from listings and the canvas panel while
/// keeping the row and its full version history. Scoped to the session.
///
/// Returns `false` when nothing changed — no such document in this
/// conversation, or it was already deleted — so a caller can tell the model
/// the truth instead of reporting a success it didn't cause.
pub async fn soft_delete(pool: &Pool, session_id: &str, id: &str) -> Result<bool, DbError> {
    let res = sqlx::query(
        r#"UPDATE documents SET deleted_at = ?
           WHERE id = ? AND session_id = ? AND deleted_at IS NULL"#,
    )
    .bind(Timestamp::now().to_string())
    .bind(id)
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Undo a [`soft_delete`]. Returns `false` when there was nothing to
/// restore (unknown document, or it was never deleted).
pub async fn undelete(pool: &Pool, session_id: &str, id: &str) -> Result<bool, DbError> {
    let res = sqlx::query(
        r#"UPDATE documents SET deleted_at = NULL
           WHERE id = ? AND session_id = ? AND deleted_at IS NOT NULL"#,
    )
    .bind(id)
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Fetch a specific version's content. `version` of `None` resolves to
/// the document's current version. Scoped to the session.
pub async fn get_version(
    pool: &Pool,
    session_id: &str,
    id: &str,
    version: Option<i64>,
) -> Result<Option<(Document, DocumentVersion)>, DbError> {
    let Some(doc) = get(pool, session_id, id).await? else {
        return Ok(None);
    };
    let v = version.unwrap_or(doc.current_ver);
    let row = sqlx::query(
        r#"SELECT document_id, version, content, summary, turn_id, created_at
           FROM document_versions WHERE document_id = ? AND version = ?"#,
    )
    .bind(id)
    .bind(v)
    .fetch_optional(pool)
    .await?;
    match row.as_ref().map(map_version).transpose()? {
        Some(ver) => Ok(Some((doc, ver))),
        None => Ok(None),
    }
}

/// Append a new version to a document and bump `current_ver`. Returns the
/// refreshed metadata (with the new `current_ver`). Scoped to the
/// session: a no-op `Ok(None)` if the document isn't in this conversation.
pub async fn append_version(
    pool: &Pool,
    session_id: &str,
    id: &str,
    content: &str,
    summary: Option<&str>,
    turn_id: Option<&str>,
) -> Result<Option<Document>, DbError> {
    let Some(doc) = get(pool, session_id, id).await? else {
        return Ok(None);
    };
    let now = Timestamp::now();
    let now_s = now.to_string();
    let next = doc.current_ver + 1;

    sqlx::query(
        r#"INSERT INTO document_versions
               (document_id, version, content, summary, turn_id, created_at)
           VALUES (?, ?, ?, ?, ?, ?)"#,
    )
    .bind(id)
    .bind(next)
    .bind(content)
    .bind(summary)
    .bind(turn_id)
    .bind(&now_s)
    .execute(pool)
    .await?;

    sqlx::query("UPDATE documents SET current_ver = ?, updated_at = ? WHERE id = ?")
        .bind(next)
        .bind(&now_s)
        .bind(id)
        .execute(pool)
        .await?;

    Ok(Some(Document {
        current_ver: next,
        updated_at: now,
        ..doc
    }))
}

/// Version-history metadata for one document: everything but the content,
/// plus the content's size, so a listing never drags whole revisions into
/// memory (or the model's context).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMeta {
    pub version: i64,
    pub summary: Option<String>,
    pub created_at: Timestamp,
    /// Content length in characters (SQLite `LENGTH()` on TEXT).
    pub chars: i64,
}

/// All versions of a document, newest first — metadata only. Scoped to the
/// session; an unknown/foreign document yields an empty list.
pub async fn list_versions(
    pool: &Pool,
    session_id: &str,
    id: &str,
) -> Result<Vec<VersionMeta>, DbError> {
    let rows = sqlx::query(
        r#"SELECT v.version, v.summary, v.created_at, LENGTH(v.content) AS chars
           FROM document_versions v
           JOIN documents d ON d.id = v.document_id
           WHERE v.document_id = ? AND d.session_id = ?
           ORDER BY v.version DESC"#,
    )
    .bind(id)
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(VersionMeta {
                version: row.try_get("version")?,
                summary: row.try_get("summary")?,
                created_at: parse_ts(&row.try_get::<String, _>("created_at")?, "created_at")?,
                chars: row.try_get("chars")?,
            })
        })
        .collect()
}

/// All documents in a session, most recently updated first.
///
/// `include_deleted` is an explicit parameter rather than a second function
/// so every call site has to state which it means: the canvas panel and the
/// model's `list_documents` want live documents only, while "what did I
/// delete?" needs the tombstones too.
pub async fn list_for_session(
    pool: &Pool,
    session_id: &str,
    include_deleted: bool,
) -> Result<Vec<Document>, DbError> {
    let filter = if include_deleted {
        ""
    } else {
        "AND deleted_at IS NULL"
    };
    let rows = sqlx::query(&format!(
        "SELECT {DOC_COLUMNS} FROM documents
         WHERE session_id = ? {filter} ORDER BY updated_at DESC"
    ))
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(map_doc).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::open;
    use std::path::Path;

    async fn seed_session(pool: &Pool, id: &str) {
        sqlx::query(
            r#"INSERT INTO users (id, email, created_at, updated_at)
               VALUES ('u1', 'u1@example.com', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')
               ON CONFLICT(id) DO NOTHING"#,
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO chat_sessions (id, user_id, created_at, updated_at)
               VALUES (?, 'u1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[test]
    fn format_parse_aliases_and_edit_kind() {
        assert_eq!(DocumentFormat::parse("MD"), Some(DocumentFormat::Markdown));
        assert_eq!(DocumentFormat::parse("TOML"), Some(DocumentFormat::Toml));
        assert_eq!(DocumentFormat::parse("nope"), None);
        assert_eq!(DocumentFormat::Markdown.edit_kind(), EditKind::Text);
        assert_eq!(DocumentFormat::Json.edit_kind(), EditKind::Structured);
    }

    #[tokio::test]
    async fn create_then_read_back_version_1() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        let id = new_id();
        create(
            &pool,
            &id,
            "s1",
            "u1",
            "RGW Guide",
            DocumentFormat::Markdown,
            "# Intro\n",
            Some("t1"),
        )
        .await
        .unwrap();
        let (doc, ver) = get_version(&pool, "s1", &id, None).await.unwrap().unwrap();
        assert_eq!(doc.current_ver, 1);
        assert_eq!(doc.title, "RGW Guide");
        assert_eq!(ver.content, "# Intro\n");
        assert_eq!(ver.version, 1);
    }

    #[tokio::test]
    async fn append_bumps_version_and_keeps_history() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        let id = new_id();
        create(
            &pool,
            &id,
            "s1",
            "u1",
            "Doc",
            DocumentFormat::Text,
            "v1",
            None,
        )
        .await
        .unwrap();
        let doc = append_version(&pool, "s1", &id, "v2", Some("edited"), Some("t2"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(doc.current_ver, 2);
        // Latest resolves to v2.
        let (_, latest) = get_version(&pool, "s1", &id, None).await.unwrap().unwrap();
        assert_eq!(latest.content, "v2");
        // History is intact.
        let (_, first) = get_version(&pool, "s1", &id, Some(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.content, "v1");
    }

    #[tokio::test]
    async fn list_versions_is_newest_first_and_session_scoped() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        seed_session(&pool, "s2").await;
        let id = new_id();
        create(
            &pool,
            &id,
            "s1",
            "u1",
            "Doc",
            DocumentFormat::Text,
            "v1",
            None,
        )
        .await
        .unwrap();
        append_version(&pool, "s1", &id, "v2 longer", Some("edited"), None)
            .await
            .unwrap();

        let versions = list_versions(&pool, "s1", &id).await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, 2);
        assert_eq!(versions[0].summary.as_deref(), Some("edited"));
        assert_eq!(versions[0].chars, 9);
        assert_eq!(versions[1].version, 1);
        assert_eq!(versions[1].summary.as_deref(), Some("Created"));
        // Foreign session sees nothing.
        assert!(list_versions(&pool, "s2", &id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn scoped_per_session() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        seed_session(&pool, "s2").await;
        let id = new_id();
        create(
            &pool,
            &id,
            "s1",
            "u1",
            "Doc",
            DocumentFormat::Text,
            "x",
            None,
        )
        .await
        .unwrap();
        // Another session can't see or touch it.
        assert!(get(&pool, "s2", &id).await.unwrap().is_none());
        assert!(
            append_version(&pool, "s2", &id, "hacked", None, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            list_for_session(&pool, "s2", false)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(list_for_session(&pool, "s1", false).await.unwrap().len(), 1);
    }

    /// A soft-deleted document leaves every listing, stays resolvable by id
    /// (so it can still be read and restored), and comes back intact.
    #[tokio::test]
    async fn soft_delete_hides_from_listings_but_keeps_the_document() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        let id = new_id();
        create(
            &pool,
            &id,
            "s1",
            "u1",
            "Draft",
            DocumentFormat::Markdown,
            "v1",
            None,
        )
        .await
        .unwrap();
        append_version(&pool, "s1", &id, "v2", Some("edited"), None)
            .await
            .unwrap();

        assert!(soft_delete(&pool, "s1", &id).await.unwrap());
        // Gone from the default listing, present when asked for explicitly.
        assert!(
            list_for_session(&pool, "s1", false)
                .await
                .unwrap()
                .is_empty()
        );
        let with_deleted = list_for_session(&pool, "s1", true).await.unwrap();
        assert_eq!(with_deleted.len(), 1);
        assert!(with_deleted[0].is_deleted());
        // Still resolvable and readable, history untouched.
        let doc = get(&pool, "s1", &id).await.unwrap().unwrap();
        assert!(doc.is_deleted());
        let (_, ver) = get_version(&pool, "s1", &id, None).await.unwrap().unwrap();
        assert_eq!(ver.content, "v2");
        assert_eq!(list_versions(&pool, "s1", &id).await.unwrap().len(), 2);

        // Deleting twice reports "nothing changed" rather than a fake success.
        assert!(!soft_delete(&pool, "s1", &id).await.unwrap());

        assert!(undelete(&pool, "s1", &id).await.unwrap());
        assert_eq!(list_for_session(&pool, "s1", false).await.unwrap().len(), 1);
        assert!(!get(&pool, "s1", &id).await.unwrap().unwrap().is_deleted());
        // Nothing to restore the second time.
        assert!(!undelete(&pool, "s1", &id).await.unwrap());
    }

    /// Both mutations are session-scoped: a foreign conversation can neither
    /// delete nor resurrect a document it doesn't own.
    #[tokio::test]
    async fn soft_delete_and_undelete_are_session_scoped() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        seed_session(&pool, "s2").await;
        let id = new_id();
        create(
            &pool,
            &id,
            "s1",
            "u1",
            "Doc",
            DocumentFormat::Text,
            "x",
            None,
        )
        .await
        .unwrap();

        assert!(!soft_delete(&pool, "s2", &id).await.unwrap());
        assert!(!get(&pool, "s1", &id).await.unwrap().unwrap().is_deleted());

        assert!(soft_delete(&pool, "s1", &id).await.unwrap());
        assert!(!undelete(&pool, "s2", &id).await.unwrap());
        assert!(get(&pool, "s1", &id).await.unwrap().unwrap().is_deleted());
    }
}
