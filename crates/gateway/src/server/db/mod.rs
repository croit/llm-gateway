// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! SQLite persistence layer.
//!
//! One pool, opened at startup. Migrations run on connect. The pool is
//! Arc-shared inside `AppState`.

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use thiserror::Error;

pub mod chat_session_tools;
pub mod cli_logins;
pub mod model_defaults;
pub mod rag;
pub mod tokens;
pub mod user_memories;
pub mod user_tool_prefs;
pub mod users;

pub type Pool = sqlx::SqlitePool;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("opening database `{url}`")]
    Open {
        url: String,
        #[source]
        source: sqlx::Error,
    },
    #[error("running migrations")]
    Migrate {
        #[source]
        source: sqlx::migrate::MigrateError,
    },
    #[error("query")]
    Query(#[from] sqlx::Error),
    #[error("decoding row column `{column}`")]
    Decode {
        column: &'static str,
        #[source]
        source: anyhow::Error,
    },
    /// Errors bubbled up from `session-core`'s persistence functions.
    /// Distinct variant (rather than re-flattening into `Query` /
    /// `Decode`) so the call site is obvious in logs.
    #[error("session-core: {0}")]
    Session(#[from] session_core::db::DbError),
}

/// Opens (or creates) a SQLite database at `path` and runs migrations.
///
/// Pass `:memory:` to use an in-memory database. Used by tests.
pub async fn open(path: &Path) -> Result<Pool, DbError> {
    let url = if path == Path::new(":memory:") {
        "sqlite::memory:".to_string()
    } else {
        format!("sqlite://{}?mode=rwc", path.display())
    };

    let opts = SqliteConnectOptions::from_str(&url)
        .map_err(|source| DbError::Open {
            url: url.clone(),
            source,
        })?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .map_err(|source| DbError::Open {
            url: url.clone(),
            source,
        })?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|source| DbError::Migrate { source })?;

    // Any assistant turn still marked `in_progress` at startup is an
    // orphan from a previous crash / SIGKILL — no worker is going to
    // resume it. Flip them to `errored` so the chat UI doesn't sit on a
    // forever-spinning "thinking…" bubble after a restart.
    let swept = session_core::db::sweep_in_progress_at_startup(&pool).await?;
    if swept > 0 {
        tracing::info!(
            swept,
            "chat: marked orphaned in_progress turns as errored at startup"
        );
    }

    Ok(pool)
}

/// Schema for a per-collection RAG store (`<data_dir>/<uuid>/rag.sqlite`).
/// Mirrors the `rag_files` / `rag_chunks` / `rag_chunks_fts` shapes that
/// used to live in the shared DB (migrations 0013/0014), minus the foreign
/// keys into `rag_collections` — that table lives in the *central* DB, not
/// here. Applied idempotently on every open so a fresh folder bootstraps
/// itself and an existing one is a no-op. `collection_id` columns are kept
/// (every row carries the owning id) so the query layer is identical to
/// the old shared-table code.
const COLLECTION_STORE_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS rag_files (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    collection_id INTEGER NOT NULL,
    path          TEXT NOT NULL,
    content_hash  TEXT NOT NULL,
    indexed_at    TEXT NOT NULL,
    UNIQUE (collection_id, path)
) STRICT;
CREATE TABLE IF NOT EXISTS rag_chunks (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    collection_id INTEGER NOT NULL,
    file_id       INTEGER NOT NULL,
    chunk_index   INTEGER NOT NULL,
    start_line    INTEGER NOT NULL,
    end_line      INTEGER NOT NULL,
    content       TEXT NOT NULL,
    vector_id     INTEGER NOT NULL,
    UNIQUE (collection_id, vector_id),
    FOREIGN KEY (file_id) REFERENCES rag_files(id) ON DELETE CASCADE
) STRICT;
CREATE INDEX IF NOT EXISTS idx_rag_files_collection ON rag_files (collection_id);
CREATE INDEX IF NOT EXISTS idx_rag_chunks_collection ON rag_chunks (collection_id);
CREATE INDEX IF NOT EXISTS idx_rag_chunks_file ON rag_chunks (file_id);
CREATE VIRTUAL TABLE IF NOT EXISTS rag_chunks_fts USING fts5(
    content,
    content='rag_chunks',
    content_rowid='id',
    tokenize='unicode61'
);
CREATE TRIGGER IF NOT EXISTS rag_chunks_fts_ai AFTER INSERT ON rag_chunks BEGIN
    INSERT INTO rag_chunks_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER IF NOT EXISTS rag_chunks_fts_ad AFTER DELETE ON rag_chunks BEGIN
    INSERT INTO rag_chunks_fts(rag_chunks_fts, rowid, content) VALUES ('delete', old.id, old.content);
END;
CREATE TRIGGER IF NOT EXISTS rag_chunks_fts_au AFTER UPDATE ON rag_chunks BEGIN
    INSERT INTO rag_chunks_fts(rag_chunks_fts, rowid, content) VALUES ('delete', old.id, old.content);
    INSERT INTO rag_chunks_fts(rowid, content) VALUES (new.id, new.content);
END;
"#;

/// Open (or create) a per-collection RAG store at `path`, running the
/// content DDL idempotently. Unlike [`open`] this carries no migration
/// history and no central tables — it's a standalone store keyed entirely
/// by the folder it lives in.
pub async fn open_collection_store(path: &Path) -> Result<Pool, DbError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DbError::Open {
            url: parent.display().to_string(),
            source: sqlx::Error::Io(source),
        })?;
    }
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let opts = SqliteConnectOptions::from_str(&url)
        .map_err(|source| DbError::Open {
            url: url.clone(),
            source,
        })?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .map_err(|source| DbError::Open {
            url: url.clone(),
            source,
        })?;
    // One batch — `raw_sql` runs every statement, including the FTS
    // triggers whose `BEGIN … END;` bodies contain inner semicolons that
    // a naive split-on-`;` would mangle.
    sqlx::raw_sql(COLLECTION_STORE_DDL)
        .execute(&pool)
        .await
        .map_err(DbError::Query)?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn opens_in_memory_and_runs_migrations() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        // The users table should exist after migrations.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn opens_file_path_and_creates_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gw.sqlite");
        let pool = open(&path).await.unwrap();
        assert!(path.exists());
        sqlx::query("SELECT 1").execute(&pool).await.unwrap();
    }
}
