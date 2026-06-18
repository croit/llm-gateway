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
