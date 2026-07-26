// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! A tiny global key/value store for deployment-wide operator settings.
//!
//! Today it backs the per-feature default models set from `/admin/models`
//! (keys under `default_model.*`; see [`crate::server::feature_defaults`]),
//! but the table is deliberately generic so future singleton settings can
//! reuse it. Keys are opaque strings; values are stored verbatim.
//!
//! Schema lives in `migrations/0033_feature_default_models.sql`.

use jiff::Timestamp;
use sqlx::Row;

use super::{DbError, Pool};

/// Read one setting. `None` means the key was never set — callers fall
/// through to their built-in default.
pub async fn get(pool: &Pool, key: &str) -> Result<Option<String>, DbError> {
    let row = sqlx::query("SELECT value FROM app_settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    row.map(|r| r.try_get::<String, _>("value"))
        .transpose()
        .map_err(Into::into)
}

/// Insert or overwrite the value for `key`.
pub async fn set(pool: &Pool, key: &str, value: &str) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO app_settings (key, value, updated_at)
           VALUES (?, ?, ?)
           ON CONFLICT(key) DO UPDATE SET
             value      = excluded.value,
             updated_at = excluded.updated_at"#,
    )
    .bind(key)
    .bind(value)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop a setting. No-op if it didn't exist — callers don't need to
/// pre-check.
pub async fn delete(pool: &Pool, key: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM app_settings WHERE key = ?")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
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
    async fn get_returns_none_when_missing() {
        let pool = fresh().await;
        assert!(get(&pool, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_then_get_round_trips() {
        let pool = fresh().await;
        set(&pool, "default_model.chat", "glm-4.7").await.unwrap();
        assert_eq!(
            get(&pool, "default_model.chat").await.unwrap().as_deref(),
            Some("glm-4.7")
        );
    }

    #[tokio::test]
    async fn set_overwrites_existing_value() {
        let pool = fresh().await;
        set(&pool, "k", "a").await.unwrap();
        set(&pool, "k", "b").await.unwrap();
        assert_eq!(get(&pool, "k").await.unwrap().as_deref(), Some("b"));
    }

    #[tokio::test]
    async fn delete_is_idempotent_on_missing_row() {
        let pool = fresh().await;
        delete(&pool, "never-existed").await.unwrap();
        set(&pool, "k", "v").await.unwrap();
        delete(&pool, "k").await.unwrap();
        assert!(get(&pool, "k").await.unwrap().is_none());
    }
}
