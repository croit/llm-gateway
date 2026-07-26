// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-conversation gateway settings — currently just the "Denkaufwand"
//! (effort) the user picked in the chat composer.
//!
//! A gateway-owned overlay on the shared `chat_sessions` table (session-core
//! owns that row and shouldn't grow gateway-specific columns), mirroring the
//! `chat_session_tools` / `chat_session_skills` pattern. A missing row means
//! "the default" — callers parse the stored string via
//! [`crate::server::reasoning::Effort::from_db`], which maps `None` to
//! `Standard`.
//!
//! Schema lives in `migrations/0024_session_settings.sql`.

use jiff::Timestamp;
use sqlx::Row;

use super::{DbError, Pool};

/// The stored effort string for a conversation, or `None` if no row exists yet
/// (the caller treats that as the default).
pub async fn get_effort(pool: &Pool, session_id: &str) -> Result<Option<String>, DbError> {
    let row = sqlx::query("SELECT effort FROM chat_session_settings WHERE session_id = ?")
        .bind(session_id)
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => Ok(r.try_get::<Option<String>, _>("effort")?),
        None => Ok(None),
    }
}

/// Persist the effort level for a conversation. Idempotent upsert.
pub async fn set_effort(pool: &Pool, session_id: &str, effort: &str) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO chat_session_settings (session_id, effort, updated_at)
           VALUES (?, ?, ?)
           ON CONFLICT(session_id) DO UPDATE SET
             effort     = excluded.effort,
             updated_at = excluded.updated_at"#,
    )
    .bind(session_id)
    .bind(effort)
    .bind(now)
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
        assert_eq!(get_effort(&pool, "s1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn set_then_read_back_and_overwrite() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        set_effort(&pool, "s1", "deep").await.unwrap();
        assert_eq!(
            get_effort(&pool, "s1").await.unwrap().as_deref(),
            Some("deep")
        );
        // Idempotent upsert overwrites.
        set_effort(&pool, "s1", "fast").await.unwrap();
        assert_eq!(
            get_effort(&pool, "s1").await.unwrap().as_deref(),
            Some("fast")
        );
    }

    #[tokio::test]
    async fn scoped_per_conversation() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        seed_session(&pool, "s2").await;
        set_effort(&pool, "s1", "max").await.unwrap();
        assert_eq!(get_effort(&pool, "s2").await.unwrap(), None);
    }
}
