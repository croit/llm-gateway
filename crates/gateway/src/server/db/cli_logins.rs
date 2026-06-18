// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `cli_logins` is short-lived state for the CLI loopback-via-polling flow.
//! A row is created when the CLI hits /auth/cli/start, decorated when the
//! callback completes, drained when the CLI polls.

use jiff::Timestamp;
use sqlx::Row;

use super::{DbError, Pool};

#[derive(Debug, Clone)]
pub struct CliLogin {
    pub state: String,
    pub pkce_challenge: String,
    pub token_plain: Option<String>,
    pub expires_at: Timestamp,
    pub created_at: Timestamp,
}

pub async fn insert(pool: &Pool, login: &CliLogin) -> Result<(), DbError> {
    sqlx::query(
        r#"INSERT INTO cli_logins (state, pkce_challenge, token_plain, expires_at, created_at)
           VALUES (?, ?, ?, ?, ?)"#,
    )
    .bind(&login.state)
    .bind(&login.pkce_challenge)
    .bind(&login.token_plain)
    .bind(login.expires_at.to_string())
    .bind(login.created_at.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn find(pool: &Pool, state: &str) -> Result<Option<CliLogin>, DbError> {
    let row = sqlx::query("SELECT * FROM cli_logins WHERE state = ?")
        .bind(state)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else { return Ok(None) };

    let expires_at: String = row.try_get("expires_at")?;
    let created_at: String = row.try_get("created_at")?;
    Ok(Some(CliLogin {
        state: row.try_get("state")?,
        pkce_challenge: row.try_get("pkce_challenge")?,
        token_plain: row.try_get("token_plain")?,
        expires_at: expires_at
            .parse()
            .map_err(|e: jiff::Error| DbError::Decode {
                column: "expires_at",
                source: e.into(),
            })?,
        created_at: created_at
            .parse()
            .map_err(|e: jiff::Error| DbError::Decode {
                column: "created_at",
                source: e.into(),
            })?,
    }))
}

/// Sets the plaintext token on an existing row. Idempotent — callable from the
/// callback handler when the OIDC dance resolves successfully.
pub async fn set_token(pool: &Pool, state: &str, token_plain: &str) -> Result<bool, DbError> {
    let result = sqlx::query("UPDATE cli_logins SET token_plain = ? WHERE state = ?")
        .bind(token_plain)
        .bind(state)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Deletes the row after the CLI has retrieved the token. Best-effort.
pub async fn delete(pool: &Pool, state: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM cli_logins WHERE state = ?")
        .bind(state)
        .execute(pool)
        .await?;
    Ok(())
}

/// Deletes any rows whose `expires_at` is in the past. Called periodically by
/// a background task so abandoned login attempts don't accumulate.
pub async fn purge_expired(pool: &Pool) -> Result<u64, DbError> {
    let result = sqlx::query("DELETE FROM cli_logins WHERE expires_at < ?")
        .bind(Timestamp::now().to_string())
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture(state: &str) -> CliLogin {
        let now = Timestamp::now();
        CliLogin {
            state: state.into(),
            pkce_challenge: "challenge-hex".into(),
            token_plain: None,
            expires_at: now + jiff::SignedDuration::from_secs(300),
            created_at: now,
        }
    }

    #[tokio::test]
    async fn insert_then_find() {
        let pool = super::super::open(Path::new(":memory:")).await.unwrap();
        let cl = fixture("state-x");
        insert(&pool, &cl).await.unwrap();
        let got = find(&pool, "state-x").await.unwrap().unwrap();
        assert_eq!(got.state, "state-x");
        assert!(got.token_plain.is_none());
    }

    #[tokio::test]
    async fn set_token_and_retrieve() {
        let pool = super::super::open(Path::new(":memory:")).await.unwrap();
        insert(&pool, &fixture("state-y")).await.unwrap();
        assert!(set_token(&pool, "state-y", "gwk_xyz").await.unwrap());
        let got = find(&pool, "state-y").await.unwrap().unwrap();
        assert_eq!(got.token_plain.as_deref(), Some("gwk_xyz"));
    }

    #[tokio::test]
    async fn purge_expired_removes_stale() {
        let pool = super::super::open(Path::new(":memory:")).await.unwrap();
        let mut cl = fixture("stale");
        cl.expires_at = Timestamp::now() - jiff::SignedDuration::from_secs(1);
        insert(&pool, &cl).await.unwrap();
        insert(&pool, &fixture("fresh")).await.unwrap();
        let removed = purge_expired(&pool).await.unwrap();
        assert_eq!(removed, 1);
        assert!(find(&pool, "stale").await.unwrap().is_none());
        assert!(find(&pool, "fresh").await.unwrap().is_some());
    }
}
