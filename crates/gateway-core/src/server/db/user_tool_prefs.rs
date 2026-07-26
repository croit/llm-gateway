// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-user tool on/off preferences — the rows behind the `/tools`
//! page.
//!
//! A personal layer on top of RBAC: a row can only ever *subtract* a
//! tool the user's roles already grant. Default is enabled, so we only
//! store explicit choices and a tool with no row is on. `tool_key` is
//! the UI toggle key (the per-template `typst_<id>` tools collapse to a
//! single `typst` key — see `server::tools::catalog`).
//!
//! Schema lives in `migrations/0007_user_tool_prefs.sql`.

use std::collections::HashSet;

use jiff::Timestamp;
use sqlx::Row;

use super::{DbError, Pool};

/// Set the on/off state for one tool key for one user. Idempotent
/// upsert — re-saving the same state just bumps `updated_at`.
pub async fn set(pool: &Pool, user_id: &str, tool_key: &str, enabled: bool) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO user_tool_prefs (user_id, tool_key, enabled, updated_at)
           VALUES (?, ?, ?, ?)
           ON CONFLICT(user_id, tool_key) DO UPDATE SET
             enabled    = excluded.enabled,
             updated_at = excluded.updated_at"#,
    )
    .bind(user_id)
    .bind(tool_key)
    .bind(i64::from(enabled))
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// The set of tool keys this user has explicitly turned **off**.
/// Everything not in this set is enabled (the default). Callers
/// subtract this from the RBAC-granted tool list at request time.
pub async fn disabled_for_user(pool: &Pool, user_id: &str) -> Result<HashSet<String>, DbError> {
    let rows = sqlx::query(
        r#"SELECT tool_key FROM user_tool_prefs
           WHERE user_id = ? AND enabled = 0"#,
    )
    .bind(user_id)
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

    #[tokio::test]
    async fn default_is_enabled_for_everyone() {
        let pool = fresh().await;
        // No rows → nothing disabled.
        assert!(disabled_for_user(&pool, "alice").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn disabling_then_reading_back() {
        let pool = fresh().await;
        set(&pool, "alice", "search_web", false).await.unwrap();
        let disabled = disabled_for_user(&pool, "alice").await.unwrap();
        assert!(disabled.contains("search_web"));
        assert_eq!(disabled.len(), 1);
    }

    #[tokio::test]
    async fn re_enabling_drops_it_from_disabled_set() {
        let pool = fresh().await;
        set(&pool, "alice", "search_web", false).await.unwrap();
        set(&pool, "alice", "search_web", true).await.unwrap();
        assert!(disabled_for_user(&pool, "alice").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prefs_are_scoped_per_user() {
        let pool = fresh().await;
        set(&pool, "alice", "fetch_url", false).await.unwrap();
        assert!(disabled_for_user(&pool, "bob").await.unwrap().is_empty());
    }
}
