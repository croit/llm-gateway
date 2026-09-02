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

pub mod app_settings;
pub mod audit;
pub mod chat_compactions;
pub mod chat_session_settings;
pub mod chat_session_skills;
pub mod chat_session_tools;
pub mod documents;
pub mod gateway_groups;
pub mod limits;
pub mod mcp_audit;
pub mod mcp_catalog;
pub mod model_defaults;
pub mod ocr_derivatives;
pub mod push_subscriptions;
pub mod rag;
pub mod rag_documents;
pub mod rag_oauth;
pub mod skill_grants;
pub mod token_tool_prefs;
pub mod tokens;
pub mod upstreams_config;
pub mod usage;
pub mod user_mcp;
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
    #[error("running migrations: {source}")]
    Migrate {
        #[source]
        source: sqlx::migrate::MigrateError,
    },
    #[error("query: {0}")]
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
    /// Sealing/unsealing an at-rest secret (e.g. a backend API key) failed.
    #[error("crypto")]
    Crypto(#[from] crate::server::crypto::CryptoError),
}

/// Parse a stored timestamp string from column `column`, mapping a parse
/// failure to [`DbError::Decode`]. The single home for timestamp decoding
/// across the gateway's `db` submodules — mirrors the same helper in
/// `session-core::db`, but returns the gateway's own [`DbError`] (the two
/// crates have distinct error types, so the helper can't be shared directly).
pub(crate) fn parse_ts(s: String, column: &'static str) -> Result<jiff::Timestamp, DbError> {
    s.parse().map_err(|e: jiff::Error| DbError::Decode {
        column,
        source: e.into(),
    })
}

/// [`parse_ts`] for a nullable column: `None` stays `None`, `Some` is parsed.
pub(crate) fn parse_optional_ts(
    s: Option<String>,
    column: &'static str,
) -> Result<Option<jiff::Timestamp>, DbError> {
    s.map(|s| parse_ts(s, column)).transpose()
}

/// Opens (or creates) a SQLite database at `path` and runs migrations.
///
/// Pass `:memory:` to use an in-memory database. Used by tests.
pub async fn open(path: &Path) -> Result<Pool, DbError> {
    let pool = connect(path, true).await?;

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

/// Open a database that **another process owns**, for an out-of-band CLI
/// command (today: `gateway restore-setup`).
///
/// Deliberately not [`open`]: that runs migrations and, worse, sweeps every
/// `in_progress` assistant turn to `errored` on the assumption that it is the
/// server starting up and those turns are crash orphans. Against a *live*
/// gateway both assumptions are wrong — the turns belong to workers that are
/// streaming right now, and erroring them would turn every user's "thinking"
/// bubble into a failure just because an operator ran a maintenance command.
///
/// It also refuses to create a missing file, so pointing the command at the
/// wrong path reports that instead of silently making an empty database.
pub async fn attach(path: &Path) -> Result<Pool, DbError> {
    connect(path, false).await
}

/// The connection half shared by [`open`] and [`attach`]. WAL means a second
/// process can write one row while the server is serving.
async fn connect(path: &Path, create_if_missing: bool) -> Result<Pool, DbError> {
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
        .create_if_missing(create_if_missing)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true);

    SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .map_err(|source| DbError::Open { url, source })
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
    -- Deep link into the source's own UI, so an answer can be checked
    -- against the original rather than taken on trust. NULL for sources
    -- that have no per-file URL (a git clone).
    web_url       TEXT,
    -- Provider-native stable id, where the source has one. Lets an
    -- incremental sync recognise a moved file instead of re-extracting it.
    remote_id     TEXT,
    -- The source's change token (etag, rev, …) when this file was indexed.
    -- Compared for equality against the current one; never parsed.
    source_version TEXT,
    UNIQUE (collection_id, path)
) STRICT;

-- One row per document that carried a profile extraction. Separate from
-- `rag_files` because extraction is optional and its shape is the profile's,
-- not the file's.
CREATE TABLE IF NOT EXISTS rag_documents (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id         INTEGER NOT NULL,
    title           TEXT,
    summary         TEXT,
    -- Which rung of the extraction ladder produced the text (text,
    -- pdf_text_layer, ocr, office). An answer sourced from an OCR guess
    -- deserves more caution than one from a clean text layer.
    extractor       TEXT NOT NULL,
    pages_total     INTEGER,
    pages_processed INTEGER,
    extracted_at    TEXT NOT NULL,
    UNIQUE (file_id),
    FOREIGN KEY (file_id) REFERENCES rag_files(id) ON DELETE CASCADE
) STRICT;

-- Entity-attribute-value rather than a wide table: the field set belongs to
-- the profile, so a wide table would need a migration per operator edit.
-- Three typed columns so ordering and range filters use an index instead of
-- SQLite's text collation — `total_gross > 1000` and `ORDER BY doc_date DESC`
-- are the whole point of this table.
CREATE TABLE IF NOT EXISTS rag_doc_fields (
    doc_id      INTEGER NOT NULL,
    key         TEXT NOT NULL,
    value_text  TEXT,
    value_num   REAL,
    value_date  TEXT,
    PRIMARY KEY (doc_id, key),
    FOREIGN KEY (doc_id) REFERENCES rag_documents(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS idx_doc_fields_text ON rag_doc_fields (key, value_text);
CREATE INDEX IF NOT EXISTS idx_doc_fields_num  ON rag_doc_fields (key, value_num);
CREATE INDEX IF NOT EXISTS idx_doc_fields_date ON rag_doc_fields (key, value_date);
CREATE INDEX IF NOT EXISTS idx_rag_documents_file ON rag_documents (file_id);
CREATE TABLE IF NOT EXISTS rag_chunks (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    collection_id INTEGER NOT NULL,
    file_id       INTEGER NOT NULL,
    chunk_index   INTEGER NOT NULL,
    -- Where this chunk sits in its document, and in what unit. Source code
    -- is cited by line; a PDF or a scan by page, where line numbers are
    -- meaningless and a page is what a human can actually check against.
    -- 'line' | 'page'.
    loc_kind      TEXT NOT NULL,
    loc_from      INTEGER NOT NULL,
    loc_to        INTEGER NOT NULL,
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
    upgrade_collection_store(&pool).await?;
    Ok(pool)
}

/// Schema version of a per-collection store.
///
/// Bump this and add a step to [`STORE_UPGRADES`] whenever the store's shape
/// changes.
const STORE_SCHEMA_VERSION: i32 = 1;

/// Ordered upgrade steps, applied to a store whose `user_version` is below
/// [`STORE_SCHEMA_VERSION`]. Index `n` upgrades version `n` to `n + 1`.
///
/// `CREATE TABLE IF NOT EXISTS` covers a *new* store but cannot add a column
/// to an existing one, and store folders now persist across builds so that
/// every re-index can be incremental. Without this list, the first schema
/// change after that would silently do nothing on every existing collection
/// and the failure would surface as a baffling `ColumnNotFound` at query
/// time.
const STORE_UPGRADES: &[&[&str]] = &[
    // 0 → 1: everything this release added to an existing store.
    //
    // A store written by an older binary has `rag_chunks.start_line/end_line`
    // and no `web_url` / `remote_id` / `source_version`. The base DDL above
    // is `CREATE TABLE IF NOT EXISTS`, so it does nothing to those stores —
    // and every query in `rag.rs` now selects the new columns. Without this
    // step, the first search after deploying fails with `no such column` on
    // every collection indexed by the old binary.
    //
    // Line provenance is backfilled rather than dropped: an existing code
    // collection keeps citing lines and needs no re-index.
    &[
        "ALTER TABLE rag_files ADD COLUMN web_url TEXT;",
        "ALTER TABLE rag_files ADD COLUMN remote_id TEXT;",
        "ALTER TABLE rag_files ADD COLUMN source_version TEXT;",
        "ALTER TABLE rag_chunks ADD COLUMN loc_kind TEXT NOT NULL DEFAULT 'line';",
        "ALTER TABLE rag_chunks ADD COLUMN loc_from INTEGER NOT NULL DEFAULT 0;",
        "ALTER TABLE rag_chunks ADD COLUMN loc_to INTEGER NOT NULL DEFAULT 0;",
        "UPDATE rag_chunks SET loc_from = start_line, loc_to = end_line;",
    ],
];

async fn upgrade_collection_store(pool: &Pool) -> Result<(), DbError> {
    let current: i32 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await
        .map_err(DbError::Query)?;
    if current >= STORE_SCHEMA_VERSION {
        return Ok(());
    }
    for step in STORE_UPGRADES.iter().skip(current.max(0) as usize) {
        for stmt in *step {
            // Two shapes of "this statement does not apply to this store"
            // are expected and benign, because one step has to cover both a
            // brand-new store (already the new shape, from the DDL above)
            // and an old one (still the previous shape):
            //
            //   * duplicate column name — the DDL already created it;
            //   * no such column — the backfill's *source* column does not
            //     exist, because this store was never in the old shape.
            //
            // Anything else is a real failure and propagates.
            match sqlx::raw_sql(stmt).execute(pool).await {
                Ok(_) => {}
                Err(err)
                    if err.to_string().contains("duplicate column name")
                        || err.to_string().contains("no such column") => {}
                Err(err) => return Err(DbError::Query(err)),
            }
        }
    }
    sqlx::query(&format!("PRAGMA user_version = {STORE_SCHEMA_VERSION}"))
        .execute(pool)
        .await
        .map_err(DbError::Query)?;
    Ok(())
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
    async fn attach_leaves_in_flight_turns_alone() {
        // `open` sweeps `in_progress` turns to `errored`, which is right for a
        // server starting up (they are crash orphans) and catastrophic for a
        // CLI command run against a LIVE gateway: every user streaming a reply
        // would watch it turn into an error because an operator ran
        // `restore-setup`. `attach` is the connection that must not do that.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.sqlite");

        let server = open(&path).await.unwrap();
        let now = jiff::Timestamp::now();
        users::upsert(
            &server,
            &users::User {
                id: "u1".into(),
                email: "u1@example.com".into(),
                name: None,
                roles: vec![],
                created_at: now,
                updated_at: now,
                timezone: None,
                speech_voice: None,
            },
        )
        .await
        .unwrap();
        let session = session_core::db::create_session(&server, "u1")
            .await
            .unwrap();
        let live =
            session_core::db::create_assistant_turn_in_progress(&server, &session.id, "a1", "m")
                .await
                .unwrap();

        let cli = attach(&path).await.unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM chat_turns WHERE id = ?")
            .bind(&live.id)
            .fetch_one(&cli)
            .await
            .unwrap();
        assert_eq!(
            status, "in_progress",
            "attaching must not error a turn that is still streaming"
        );
    }

    #[tokio::test]
    async fn attach_refuses_to_create_a_missing_database() {
        // Pointing the CLI at the wrong path must say so, not silently make an
        // empty database and then report a gateway with no users.
        let dir = tempfile::tempdir().unwrap();
        assert!(attach(&dir.path().join("nope.sqlite")).await.is_err());
    }

    /// A store written by the previous release must survive the upgrade.
    ///
    /// This is the regression for the worst kind of bug this schema can have:
    /// `CREATE TABLE IF NOT EXISTS` silently does nothing to an existing
    /// store, so without a real upgrade step the first search after deploying
    /// fails with `no such column` on every collection indexed by the old
    /// binary.
    #[tokio::test]
    async fn an_old_store_is_upgraded_in_place_and_keeps_its_line_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rag.sqlite");

        // Build exactly what the previous release wrote — from scratch, so
        // none of the new DDL ever touches this file. That includes the FTS
        // index and its triggers: the backfill below updates `rag_chunks`,
        // which fires the update trigger, and fts5's `'delete'` command
        // errors with "database disk image is malformed" on a row that was
        // never indexed. A real old store has them, so the fixture must too.
        {
            let url = format!("sqlite://{}?mode=rwc", path.display());
            let opts = SqliteConnectOptions::from_str(&url)
                .unwrap()
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal);
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await
                .unwrap();
            sqlx::raw_sql(
                "CREATE TABLE rag_files (
                     id INTEGER PRIMARY KEY AUTOINCREMENT, collection_id INTEGER NOT NULL,
                     path TEXT NOT NULL, content_hash TEXT NOT NULL, indexed_at TEXT NOT NULL,
                     UNIQUE (collection_id, path)) STRICT;
                 CREATE TABLE rag_chunks (
                     id INTEGER PRIMARY KEY AUTOINCREMENT, collection_id INTEGER NOT NULL,
                     file_id INTEGER NOT NULL, chunk_index INTEGER NOT NULL,
                     start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
                     content TEXT NOT NULL, vector_id INTEGER NOT NULL,
                     UNIQUE (collection_id, vector_id)) STRICT;
                 CREATE VIRTUAL TABLE rag_chunks_fts USING fts5(
                     content, content='rag_chunks', content_rowid='id',
                     tokenize='unicode61');
                 CREATE TRIGGER rag_chunks_fts_ai AFTER INSERT ON rag_chunks BEGIN
                     INSERT INTO rag_chunks_fts(rowid, content) VALUES (new.id, new.content);
                 END;
                 CREATE TRIGGER rag_chunks_fts_ad AFTER DELETE ON rag_chunks BEGIN
                     INSERT INTO rag_chunks_fts(rag_chunks_fts, rowid, content)
                         VALUES ('delete', old.id, old.content);
                 END;
                 CREATE TRIGGER rag_chunks_fts_au AFTER UPDATE ON rag_chunks BEGIN
                     INSERT INTO rag_chunks_fts(rag_chunks_fts, rowid, content)
                         VALUES ('delete', old.id, old.content);
                     INSERT INTO rag_chunks_fts(rowid, content) VALUES (new.id, new.content);
                 END;
                 INSERT INTO rag_files (collection_id, path, content_hash, indexed_at)
                     VALUES (1, 'src/main.rs', 'h', '2026-01-01T00:00:00Z');
                 INSERT INTO rag_chunks
                     (collection_id, file_id, chunk_index, start_line, end_line, content, vector_id)
                     VALUES (1, 1, 0, 12, 30, 'fn main() {}', 1);",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        // Re-opening runs the upgrade.
        let pool = open_collection_store(&path).await.unwrap();
        let (kind, from, to): (String, i64, i64) =
            sqlx::query_as("SELECT loc_kind, loc_from, loc_to FROM rag_chunks")
                .fetch_one(&pool)
                .await
                .expect("the new provenance columns exist after the upgrade");
        assert_eq!(kind, "line");
        assert_eq!(
            (from, to),
            (12, 30),
            "existing line provenance was backfilled, not thrown away — an old \
             code collection keeps working without a re-index"
        );
        // The columns every query now selects are present.
        let _: (Option<String>, Option<String>, Option<String>) =
            sqlx::query_as("SELECT web_url, remote_id, source_version FROM rag_files")
                .fetch_one(&pool)
                .await
                .expect("the new file columns exist after the upgrade");
        let version: i32 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, STORE_SCHEMA_VERSION);

        // The backfill UPDATE runs through the FTS triggers, so the lexical
        // half of hybrid search has to still answer afterwards.
        let hits: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM rag_chunks_fts WHERE rag_chunks_fts MATCH 'main'",
        )
        .fetch_one(&pool)
        .await
        .expect("the FTS index survived the upgrade");
        assert_eq!(hits, 1, "the existing chunk is still lexically searchable");
    }

    /// The upgrade must be a no-op on a store that is already current,
    /// including on every subsequent open.
    #[tokio::test]
    async fn upgrading_a_fresh_store_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rag.sqlite");
        for _ in 0..3 {
            let pool = open_collection_store(&path).await.unwrap();
            sqlx::query("SELECT loc_kind FROM rag_chunks LIMIT 0")
                .execute(&pool)
                .await
                .expect("the schema is intact");
            pool.close().await;
        }
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
