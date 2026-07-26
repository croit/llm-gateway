// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-conversation skill stickiness — the rows behind the "once loaded, a
//! skill keeps applying" behaviour (Agent Skills, Phase 2).
//!
//! When the model calls `read_skill(name)` to load a skill's instructions,
//! [`record`] writes a row here. On every later turn
//! `openai_driver::build_request_context` calls [`loaded_for_session`] and
//! re-injects those skills' `SKILL.md` bodies into the system message, so the
//! guidance persists without the model re-reading it each turn. RBAC is still
//! applied at render time, so a stale row for a since-revoked skill is simply
//! filtered out — this table only remembers intent, it never widens access.
//!
//! Schema lives in `migrations/0020_chat_session_skills.sql`.

use jiff::Timestamp;
use sqlx::Row;

use super::{DbError, Pool};

/// Mark `skill_name` as loaded in `session_id`. Idempotent upsert — loading
/// an already-loaded skill just refreshes `loaded_at`, so the model calling
/// `read_skill` again on a later turn is harmless.
pub async fn record(pool: &Pool, session_id: &str, skill_name: &str) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO chat_session_skills (session_id, skill_name, loaded_at)
           VALUES (?, ?, ?)
           ON CONFLICT(session_id, skill_name) DO UPDATE SET
             loaded_at = excluded.loaded_at"#,
    )
    .bind(session_id)
    .bind(skill_name)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Unload `skill_name` from `session_id` (the user un-pinned it in the
/// composer). No-op if it wasn't loaded. The model can still re-load it later
/// via `read_skill`, and a stale row is harmless, so this is a pure
/// user-intent toggle.
pub async fn remove(pool: &Pool, session_id: &str, skill_name: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM chat_session_skills WHERE session_id = ? AND skill_name = ?")
        .bind(session_id)
        .bind(skill_name)
        .execute(pool)
        .await?;
    Ok(())
}

/// The skill names loaded in this conversation, oldest first (so re-injected
/// guidance keeps a stable order across turns — easy on the prefix cache).
pub async fn loaded_for_session(pool: &Pool, session_id: &str) -> Result<Vec<String>, DbError> {
    let rows = sqlx::query(
        r#"SELECT skill_name FROM chat_session_skills
           WHERE session_id = ?
           ORDER BY loaded_at ASC, skill_name ASC"#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| r.try_get::<String, _>("skill_name").map_err(DbError::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::open;
    use std::path::Path;

    async fn fresh() -> Pool {
        open(Path::new(":memory:")).await.unwrap()
    }

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
    async fn default_is_empty() {
        let pool = fresh().await;
        seed_session(&pool, "s1").await;
        assert!(loaded_for_session(&pool, "s1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn record_then_read_back_is_idempotent() {
        let pool = fresh().await;
        seed_session(&pool, "s1").await;
        record(&pool, "s1", "brand").await.unwrap();
        record(&pool, "s1", "brand").await.unwrap(); // idempotent
        record(&pool, "s1", "legal").await.unwrap();
        let loaded = loaded_for_session(&pool, "s1").await.unwrap();
        assert_eq!(loaded, vec!["brand".to_string(), "legal".to_string()]);
    }

    #[tokio::test]
    async fn scoped_per_conversation() {
        let pool = fresh().await;
        seed_session(&pool, "s1").await;
        seed_session(&pool, "s2").await;
        record(&pool, "s1", "brand").await.unwrap();
        assert!(loaded_for_session(&pool, "s2").await.unwrap().is_empty());
    }
}
