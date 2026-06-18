// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-model sampling defaults — the rows behind `/admin/models`.
//!
//! Each row stores the raw TOML string the admin typed in the UI;
//! we round-trip it verbatim on edit so re-rendering doesn't
//! silently re-format the operator's input. Parsing happens at
//! request-merge time, with a save-time syntactic validation that
//! rejects obviously-broken submissions.
//!
//! Schema lives in `migrations/0006_model_defaults.sql`.

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{DbError, Pool};

/// One stored row, surface-exposed to the admin UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDefaults {
    pub model_name: String,
    /// Raw TOML — what the admin typed. Round-tripped verbatim on
    /// re-render. May be empty (operator cleared the textarea +
    /// hit save — equivalent to deleting the row, but tolerated).
    pub defaults_toml: String,
    pub updated_at: Timestamp,
}

fn map_row(row: &SqliteRow) -> Result<ModelDefaults, DbError> {
    let model_name: String = row.try_get("model_name")?;
    let defaults_toml: String = row.try_get("defaults_toml")?;
    let updated_at_s: String = row.try_get("updated_at")?;
    let updated_at: Timestamp = updated_at_s
        .parse()
        .map_err(|e: jiff::Error| DbError::Decode {
            column: "updated_at",
            source: e.into(),
        })?;
    Ok(ModelDefaults {
        model_name,
        defaults_toml,
        updated_at,
    })
}

/// Look up one row. `None` means "no defaults set" — callers fall
/// through to forwarding the client body verbatim.
pub async fn get(pool: &Pool, model_name: &str) -> Result<Option<ModelDefaults>, DbError> {
    let row = sqlx::query(
        r#"SELECT model_name, defaults_toml, updated_at
           FROM model_defaults
           WHERE model_name = ?"#,
    )
    .bind(model_name)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_row).transpose()
}

/// Insert or replace the row for `model_name`. The caller is
/// responsible for syntactic validation of `defaults_toml` — this
/// function only enforces the DB-level constraints (NOT NULL,
/// PRIMARY KEY).
pub async fn upsert(pool: &Pool, model_name: &str, defaults_toml: &str) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO model_defaults (model_name, defaults_toml, updated_at)
           VALUES (?, ?, ?)
           ON CONFLICT(model_name) DO UPDATE SET
             defaults_toml = excluded.defaults_toml,
             updated_at    = excluded.updated_at"#,
    )
    .bind(model_name)
    .bind(defaults_toml)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop the row entirely. No-op if it didn't exist — callers don't
/// need to pre-check.
pub async fn delete(pool: &Pool, model_name: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM model_defaults WHERE model_name = ?")
        .bind(model_name)
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
    async fn round_trip_get_after_upsert() {
        let pool = fresh().await;
        upsert(&pool, "Qwen/Qwen3-72B", "temperature = 0.7\ntop_p = 0.95")
            .await
            .unwrap();
        let row = get(&pool, "Qwen/Qwen3-72B").await.unwrap().unwrap();
        assert_eq!(row.model_name, "Qwen/Qwen3-72B");
        assert!(row.defaults_toml.contains("temperature = 0.7"));
    }

    #[tokio::test]
    async fn upsert_replaces_existing_row() {
        let pool = fresh().await;
        upsert(&pool, "m", "a = 1").await.unwrap();
        upsert(&pool, "m", "b = 2").await.unwrap();
        let row = get(&pool, "m").await.unwrap().unwrap();
        assert_eq!(row.defaults_toml, "b = 2");
    }

    #[tokio::test]
    async fn get_returns_none_when_missing() {
        let pool = fresh().await;
        assert!(get(&pool, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_is_idempotent_on_missing_row() {
        let pool = fresh().await;
        delete(&pool, "never-existed").await.unwrap();
        upsert(&pool, "m", "x = 1").await.unwrap();
        delete(&pool, "m").await.unwrap();
        assert!(get(&pool, "m").await.unwrap().is_none());
    }
}
