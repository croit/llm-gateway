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
}

/// How edits address a document of a given format: anchored find/replace
/// for free text, RFC 6902 JSON Patch for structured data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// `markdown` / `text` / `html` — edited with anchored find/replace.
    Text,
    /// `json` / `yaml` / `toml` — edited with an RFC 6902 JSON Patch
    /// (YAML/TOML are parsed to JSON, patched, then reserialised).
    Structured,
}

impl DocumentFormat {
    /// Iteration/listing order.
    pub const ALL: [DocumentFormat; 5] = [
        DocumentFormat::Markdown,
        DocumentFormat::Text,
        DocumentFormat::Html,
        DocumentFormat::Json,
        DocumentFormat::Toml,
    ];

    /// Stable string stored in the DB column + accepted from tool args.
    pub fn as_str(self) -> &'static str {
        match self {
            DocumentFormat::Markdown => "markdown",
            DocumentFormat::Text => "text",
            DocumentFormat::Html => "html",
            DocumentFormat::Json => "json",
            DocumentFormat::Toml => "toml",
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
            _ => None,
        }
    }

    /// Parse a value read back from the DB, defaulting to `Text` for
    /// anything unexpected — a stray row should never fail a listing.
    fn from_db(s: &str) -> Self {
        Self::parse(s).unwrap_or(DocumentFormat::Text)
    }

    /// How this format is edited.
    pub fn edit_kind(self) -> EditKind {
        match self {
            DocumentFormat::Markdown | DocumentFormat::Text | DocumentFormat::Html => {
                EditKind::Text
            }
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
    })
}

/// Fetch a document's metadata, scoped to its session. `None` if it
/// doesn't exist or belongs to another conversation.
pub async fn get(pool: &Pool, session_id: &str, id: &str) -> Result<Option<Document>, DbError> {
    let row = sqlx::query(
        r#"SELECT id, session_id, title, format, current_ver, created_at, updated_at
           FROM documents WHERE id = ? AND session_id = ?"#,
    )
    .bind(id)
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_doc).transpose()
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

/// All documents in a session, most recently updated first.
pub async fn list_for_session(pool: &Pool, session_id: &str) -> Result<Vec<Document>, DbError> {
    let rows = sqlx::query(
        r#"SELECT id, session_id, title, format, current_ver, created_at, updated_at
           FROM documents WHERE session_id = ? ORDER BY updated_at DESC"#,
    )
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
        assert!(list_for_session(&pool, "s2").await.unwrap().is_empty());
        assert_eq!(list_for_session(&pool, "s1").await.unwrap().len(), 1);
    }
}
