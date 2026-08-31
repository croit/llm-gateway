// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! In-flight OAuth consent for a RAG source.
//!
//! One row per authorization the operator has started and not yet finished.
//! The `state` is both the CSRF token and the lookup key, and the row is
//! consumed on first use — a replayed callback finds nothing, which is the
//! point.

use jiff::{Span, Timestamp};
use sqlx::Row;

use super::{DbError, Pool, parse_ts};

/// How long an operator has to finish a consent screen before the pending row
/// stops being valid. Long enough to find the right Google account and pick a
/// folder, short enough that an abandoned attempt is not a standing
/// credential-shaped hole.
const CONSENT_TTL_MINUTES: i64 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSourceOauth {
    pub state: String,
    pub collection_id: i64,
    pub source_kind: String,
    pub pkce_verifier: String,
    pub redirect_uri: String,
    pub token_url: String,
    pub admin_user_id: String,
}

/// Record a started authorization, and sweep anything that has expired.
///
/// The sweep rides along here rather than on a timer: this table only grows
/// when someone starts a consent, so the moment one starts is exactly when
/// clearing the abandoned ones is free.
pub async fn create_pending(pool: &Pool, p: &PendingSourceOauth) -> Result<(), DbError> {
    let now = Timestamp::now();
    let expires = now + Span::new().minutes(CONSENT_TTL_MINUTES);
    sqlx::query("DELETE FROM pending_rag_oauth WHERE expires_at < ?")
        .bind(now.to_string())
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO pending_rag_oauth
           (state, collection_id, source_kind, pkce_verifier, redirect_uri, token_url,
            admin_user_id, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&p.state)
    .bind(p.collection_id)
    .bind(&p.source_kind)
    .bind(&p.pkce_verifier)
    .bind(&p.redirect_uri)
    .bind(&p.token_url)
    .bind(&p.admin_user_id)
    .bind(now.to_string())
    .bind(expires.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Consume a pending authorization: return it and delete it in one go.
///
/// Deleting on read is what makes the `state` single-use. A callback replayed
/// with the same code — by a back button, a shared link, or someone who
/// captured the redirect — finds nothing and cannot mint a second token.
/// Returns `None` for an unknown, already-used, or expired state; the caller
/// cannot tell those apart, and should not.
pub async fn take_pending(pool: &Pool, state: &str) -> Result<Option<PendingSourceOauth>, DbError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT state, collection_id, source_kind, pkce_verifier, redirect_uri, token_url,
                admin_user_id, expires_at
         FROM pending_rag_oauth WHERE state = ?",
    )
    .bind(state)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    sqlx::query("DELETE FROM pending_rag_oauth WHERE state = ?")
        .bind(state)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let expires_at = parse_ts(row.try_get::<String, _>("expires_at")?, "expires_at")?;
    if expires_at < Timestamp::now() {
        return Ok(None);
    }
    Ok(Some(PendingSourceOauth {
        state: row.try_get("state")?,
        collection_id: row.try_get("collection_id")?,
        source_kind: row.try_get("source_kind")?,
        pkce_verifier: row.try_get("pkce_verifier")?,
        redirect_uri: row.try_get("redirect_uri")?,
        token_url: row.try_get("token_url")?,
        admin_user_id: row.try_get("admin_user_id")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::open;
    use crate::server::db::rag as rag_db;
    use std::path::Path;

    async fn fresh() -> (Pool, i64) {
        let pool = open(Path::new(":memory:")).await.unwrap();
        let c = rag_db::create_collection(
            &pool,
            &rag_db::NewCollection {
                name: "drive".into(),
                description: None,
                git_url: String::new(),
                git_ref: "main".into(),
                pat: None,
                source: Default::default(),
                profile_id: None,
                extraction_model: None,
                embedding_model: "e".into(),
                include_globs: vec![],
                exclude_globs: vec![],
                chunk_size: 800,
                chunk_overlap: 100,
                search_mode: rag_db::SearchMode::Versioned,
            },
        )
        .await
        .unwrap();
        (pool, c.id)
    }

    fn sample(collection_id: i64) -> PendingSourceOauth {
        PendingSourceOauth {
            state: "st-1".into(),
            collection_id,
            source_kind: "gdrive".into(),
            pkce_verifier: "verifier".into(),
            redirect_uri: "https://gw.example.com/rag/oauth/callback".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            admin_user_id: "u1".into(),
        }
    }

    #[tokio::test]
    async fn a_pending_consent_round_trips() {
        let (pool, cid) = fresh().await;
        let p = sample(cid);
        create_pending(&pool, &p).await.unwrap();
        let got = take_pending(&pool, "st-1").await.unwrap().unwrap();
        assert_eq!(got, p);
    }

    /// The state is single-use. A replayed callback must not be able to mint
    /// a second token from the same authorization.
    #[tokio::test]
    async fn a_consumed_state_cannot_be_used_again() {
        let (pool, cid) = fresh().await;
        create_pending(&pool, &sample(cid)).await.unwrap();
        assert!(take_pending(&pool, "st-1").await.unwrap().is_some());
        assert!(
            take_pending(&pool, "st-1").await.unwrap().is_none(),
            "the row is consumed on read, so a replay finds nothing"
        );
    }

    #[tokio::test]
    async fn an_unknown_state_is_simply_absent() {
        let (pool, _) = fresh().await;
        assert!(take_pending(&pool, "never-issued").await.unwrap().is_none());
    }

    /// Deleting the collection must take its in-flight consent with it —
    /// otherwise a callback could arrive for a corpus that no longer exists.
    #[tokio::test]
    async fn deleting_the_collection_drops_its_pending_consent() {
        let (pool, cid) = fresh().await;
        create_pending(&pool, &sample(cid)).await.unwrap();
        rag_db::delete_collection(&pool, cid).await.unwrap();
        assert!(take_pending(&pool, "st-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_expired_consent_is_not_honoured_and_is_swept() {
        let (pool, cid) = fresh().await;
        let stale = Timestamp::now() - Span::new().hours(2);
        sqlx::query(
            "INSERT INTO pending_rag_oauth
               (state, collection_id, source_kind, pkce_verifier, redirect_uri, token_url,
                admin_user_id, created_at, expires_at)
             VALUES ('old', ?, 'gdrive', 'v', 'r', 't', 'u1', ?, ?)",
        )
        .bind(cid)
        .bind(stale.to_string())
        .bind(stale.to_string())
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            take_pending(&pool, "old").await.unwrap().is_none(),
            "an operator who wandered off cannot finish the flow an hour later"
        );

        // And starting a new one clears whatever else has gone stale.
        sqlx::query(
            "INSERT INTO pending_rag_oauth
               (state, collection_id, source_kind, pkce_verifier, redirect_uri, token_url,
                admin_user_id, created_at, expires_at)
             VALUES ('old2', ?, 'gdrive', 'v', 'r', 't', 'u1', ?, ?)",
        )
        .bind(cid)
        .bind(stale.to_string())
        .bind(stale.to_string())
        .execute(&pool)
        .await
        .unwrap();
        create_pending(&pool, &sample(cid)).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_rag_oauth")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 1, "the stale row was swept when a new consent began");
    }
}
