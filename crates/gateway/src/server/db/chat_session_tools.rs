// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-conversation tool enablement — the rows behind the
//! per-conversation tool overlay (tool-context-optimization Phase 1 + 3).
//!
//! A conversation-scoped layer on top of RBAC + global `user_tool_prefs`:
//! a row turns an opt-in tool *group* on for one chat session. The single
//! always-on bootstrap (`server::tools::catalog::BOOTSTRAP_TOOL_ID` =
//! `enable_tools`) needs no row; every other capability lives behind a row
//! here. `tool_key` is the catalog toggle key, so one row governs a whole
//! group (`memory`, `typst`, `mcp__<server>`, …). Rows are written either
//! by the model calling `enable_tools` explicitly (`source = "model"`) or
//! by the chat driver auto-enabling on a direct call (`source =
//! "auto-call"`).
//!
//! Schema lives in `migrations/0012_chat_session_tools.sql`.

use std::collections::HashSet;

use jiff::Timestamp;
use sqlx::Row;

use super::{DbError, Pool};

/// Set the on/off state of one tool group for one conversation. `source`
/// records *why* (`manual` | `suggested` | `auto`) for auditing the
/// router. Idempotent upsert — re-enabling an already-on group just bumps
/// `updated_at` (and refreshes `source`), so the embedding router can call
/// this every turn without churn.
pub async fn set(
    pool: &Pool,
    session_id: &str,
    tool_key: &str,
    enabled: bool,
    source: &str,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO chat_session_tools (session_id, tool_key, enabled, source, updated_at)
           VALUES (?, ?, ?, ?, ?)
           ON CONFLICT(session_id, tool_key) DO UPDATE SET
             enabled    = excluded.enabled,
             source     = excluded.source,
             updated_at = excluded.updated_at"#,
    )
    .bind(session_id)
    .bind(tool_key)
    .bind(i64::from(enabled))
    .bind(source)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// The set of tool-group keys turned **on** for this conversation. The
/// overlay in `AppState::allowed_tools_for_session` unions this with the
/// always-on core and intersects against the RBAC-granted set.
pub async fn enabled_keys_for_session(
    pool: &Pool,
    session_id: &str,
) -> Result<HashSet<String>, DbError> {
    let rows = sqlx::query(
        r#"SELECT tool_key FROM chat_session_tools
           WHERE session_id = ? AND enabled = 1"#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| r.try_get::<String, _>("tool_key").map_err(DbError::from))
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

    /// A chat_sessions row to satisfy the FK. Mirrors the columns added by
    /// migrations 0005 + 0011.
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
        assert!(
            enabled_keys_for_session(&pool, "s1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn enabling_then_reading_back() {
        let pool = fresh().await;
        seed_session(&pool, "s1").await;
        set(&pool, "s1", "typst", true, "auto").await.unwrap();
        let on = enabled_keys_for_session(&pool, "s1").await.unwrap();
        assert!(on.contains("typst"));
        assert_eq!(on.len(), 1);
    }

    #[tokio::test]
    async fn disabling_drops_it_from_enabled_set() {
        let pool = fresh().await;
        seed_session(&pool, "s1").await;
        set(&pool, "s1", "typst", true, "manual").await.unwrap();
        set(&pool, "s1", "typst", false, "manual").await.unwrap();
        assert!(
            enabled_keys_for_session(&pool, "s1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn scoped_per_conversation() {
        let pool = fresh().await;
        seed_session(&pool, "s1").await;
        seed_session(&pool, "s2").await;
        set(&pool, "s1", "typst", true, "auto").await.unwrap();
        assert!(
            enabled_keys_for_session(&pool, "s2")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
