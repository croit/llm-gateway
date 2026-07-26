// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-token tool on/off preferences — the rows behind the per-token
//! capability toggles on the `/tokens` page.
//!
//! A layer on top of the per-user grant: a row can only ever *subtract*
//! a tool the owning user's roles already grant (and that they haven't
//! already turned off globally on `/tools`). Default is enabled, so we
//! only store explicit choices and a tool with no row is on. `tool_key`
//! is the UI toggle key (the per-template `typst_<id>` tools collapse to
//! a single `typst` key — see `server::tools::catalog`).
//!
//! These rows only matter when the token's master `tools_enabled` flag
//! is on; while it's off the request path skips tool injection entirely
//! (see `RamaState::allowed_tools_for_token`).
//!
//! Schema lives in `migrations/0019_token_tool_prefs.sql`.

use std::collections::HashSet;

use jiff::Timestamp;
use sqlx::Row;

use super::{DbError, Pool};

/// Set the on/off state for one tool key on one token. Idempotent upsert
/// — re-saving the same state just bumps `updated_at`.
pub async fn set(
    pool: &Pool,
    token_id: &str,
    tool_key: &str,
    enabled: bool,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO token_tool_prefs (token_id, tool_key, enabled, updated_at)
           VALUES (?, ?, ?, ?)
           ON CONFLICT(token_id, tool_key) DO UPDATE SET
             enabled    = excluded.enabled,
             updated_at = excluded.updated_at"#,
    )
    .bind(token_id)
    .bind(tool_key)
    .bind(i64::from(enabled))
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// The set of tool keys this token has explicitly turned **off**.
/// Everything not in this set is enabled (the default). Callers subtract
/// this from the user's granted tool list at request time.
pub async fn disabled_for_token(pool: &Pool, token_id: &str) -> Result<HashSet<String>, DbError> {
    let rows = sqlx::query(
        r#"SELECT tool_key FROM token_tool_prefs
           WHERE token_id = ? AND enabled = 0"#,
    )
    .bind(token_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| r.try_get::<String, _>("tool_key").map_err(DbError::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::{open, tokens, users};
    use std::path::Path;

    /// A pool with a couple of real token rows — the `token_tool_prefs`
    /// FK requires a parent token, so prefs tests seed them first.
    async fn fresh() -> Pool {
        let pool = open(Path::new(":memory:")).await.unwrap();
        let now = jiff::Timestamp::now();
        users::upsert(
            &pool,
            &users::User {
                id: "alice".into(),
                email: "alice@example.com".into(),
                name: None,
                roles: vec![],
                created_at: now,
                updated_at: now,
                timezone: None,
            },
        )
        .await
        .unwrap();
        for id in ["tok-1", "tok-2"] {
            tokens::insert(
                &pool,
                &tokens::Token {
                    id: id.into(),
                    user_id: "alice".into(),
                    name: id.into(),
                    hash: format!("hash-{id}"),
                    created_at: now,
                    last_used_at: None,
                    expires_at: now + jiff::SignedDuration::from_hours(24),
                    revoked_at: None,
                    tools_enabled: true,
                },
            )
            .await
            .unwrap();
        }
        pool
    }

    #[tokio::test]
    async fn default_is_enabled_for_every_token() {
        let pool = fresh().await;
        // No rows → nothing disabled.
        assert!(disabled_for_token(&pool, "tok-1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn disabling_then_reading_back() {
        let pool = fresh().await;
        set(&pool, "tok-1", "rag_search", false).await.unwrap();
        let disabled = disabled_for_token(&pool, "tok-1").await.unwrap();
        assert!(disabled.contains("rag_search"));
        assert_eq!(disabled.len(), 1);
    }

    #[tokio::test]
    async fn re_enabling_drops_it_from_disabled_set() {
        let pool = fresh().await;
        set(&pool, "tok-1", "rag_search", false).await.unwrap();
        set(&pool, "tok-1", "rag_search", true).await.unwrap();
        assert!(disabled_for_token(&pool, "tok-1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prefs_are_scoped_per_token() {
        let pool = fresh().await;
        set(&pool, "tok-1", "search_web", false).await.unwrap();
        assert!(disabled_for_token(&pool, "tok-2").await.unwrap().is_empty());
    }
}
