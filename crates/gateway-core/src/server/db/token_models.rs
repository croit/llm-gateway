// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-token model allowlists — the rows behind the model picker on the
//! `/tokens` page.
//!
//! **No rows = unrestricted**, which is what every token gets by default and
//! what every token issued before migration 0060 keeps. One or more rows turns
//! the token into a strict allowlist: only those model ids, and a model added
//! to the gateway later is denied until it is added here too.
//!
//! That is deliberately not the subtractive shape used by
//! [`super::token_tool_prefs`]. A tool toggle is a convenience for the token's
//! own owner; a model allowlist is a boundary on a credential that may live in
//! someone else's CI. Storing denials would silently widen every issued token
//! the next time an operator adds a pool.
//!
//! Like the tool prefs, this can only ever *narrow*: pool `allowed_groups` are
//! resolved first, so listing a model the owning user's groups cannot reach
//! grants nothing.
//!
//! Schema lives in `migrations/0060_per_token_accounting.sql`.

use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
use sqlx::Row;

use super::{DbError, Pool};

/// Replace a token's allowlist with `models`. An empty slice clears it, which
/// restores the unrestricted default. Runs in a transaction: a half-applied
/// allowlist would silently widen or narrow the token.
pub async fn set_for_token(pool: &Pool, token_id: &str, models: &[String]) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM token_models WHERE token_id = ?")
        .bind(token_id)
        .execute(&mut *tx)
        .await?;
    // De-duplicate rather than lean on the primary key: a repeated id in the
    // form post is a UI accident, not a conflict worth failing the save over.
    let mut seen = HashSet::new();
    for m in models {
        let m = m.trim();
        if m.is_empty() || !seen.insert(m) {
            continue;
        }
        sqlx::query("INSERT INTO token_models (token_id, model, created_at) VALUES (?, ?, ?)")
            .bind(token_id)
            .bind(m)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The models this token is restricted to, or `None` when unrestricted.
///
/// `None` and `Some(empty)` are not the same thing and the distinction is the
/// whole contract: `None` means "every model the owner can reach", while an
/// empty set would mean "nothing at all". `set_for_token` never stores the
/// latter, so it cannot come back from the database — but callers still get an
/// `Option` so the unrestricted case is impossible to confuse with a
/// restriction that happens to be empty.
pub async fn for_token(pool: &Pool, token_id: &str) -> Result<Option<HashSet<String>>, DbError> {
    let rows = sqlx::query("SELECT model FROM token_models WHERE token_id = ?")
        .bind(token_id)
        .fetch_all(pool)
        .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut out = HashSet::with_capacity(rows.len());
    for r in &rows {
        out.insert(r.try_get::<String, _>("model")?);
    }
    Ok(Some(out))
}

/// Allowlists for every token owned by `user_id`, keyed by token id. Tokens
/// with no restriction are absent from the map — one query for a whole
/// `/tokens` page instead of one per row.
pub async fn for_user(pool: &Pool, user_id: &str) -> Result<HashMap<String, Vec<String>>, DbError> {
    let rows = sqlx::query(
        "SELECT tm.token_id AS token_id, tm.model AS model
           FROM token_models tm
           JOIN tokens t ON t.id = tm.token_id
          WHERE t.user_id = ?
          ORDER BY tm.model",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for r in &rows {
        out.entry(r.try_get::<String, _>("token_id")?)
            .or_default()
            .push(r.try_get::<String, _>("model")?);
    }
    Ok(out)
}

/// Allowlists for every token in the deployment, keyed by token id — the
/// admin-wide token list, again in one query.
pub async fn all(pool: &Pool) -> Result<HashMap<String, Vec<String>>, DbError> {
    let rows = sqlx::query("SELECT token_id, model FROM token_models ORDER BY model")
        .fetch_all(pool)
        .await?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for r in &rows {
        out.entry(r.try_get::<String, _>("token_id")?)
            .or_default()
            .push(r.try_get::<String, _>("model")?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::server::db::{open, tokens, users};

    /// A pool with real token rows — `token_models` has an FK onto `tokens`,
    /// so the parent has to exist before an allowlist can name it.
    async fn pool() -> Pool {
        let pool = open(std::path::Path::new(":memory:")).await.unwrap();
        let now = Timestamp::now();
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
                speech_voice: None,
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

    /// The default has to be "everything", or adding the feature would
    /// retroactively break every token already in the field.
    #[tokio::test]
    async fn a_token_with_no_rows_is_unrestricted() {
        let pool = pool().await;
        assert_eq!(for_token(&pool, "tok-1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn setting_then_clearing_restores_the_default() {
        let pool = pool().await;
        set_for_token(&pool, "tok-1", &["a".into(), "b".into()])
            .await
            .unwrap();
        let got = for_token(&pool, "tok-1").await.unwrap().unwrap();
        assert_eq!(got, HashSet::from(["a".to_string(), "b".to_string()]));

        set_for_token(&pool, "tok-1", &[]).await.unwrap();
        assert_eq!(
            for_token(&pool, "tok-1").await.unwrap(),
            None,
            "clearing the list must mean unrestricted, not deny-everything"
        );
    }

    /// A re-save replaces the list outright — the picker posts the whole set,
    /// so a model unchecked in the form has to disappear from the table.
    #[tokio::test]
    async fn saving_replaces_rather_than_merges() {
        let pool = pool().await;
        set_for_token(&pool, "tok-1", &["a".into(), "b".into()])
            .await
            .unwrap();
        set_for_token(&pool, "tok-1", &["b".into(), "c".into()])
            .await
            .unwrap();
        let got = for_token(&pool, "tok-1").await.unwrap().unwrap();
        assert_eq!(got, HashSet::from(["b".to_string(), "c".to_string()]));
    }

    /// The page-level reads must skip unrestricted tokens entirely, so a
    /// caller can tell "no allowlist" from "an allowlist that is empty".
    #[tokio::test]
    async fn the_bulk_reads_omit_unrestricted_tokens() {
        let pool = pool().await;
        set_for_token(&pool, "tok-1", &["a".into()]).await.unwrap();

        let by_user = for_user(&pool, "alice").await.unwrap();
        assert_eq!(
            by_user.get("tok-1").map(Vec::as_slice),
            Some(&["a".to_string()][..])
        );
        assert!(
            !by_user.contains_key("tok-2"),
            "tok-2 is unrestricted: {by_user:?}"
        );

        let everything = all(&pool).await.unwrap();
        assert_eq!(everything.len(), 1, "{everything:?}");
    }

    #[tokio::test]
    async fn duplicates_and_blanks_in_a_post_are_tolerated() {
        let pool = pool().await;
        set_for_token(
            &pool,
            "tok-1",
            &["a".into(), " a ".into(), "".into(), "  ".into(), "b".into()],
        )
        .await
        .unwrap();
        let got = for_token(&pool, "tok-1").await.unwrap().unwrap();
        assert_eq!(got, HashSet::from(["a".to_string(), "b".to_string()]));
    }
}
