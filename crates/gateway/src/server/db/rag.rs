// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! RAG persistence — collections, indexed files, chunk metadata.
//!
//! Schema lives in `migrations/0013_rag.sql`. The vector data itself is
//! outside SQLite (per-collection `data/rag/<id>.usearch` files); this
//! module owns the metadata side and the indexer's status transitions.
//!
//! Conventions:
//!   * `i64` everywhere SQLite uses `INTEGER PRIMARY KEY`.
//!   * RFC 3339 strings for timestamps, parsed/rendered via `jiff::Timestamp`.
//!   * Glob arrays travel through the DB as JSON-encoded text — keeps the
//!     schema simple and lets callers thread `Vec<String>` end-to-end.

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{DbError, Pool};

/// Lifecycle of a collection from the indexer's point of view. The chat
/// surface only ever searches `Ready` collections; everything else is in
/// some intermediate state the admin UI surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionStatus {
    /// New row, or re-index requested. The indexer will pick it up.
    Pending,
    /// Indexer is currently cloning the repo.
    Cloning,
    /// Repo is cloned; chunks are being embedded.
    Indexing,
    /// Last indexing run succeeded; searchable.
    Ready,
    /// Last indexing run failed; see `last_error`. Won't retry until
    /// status is flipped back to `Pending`.
    Error,
}

impl CollectionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CollectionStatus::Pending => "pending",
            CollectionStatus::Cloning => "cloning",
            CollectionStatus::Indexing => "indexing",
            CollectionStatus::Ready => "ready",
            CollectionStatus::Error => "error",
        }
    }

    /// Lenient parse — any stray DB value lands in `Error` so a single
    /// bad row can't keep the admin page from rendering.
    fn from_db(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "cloning" => Self::Cloning,
            "indexing" => Self::Indexing,
            "ready" => Self::Ready,
            _ => Self::Error,
        }
    }
}

/// One configured codebase. Fields mirror the migration; see `0013_rag.sql`
/// for the why-each-column commentary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub git_url: String,
    pub git_ref: String,
    pub pat: Option<String>,
    pub embedding_model: String,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub chunk_size: i64,
    pub chunk_overlap: i64,
    pub status: CollectionStatus,
    pub last_indexed_at: Option<Timestamp>,
    pub last_indexed_commit: Option<String>,
    pub last_error: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Parameters for creating a fresh collection. Splits caller-supplied
/// state from indexer-owned state (status, last_*) so callers can't
/// accidentally fabricate a "ready" row.
#[derive(Debug, Clone)]
pub struct NewCollection {
    pub name: String,
    pub description: Option<String>,
    pub git_url: String,
    pub git_ref: String,
    pub pat: Option<String>,
    pub embedding_model: String,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub chunk_size: i64,
    pub chunk_overlap: i64,
}

fn parse_ts(s: &str, column: &'static str) -> Result<Timestamp, DbError> {
    s.parse().map_err(|e: jiff::Error| DbError::Decode {
        column,
        source: e.into(),
    })
}

fn decode_globs(s: &str, column: &'static str) -> Result<Vec<String>, DbError> {
    serde_json::from_str(s).map_err(|e| DbError::Decode {
        column,
        source: anyhow::Error::from(e),
    })
}

fn map_collection_row(row: &SqliteRow) -> Result<Collection, DbError> {
    let last_indexed_at: Option<String> = row.try_get("last_indexed_at")?;
    let last_indexed_at = last_indexed_at
        .map(|s| parse_ts(&s, "last_indexed_at"))
        .transpose()?;
    let include_globs_json: String = row.try_get("include_globs_json")?;
    let exclude_globs_json: String = row.try_get("exclude_globs_json")?;
    let created_at_s: String = row.try_get("created_at")?;
    let updated_at_s: String = row.try_get("updated_at")?;
    let status_s: String = row.try_get("status")?;
    Ok(Collection {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        git_url: row.try_get("git_url")?,
        git_ref: row.try_get("git_ref")?,
        pat: row.try_get("pat")?,
        embedding_model: row.try_get("embedding_model")?,
        include_globs: decode_globs(&include_globs_json, "include_globs_json")?,
        exclude_globs: decode_globs(&exclude_globs_json, "exclude_globs_json")?,
        chunk_size: row.try_get("chunk_size")?,
        chunk_overlap: row.try_get("chunk_overlap")?,
        status: CollectionStatus::from_db(&status_s),
        last_indexed_at,
        last_indexed_commit: row.try_get("last_indexed_commit")?,
        last_error: row.try_get("last_error")?,
        created_at: parse_ts(&created_at_s, "created_at")?,
        updated_at: parse_ts(&updated_at_s, "updated_at")?,
    })
}

const COLLECTION_COLUMNS: &str = "id, name, description, git_url, git_ref, pat, \
     embedding_model, include_globs_json, exclude_globs_json, chunk_size, chunk_overlap, \
     status, last_indexed_at, last_indexed_commit, last_error, created_at, updated_at";

pub async fn create_collection(pool: &Pool, new: &NewCollection) -> Result<Collection, DbError> {
    let now = Timestamp::now();
    let now_s = now.to_string();
    let include_json = serde_json::to_string(&new.include_globs).map_err(|e| DbError::Decode {
        column: "include_globs_json",
        source: e.into(),
    })?;
    let exclude_json = serde_json::to_string(&new.exclude_globs).map_err(|e| DbError::Decode {
        column: "exclude_globs_json",
        source: e.into(),
    })?;
    let id: i64 = sqlx::query_scalar(
        r#"INSERT INTO rag_collections
           (name, description, git_url, git_ref, pat, embedding_model,
            include_globs_json, exclude_globs_json, chunk_size, chunk_overlap,
            status, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?)
           RETURNING id"#,
    )
    .bind(&new.name)
    .bind(&new.description)
    .bind(&new.git_url)
    .bind(&new.git_ref)
    .bind(&new.pat)
    .bind(&new.embedding_model)
    .bind(&include_json)
    .bind(&exclude_json)
    .bind(new.chunk_size)
    .bind(new.chunk_overlap)
    .bind(&now_s)
    .bind(&now_s)
    .fetch_one(pool)
    .await?;
    find_collection_by_id(pool, id)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound.into())
}

pub async fn list_collections(pool: &Pool) -> Result<Vec<Collection>, DbError> {
    let q = format!("SELECT {COLLECTION_COLUMNS} FROM rag_collections ORDER BY created_at DESC");
    let rows = sqlx::query(&q).fetch_all(pool).await?;
    rows.iter().map(map_collection_row).collect()
}

pub async fn find_collection_by_id(pool: &Pool, id: i64) -> Result<Option<Collection>, DbError> {
    let q = format!("SELECT {COLLECTION_COLUMNS} FROM rag_collections WHERE id = ?");
    let row = sqlx::query(&q).bind(id).fetch_optional(pool).await?;
    row.as_ref().map(map_collection_row).transpose()
}

pub async fn find_collection_by_name(
    pool: &Pool,
    name: &str,
) -> Result<Option<Collection>, DbError> {
    let q = format!("SELECT {COLLECTION_COLUMNS} FROM rag_collections WHERE name = ?");
    let row = sqlx::query(&q).bind(name).fetch_optional(pool).await?;
    row.as_ref().map(map_collection_row).transpose()
}

/// Set a collection's status. Indexer-only; the admin API uses
/// [`request_reindex`] to bump back to `Pending` rather than calling this
/// directly so timestamps stay consistent.
pub async fn set_collection_status(
    pool: &Pool,
    id: i64,
    status: CollectionStatus,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query("UPDATE rag_collections SET status = ?, updated_at = ? WHERE id = ?")
        .bind(status.as_str())
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Indexer-side: a successful run lands here. Sets status to `Ready`,
/// stamps `last_indexed_at`, records the resolved commit, and clears
/// any prior `last_error`.
pub async fn mark_indexed(pool: &Pool, id: i64, commit_sha: &str) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"UPDATE rag_collections
           SET status = 'ready', last_indexed_at = ?, last_indexed_commit = ?,
               last_error = NULL, updated_at = ?
           WHERE id = ?"#,
    )
    .bind(&now)
    .bind(commit_sha)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Indexer-side: a failed run lands here. Status → `Error`, message
/// stored verbatim. Stays in `Error` until an admin reset.
pub async fn mark_failed(pool: &Pool, id: i64, message: &str) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"UPDATE rag_collections
           SET status = 'error', last_error = ?, updated_at = ?
           WHERE id = ?"#,
    )
    .bind(message)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Admin-side: re-queue a collection for indexing regardless of its
/// current status. Clears the prior error so the UI can show "queued"
/// without sticky failure text bleeding through.
pub async fn request_reindex(pool: &Pool, id: i64) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"UPDATE rag_collections
           SET status = 'pending', last_error = NULL, updated_at = ?
           WHERE id = ?"#,
    )
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_collection(pool: &Pool, id: i64) -> Result<bool, DbError> {
    let affected = sqlx::query("DELETE FROM rag_collections WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

// ---- file-side metadata ---------------------------------------------------

/// One indexed source file. `content_hash` is the diff key: the indexer
/// compares it on re-pull and skips re-chunking files whose hash matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    pub id: i64,
    pub collection_id: i64,
    pub path: String,
    pub content_hash: String,
    pub indexed_at: Timestamp,
}

fn map_file_row(row: &SqliteRow) -> Result<IndexedFile, DbError> {
    let indexed_at_s: String = row.try_get("indexed_at")?;
    Ok(IndexedFile {
        id: row.try_get("id")?,
        collection_id: row.try_get("collection_id")?,
        path: row.try_get("path")?,
        content_hash: row.try_get("content_hash")?,
        indexed_at: parse_ts(&indexed_at_s, "indexed_at")?,
    })
}

pub async fn list_files_for_collection(
    pool: &Pool,
    collection_id: i64,
) -> Result<Vec<IndexedFile>, DbError> {
    let rows = sqlx::query(
        r#"SELECT id, collection_id, path, content_hash, indexed_at
           FROM rag_files WHERE collection_id = ?"#,
    )
    .bind(collection_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(map_file_row).collect()
}

/// Upsert a file row. Returns the file's id. Used by the indexer after
/// it has decided this file needs (re-)embedding — paired with a
/// [`delete_chunks_for_file`] so the old chunks/vectors are reaped first.
pub async fn upsert_file(
    pool: &Pool,
    collection_id: i64,
    path: &str,
    content_hash: &str,
) -> Result<i64, DbError> {
    let now = Timestamp::now().to_string();
    let id: i64 = sqlx::query_scalar(
        r#"INSERT INTO rag_files (collection_id, path, content_hash, indexed_at)
           VALUES (?, ?, ?, ?)
           ON CONFLICT (collection_id, path) DO UPDATE
               SET content_hash = excluded.content_hash,
                   indexed_at   = excluded.indexed_at
           RETURNING id"#,
    )
    .bind(collection_id)
    .bind(path)
    .bind(content_hash)
    .bind(&now)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn delete_file(pool: &Pool, file_id: i64) -> Result<(), DbError> {
    sqlx::query("DELETE FROM rag_files WHERE id = ?")
        .bind(file_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---- chunk-side metadata --------------------------------------------------

/// One chunk: provenance for a single vector. `vector_id` is the integer
/// key into the per-collection usearch index file. `file_path` is
/// materialised at fetch time (JOIN against `rag_files`) so the search
/// tool can render `path:start-end` without a second round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub id: i64,
    pub collection_id: i64,
    pub file_id: i64,
    pub file_path: String,
    pub chunk_index: i64,
    pub start_line: i64,
    pub end_line: i64,
    pub content: String,
    pub vector_id: i64,
}

/// A chunk awaiting insertion. The indexer fills these out then calls
/// [`insert_chunks`] in batches alongside the vector upsert.
#[derive(Debug, Clone)]
pub struct NewChunk {
    pub file_id: i64,
    pub chunk_index: i64,
    pub start_line: i64,
    pub end_line: i64,
    pub content: String,
    pub vector_id: i64,
}

fn map_chunk_row(row: &SqliteRow) -> Result<Chunk, DbError> {
    Ok(Chunk {
        id: row.try_get("id")?,
        collection_id: row.try_get("collection_id")?,
        file_id: row.try_get("file_id")?,
        file_path: row.try_get("file_path")?,
        chunk_index: row.try_get("chunk_index")?,
        start_line: row.try_get("start_line")?,
        end_line: row.try_get("end_line")?,
        content: row.try_get("content")?,
        vector_id: row.try_get("vector_id")?,
    })
}

/// Insert a batch of chunks in one transaction. Empty input is a no-op.
pub async fn insert_chunks(
    pool: &Pool,
    collection_id: i64,
    chunks: &[NewChunk],
) -> Result<(), DbError> {
    if chunks.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for c in chunks {
        sqlx::query(
            r#"INSERT INTO rag_chunks
               (collection_id, file_id, chunk_index, start_line, end_line, content, vector_id)
               VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(collection_id)
        .bind(c.file_id)
        .bind(c.chunk_index)
        .bind(c.start_line)
        .bind(c.end_line)
        .bind(&c.content)
        .bind(c.vector_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Returns `(chunk_id, vector_id)` pairs for every chunk belonging to
/// `file_id`. The indexer uses this when a file changes: pull the prior
/// vector ids, remove them from usearch, then delete the chunks.
pub async fn chunk_vector_ids_for_file(pool: &Pool, file_id: i64) -> Result<Vec<i64>, DbError> {
    let rows: Vec<i64> = sqlx::query_scalar("SELECT vector_id FROM rag_chunks WHERE file_id = ?")
        .bind(file_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn delete_chunks_for_file(pool: &Pool, file_id: i64) -> Result<(), DbError> {
    sqlx::query("DELETE FROM rag_chunks WHERE file_id = ?")
        .bind(file_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Largest `vector_id` seen in `collection_id`, or `None` for an empty
/// collection. The indexer uses this to pick the next id to allocate so
/// vector ids stay monotonic across re-indexes — usearch tolerates
/// reused ids in principle, but monotonic-only keeps debugging sane.
pub async fn max_vector_id(pool: &Pool, collection_id: i64) -> Result<Option<i64>, DbError> {
    let max: Option<i64> =
        sqlx::query_scalar("SELECT MAX(vector_id) FROM rag_chunks WHERE collection_id = ?")
            .bind(collection_id)
            .fetch_one(pool)
            .await?;
    Ok(max)
}

/// Resolve a batch of `(collection_id, vector_id)` hits from a usearch
/// search back into chunk rows so the tool can surface provenance.
/// Preserves caller order; missing rows are dropped silently (they would
/// be index/db drift — rare and not worth failing the call).
pub async fn chunks_by_vector_ids(
    pool: &Pool,
    collection_id: i64,
    vector_ids: &[i64],
) -> Result<Vec<Chunk>, DbError> {
    if vector_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; vector_ids.len()].join(",");
    let q = format!(
        "SELECT c.id, c.collection_id, c.file_id, f.path AS file_path, \
                c.chunk_index, c.start_line, c.end_line, c.content, c.vector_id \
         FROM rag_chunks c \
         JOIN rag_files f ON f.id = c.file_id \
         WHERE c.collection_id = ? AND c.vector_id IN ({placeholders})"
    );
    let mut query = sqlx::query(&q).bind(collection_id);
    for vid in vector_ids {
        query = query.bind(vid);
    }
    let rows = query.fetch_all(pool).await?;
    let mut by_vid: std::collections::HashMap<i64, Chunk> = std::collections::HashMap::new();
    for row in &rows {
        let c = map_chunk_row(row)?;
        by_vid.insert(c.vector_id, c);
    }
    Ok(vector_ids
        .iter()
        .filter_map(|vid| by_vid.remove(vid))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::open;
    use std::path::Path;

    async fn fresh() -> Pool {
        open(Path::new(":memory:")).await.unwrap()
    }

    fn sample_new() -> NewCollection {
        NewCollection {
            name: "gateway".into(),
            description: Some("the gateway repo".into()),
            git_url: "https://example.invalid/repo.git".into(),
            git_ref: "main".into(),
            pat: None,
            embedding_model: "embed-model".into(),
            include_globs: vec!["*.rs".into()],
            exclude_globs: vec!["target/".into()],
            chunk_size: 800,
            chunk_overlap: 100,
        }
    }

    #[tokio::test]
    async fn create_and_round_trip_collection() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        assert_eq!(c.name, "gateway");
        assert_eq!(c.status, CollectionStatus::Pending);
        assert_eq!(c.include_globs, vec!["*.rs"]);
        assert_eq!(c.exclude_globs, vec!["target/"]);

        let by_id = find_collection_by_id(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(by_id, c);
        let by_name = find_collection_by_name(&pool, "gateway")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_name, c);
    }

    #[tokio::test]
    async fn unique_name_constraint_rejects_duplicates() {
        let pool = fresh().await;
        create_collection(&pool, &sample_new()).await.unwrap();
        let err = create_collection(&pool, &sample_new()).await.unwrap_err();
        assert!(
            matches!(err, DbError::Query(_)),
            "expected Query(UNIQUE failure), got {err:?}"
        );
    }

    #[tokio::test]
    async fn lifecycle_transitions_clear_and_set_last_error() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();

        set_collection_status(&pool, c.id, CollectionStatus::Indexing)
            .await
            .unwrap();
        mark_failed(&pool, c.id, "git auth failed").await.unwrap();
        let after_fail = find_collection_by_id(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(after_fail.status, CollectionStatus::Error);
        assert_eq!(after_fail.last_error.as_deref(), Some("git auth failed"));

        request_reindex(&pool, c.id).await.unwrap();
        let after_requeue = find_collection_by_id(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(after_requeue.status, CollectionStatus::Pending);
        assert!(after_requeue.last_error.is_none());

        mark_indexed(&pool, c.id, "abc123").await.unwrap();
        let after_ok = find_collection_by_id(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(after_ok.status, CollectionStatus::Ready);
        assert_eq!(after_ok.last_indexed_commit.as_deref(), Some("abc123"));
        assert!(after_ok.last_indexed_at.is_some());
        assert!(after_ok.last_error.is_none());
    }

    #[tokio::test]
    async fn upsert_file_returns_stable_id_and_updates_hash() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let id1 = upsert_file(&pool, c.id, "src/main.rs", "hash-a")
            .await
            .unwrap();
        let id2 = upsert_file(&pool, c.id, "src/main.rs", "hash-b")
            .await
            .unwrap();
        assert_eq!(id1, id2, "upsert must keep the same row id");
        let files = list_files_for_collection(&pool, c.id).await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content_hash, "hash-b");
    }

    #[tokio::test]
    async fn chunks_round_trip_with_vector_id_join() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let f = upsert_file(&pool, c.id, "src/lib.rs", "h").await.unwrap();
        insert_chunks(
            &pool,
            c.id,
            &[
                NewChunk {
                    file_id: f,
                    chunk_index: 0,
                    start_line: 1,
                    end_line: 10,
                    content: "first".into(),
                    vector_id: 10,
                },
                NewChunk {
                    file_id: f,
                    chunk_index: 1,
                    start_line: 11,
                    end_line: 20,
                    content: "second".into(),
                    vector_id: 11,
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(max_vector_id(&pool, c.id).await.unwrap(), Some(11));

        // Caller-order preserved; missing ids dropped.
        let resolved = chunks_by_vector_ids(&pool, c.id, &[11, 999, 10])
            .await
            .unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].content, "second");
        assert_eq!(resolved[1].content, "first");
    }

    #[tokio::test]
    async fn deleting_collection_cascades_files_and_chunks() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let f = upsert_file(&pool, c.id, "src/x.rs", "h").await.unwrap();
        insert_chunks(
            &pool,
            c.id,
            &[NewChunk {
                file_id: f,
                chunk_index: 0,
                start_line: 1,
                end_line: 1,
                content: "x".into(),
                vector_id: 1,
            }],
        )
        .await
        .unwrap();
        assert!(delete_collection(&pool, c.id).await.unwrap());
        assert!(
            list_files_for_collection(&pool, c.id)
                .await
                .unwrap()
                .is_empty()
        );
        let leftover: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM rag_chunks WHERE collection_id = ?")
                .bind(c.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(leftover, 0);
    }

    #[tokio::test]
    async fn chunks_by_vector_ids_empty_input_is_empty_output() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        assert!(
            chunks_by_vector_ids(&pool, c.id, &[])
                .await
                .unwrap()
                .is_empty()
        );
    }
}
