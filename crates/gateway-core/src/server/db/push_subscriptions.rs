// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Web Push subscriptions — one row per (user, browser) that opted in to
//! turn-complete notifications.
//!
//! A subscription is the browser-issued push endpoint plus the two RFC 8291
//! keys (`p256dh`, `auth`) needed to encrypt a payload for it. All three are
//! stored verbatim: they authorize sending TO a browser, not acting AS the
//! user, so they aren't gateway secrets. The send path
//! (`gateway_features::push`) reads a user's rows when a turn finalizes and
//! prunes any the push service reports gone.
//!
//! Schema lives in `migrations/0048_push_subscriptions.sql`.

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

use super::{DbError, Pool};

/// One browser's push subscription. `endpoint` is globally unique per
/// browser subscription and is the send target; `p256dh` + `auth` are the
/// base64url key material the payload encryption needs.
#[derive(Debug, Clone)]
pub struct PushSubscription {
    pub id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    /// UI language code captured at subscribe time (`en`/`de`/…), used to
    /// localize the notification. `None` → gateway default.
    pub lang: Option<String>,
}

fn map_row(row: &SqliteRow) -> Result<PushSubscription, DbError> {
    Ok(PushSubscription {
        id: row.try_get("id")?,
        endpoint: row.try_get("endpoint")?,
        p256dh: row.try_get("p256dh")?,
        auth: row.try_get("auth")?,
        lang: row.try_get("lang")?,
    })
}

/// Register (or refresh) a browser's subscription for `user_id`. Keyed on
/// the unique `endpoint`: a browser that re-subscribes (key rotation, or a
/// different user signing in on the same browser) upserts the row — updating
/// the owner + keys — rather than piling up duplicates.
#[allow(clippy::too_many_arguments)]
pub async fn upsert(
    pool: &Pool,
    user_id: &str,
    endpoint: &str,
    p256dh: &str,
    auth: &str,
    lang: Option<&str>,
    user_agent: Option<&str>,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        r#"INSERT INTO push_subscriptions
               (id, user_id, endpoint, p256dh, auth, lang, user_agent, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT(endpoint) DO UPDATE SET
               user_id    = excluded.user_id,
               p256dh     = excluded.p256dh,
               auth       = excluded.auth,
               lang       = excluded.lang,
               user_agent = excluded.user_agent,
               updated_at = excluded.updated_at"#,
    )
    .bind(&id)
    .bind(user_id)
    .bind(endpoint)
    .bind(p256dh)
    .bind(auth)
    .bind(lang)
    .bind(user_agent)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// All of a user's subscriptions — the fan-out set when one of their turns
/// finalizes.
pub async fn list_for_user(pool: &Pool, user_id: &str) -> Result<Vec<PushSubscription>, DbError> {
    let rows = sqlx::query(
        r#"SELECT id, endpoint, p256dh, auth, lang FROM push_subscriptions WHERE user_id = ?"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(map_row).collect()
}

/// Remove a subscription by its endpoint, scoped to its owner — the explicit
/// "disable notifications" path (the browser unsubscribed and told us its
/// endpoint). Scoping by `user_id` means one user can't unsubscribe another's
/// device even if they learn its endpoint. No-op if it wasn't there.
pub async fn delete_by_endpoint(pool: &Pool, user_id: &str, endpoint: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM push_subscriptions WHERE user_id = ? AND endpoint = ?")
        .bind(user_id)
        .bind(endpoint)
        .execute(pool)
        .await?;
    Ok(())
}

/// Remove a subscription by id — used to prune a row the push service
/// reported gone (404/410) at send time. No-op if already deleted.
pub async fn delete(pool: &Pool, id: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM push_subscriptions WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether the user has at least one subscription — backs the `/tokens`
/// notifications card's "already enabled on some device" hint.
pub async fn count_for_user(pool: &Pool, user_id: &str) -> Result<i64, DbError> {
    let row = sqlx::query("SELECT COUNT(*) AS n FROM push_subscriptions WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok(row.try_get::<i64, _>("n")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::{open, users};
    use std::path::Path;

    async fn seed_user(pool: &Pool, id: &str) {
        let now = Timestamp::now();
        users::upsert(
            pool,
            &users::User {
                id: id.into(),
                email: format!("{id}@example.com"),
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
    }

    #[tokio::test]
    async fn upsert_lists_and_deletes() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_user(&pool, "alice").await;
        upsert(
            &pool,
            "alice",
            "https://push.example.com/a",
            "p1",
            "a1",
            Some("de"),
            Some("UA"),
        )
        .await
        .unwrap();
        let subs = list_for_user(&pool, "alice").await.unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].endpoint, "https://push.example.com/a");
        assert_eq!(subs[0].lang.as_deref(), Some("de"));
        assert_eq!(count_for_user(&pool, "alice").await.unwrap(), 1);

        // Scoped to the owner: another user's delete is a no-op…
        delete_by_endpoint(&pool, "bob", "https://push.example.com/a")
            .await
            .unwrap();
        assert_eq!(count_for_user(&pool, "alice").await.unwrap(), 1);
        // …the owner's delete removes it.
        delete_by_endpoint(&pool, "alice", "https://push.example.com/a")
            .await
            .unwrap();
        assert!(list_for_user(&pool, "alice").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn re_subscribe_upserts_on_endpoint_and_can_reassign_owner() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_user(&pool, "alice").await;
        seed_user(&pool, "bob").await;
        upsert(
            &pool,
            "alice",
            "https://push.example.com/shared",
            "p1",
            "a1",
            None,
            None,
        )
        .await
        .unwrap();
        // Same browser endpoint, different user + rotated keys → one row, reassigned.
        upsert(
            &pool,
            "bob",
            "https://push.example.com/shared",
            "p2",
            "a2",
            None,
            None,
        )
        .await
        .unwrap();
        assert!(list_for_user(&pool, "alice").await.unwrap().is_empty());
        let bob = list_for_user(&pool, "bob").await.unwrap();
        assert_eq!(bob.len(), 1);
        assert_eq!(bob[0].p256dh, "p2");

        delete(&pool, &bob[0].id).await.unwrap();
        assert_eq!(count_for_user(&pool, "bob").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn deleting_the_user_cascades_subscriptions() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        seed_user(&pool, "alice").await;
        upsert(
            &pool,
            "alice",
            "https://push.example.com/a",
            "p",
            "a",
            None,
            None,
        )
        .await
        .unwrap();
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind("alice")
            .execute(&pool)
            .await
            .unwrap();
        assert!(list_for_user(&pool, "alice").await.unwrap().is_empty());
    }
}
