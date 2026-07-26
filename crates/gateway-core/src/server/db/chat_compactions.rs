// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-conversation compaction state — the summary that stands in for a
//! session's oldest turns on upstream replay.
//!
//! A gateway-owned overlay on the shared `chat_sessions` table (session-core
//! owns that row and shouldn't grow gateway-specific columns), mirroring the
//! `chat_session_settings` pattern. A missing row means "never compacted";
//! `openai_driver::run_one_turn` then replays the whole history as before.
//!
//! When a row exists, replay replaces every turn with `seq <= up_to_seq` by
//! the `summary` (as one system message) and sends turns with a greater seq
//! verbatim. Re-compaction UPDATEs the row in place, folding the previous
//! summary plus the newly-aged turns into a fresh summary and bumping
//! `up_to_seq`.
//!
//! Schema lives in `migrations/0032_chat_compaction.sql`.

use jiff::Timestamp;
use sqlx::Row;

use super::{DbError, Pool};

/// One session's compaction overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    /// Highest turn `seq` the summary covers. Turns with `seq <= up_to_seq`
    /// are replaced by the summary on replay; greater seqs go verbatim.
    pub up_to_seq: i64,
    /// The LLM-generated summary of the folded turns.
    pub summary: String,
    pub tokens_before: Option<i64>,
    pub tokens_after: Option<i64>,
}

/// Fetch the compaction overlay for a session, or `None` if it has never been
/// compacted.
pub async fn get(pool: &Pool, session_id: &str) -> Result<Option<Compaction>, DbError> {
    let row = sqlx::query(
        r#"SELECT up_to_seq, summary, tokens_before, tokens_after
           FROM chat_compactions
           WHERE session_id = ?"#,
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => Ok(Some(Compaction {
            up_to_seq: r.try_get("up_to_seq")?,
            summary: r.try_get("summary")?,
            tokens_before: r.try_get("tokens_before")?,
            tokens_after: r.try_get("tokens_after")?,
        })),
        None => Ok(None),
    }
}

/// Insert or replace a session's compaction overlay. Idempotent upsert keyed on
/// `session_id`, so the first compaction inserts and every re-compaction
/// overwrites in place (bumping `up_to_seq` and replacing the summary).
pub async fn upsert(
    pool: &Pool,
    session_id: &str,
    up_to_seq: i64,
    summary: &str,
    tokens_before: Option<i64>,
    tokens_after: Option<i64>,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO chat_compactions
             (session_id, up_to_seq, summary, tokens_before, tokens_after, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT(session_id) DO UPDATE SET
             up_to_seq     = excluded.up_to_seq,
             summary       = excluded.summary,
             tokens_before = excluded.tokens_before,
             tokens_after  = excluded.tokens_after,
             updated_at    = excluded.updated_at"#,
    )
    .bind(session_id)
    .bind(up_to_seq)
    .bind(summary)
    .bind(tokens_before)
    .bind(tokens_after)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
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

    #[tokio::test]
    async fn default_is_none() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        assert_eq!(get(&pool, "s1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn upsert_then_read_back_and_recompact() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        upsert(&pool, "s1", 4, "first summary", Some(9000), Some(200))
            .await
            .unwrap();
        let c = get(&pool, "s1").await.unwrap().unwrap();
        assert_eq!(c.up_to_seq, 4);
        assert_eq!(c.summary, "first summary");
        assert_eq!(c.tokens_before, Some(9000));

        // Re-compaction overwrites in place and bumps the cutoff.
        upsert(&pool, "s1", 10, "second summary", Some(9500), Some(250))
            .await
            .unwrap();
        let c = get(&pool, "s1").await.unwrap().unwrap();
        assert_eq!(c.up_to_seq, 10);
        assert_eq!(c.summary, "second summary");
    }

    #[tokio::test]
    async fn scoped_per_conversation() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        seed_session(&pool, "s2").await;
        upsert(&pool, "s1", 2, "x", None, None).await.unwrap();
        assert_eq!(get(&pool, "s2").await.unwrap(), None);
    }
}
