// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{DbError, Pool};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Token {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub hash: String,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
    pub expires_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
    /// Master "tool use" switch for this token. Default `false` (off): a
    /// token sees gateway tools only after its owner turns this on. The
    /// per-capability `token_tool_prefs` rows only matter when this is on.
    pub tools_enabled: bool,
}

/// What the UI sees about a token — never includes `hash` or plaintext.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenView {
    pub id: String,
    pub name: String,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
    pub expires_at: Timestamp,
    pub revoked: bool,
}

impl From<Token> for TokenView {
    fn from(t: Token) -> Self {
        Self {
            id: t.id,
            name: t.name,
            created_at: t.created_at,
            last_used_at: t.last_used_at,
            expires_at: t.expires_at,
            revoked: t.revoked_at.is_some(),
        }
    }
}

fn parse_optional_ts(
    s: Option<String>,
    column: &'static str,
) -> Result<Option<Timestamp>, DbError> {
    s.map(|s| {
        s.parse().map_err(|e: jiff::Error| DbError::Decode {
            column,
            source: e.into(),
        })
    })
    .transpose()
}

fn map_row(row: &SqliteRow) -> Result<Token, DbError> {
    let id: String = row.try_get("id")?;
    let user_id: String = row.try_get("user_id")?;
    let name: String = row.try_get("name")?;
    let hash: String = row.try_get("hash")?;
    let created_at_s: String = row.try_get("created_at")?;
    let last_used_at_s: Option<String> = row.try_get("last_used_at")?;
    let expires_at_s: String = row.try_get("expires_at")?;
    let revoked_at_s: Option<String> = row.try_get("revoked_at")?;
    let tools_enabled: i64 = row.try_get("tools_enabled")?;

    Ok(Token {
        id,
        user_id,
        name,
        hash,
        created_at: created_at_s
            .parse()
            .map_err(|e: jiff::Error| DbError::Decode {
                column: "created_at",
                source: e.into(),
            })?,
        last_used_at: parse_optional_ts(last_used_at_s, "last_used_at")?,
        expires_at: expires_at_s
            .parse()
            .map_err(|e: jiff::Error| DbError::Decode {
                column: "expires_at",
                source: e.into(),
            })?,
        revoked_at: parse_optional_ts(revoked_at_s, "revoked_at")?,
        tools_enabled: tools_enabled != 0,
    })
}

pub async fn insert(pool: &Pool, t: &Token) -> Result<(), DbError> {
    sqlx::query(
        r#"INSERT INTO tokens
               (id, user_id, name, hash, created_at, expires_at, tools_enabled)
           VALUES (?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&t.id)
    .bind(&t.user_id)
    .bind(&t.name)
    .bind(&t.hash)
    .bind(t.created_at.to_string())
    .bind(t.expires_at.to_string())
    .bind(i64::from(t.tools_enabled))
    .execute(pool)
    .await?;
    Ok(())
}

/// Flip a token's master "tool use" switch. Scoped by `user_id` so a
/// caller can only ever change their own token. Returns `Ok(false)` if
/// no row matched (didn't exist or wrong owner).
pub async fn set_tools_enabled(
    pool: &Pool,
    user_id: &str,
    token_id: &str,
    enabled: bool,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        r#"UPDATE tokens
              SET tools_enabled = ?
            WHERE id = ?
              AND user_id = ?"#,
    )
    .bind(i64::from(enabled))
    .bind(token_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Look up a token by its SHA-256 hash. Returns None for missing, revoked, or
/// expired tokens — callers don't need to recheck.
pub async fn find_active_by_hash(pool: &Pool, hash: &str) -> Result<Option<Token>, DbError> {
    let row = sqlx::query(
        r#"SELECT * FROM tokens
           WHERE hash = ?
             AND revoked_at IS NULL
             AND expires_at > ?"#,
    )
    .bind(hash)
    .bind(Timestamp::now().to_string())
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_row).transpose()
}

/// All non-deleted tokens for a user, newest first. Includes revoked and
/// expired tokens — the UI shows the full history and lets users see when
/// something was revoked.
pub async fn list_for_user(pool: &Pool, user_id: &str) -> Result<Vec<Token>, DbError> {
    let rows = sqlx::query("SELECT * FROM tokens WHERE user_id = ? ORDER BY created_at DESC")
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    rows.iter().map(map_row).collect()
}

/// Marks a token revoked. Returns `Ok(false)` if the token didn't exist or
/// didn't belong to `user_id`.
pub async fn revoke(pool: &Pool, user_id: &str, token_id: &str) -> Result<bool, DbError> {
    let result = sqlx::query(
        r#"UPDATE tokens
              SET revoked_at = ?
            WHERE id = ?
              AND user_id = ?
              AND revoked_at IS NULL"#,
    )
    .bind(Timestamp::now().to_string())
    .bind(token_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Hard-deletes an already-revoked token from the table. Only deletes if
/// the token belongs to `user_id` *and* is already revoked — active tokens
/// must be revoked first so we keep the user's authentication-history
/// audit trail intact. Returns `Ok(false)` if no row matched (didn't
/// exist, wrong owner, or not yet revoked).
pub async fn delete_if_revoked(
    pool: &Pool,
    user_id: &str,
    token_id: &str,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        r#"DELETE FROM tokens
            WHERE id = ?
              AND user_id = ?
              AND revoked_at IS NOT NULL"#,
    )
    .bind(token_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Bumps `last_used_at`. Cheap, fire-and-forget. Caller decides whether to
/// debounce (e.g. only every minute per token).
pub async fn touch(pool: &Pool, token_id: &str) -> Result<(), DbError> {
    sqlx::query("UPDATE tokens SET last_used_at = ? WHERE id = ?")
        .bind(Timestamp::now().to_string())
        .bind(token_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::users;
    use super::*;
    use std::path::Path;

    async fn setup() -> (Pool, String) {
        let pool = super::super::open(Path::new(":memory:")).await.unwrap();
        let user_id = "alice".to_string();
        let now = Timestamp::now();
        users::upsert(
            &pool,
            &users::User {
                id: user_id.clone(),
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
        (pool, user_id)
    }

    fn fixture(user_id: &str, id: &str, hash: &str) -> Token {
        let now = Timestamp::now();
        Token {
            id: id.into(),
            user_id: user_id.into(),
            name: format!("token-{id}"),
            hash: hash.into(),
            created_at: now,
            last_used_at: None,
            expires_at: now + jiff::SignedDuration::from_hours(24),
            revoked_at: None,
            tools_enabled: false,
        }
    }

    #[tokio::test]
    async fn insert_then_find_by_hash() {
        let (pool, uid) = setup().await;
        let t = fixture(&uid, "t1", "hash-aaa");
        insert(&pool, &t).await.unwrap();

        let got = find_active_by_hash(&pool, "hash-aaa")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.id, "t1");
        assert!(
            find_active_by_hash(&pool, "hash-nope")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn find_active_skips_revoked() {
        let (pool, uid) = setup().await;
        let t = fixture(&uid, "t2", "hash-bbb");
        insert(&pool, &t).await.unwrap();
        assert!(revoke(&pool, &uid, "t2").await.unwrap());

        assert!(
            find_active_by_hash(&pool, "hash-bbb")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn find_active_skips_expired() {
        let (pool, uid) = setup().await;
        let now = Timestamp::now();
        let mut t = fixture(&uid, "t3", "hash-ccc");
        t.expires_at = now - jiff::SignedDuration::from_hours(1);
        insert(&pool, &t).await.unwrap();

        assert!(
            find_active_by_hash(&pool, "hash-ccc")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn revoke_is_idempotent_after_first_call() {
        let (pool, uid) = setup().await;
        let t = fixture(&uid, "t4", "hash-ddd");
        insert(&pool, &t).await.unwrap();

        assert!(revoke(&pool, &uid, "t4").await.unwrap());
        assert!(!revoke(&pool, &uid, "t4").await.unwrap()); // already revoked
    }

    #[tokio::test]
    async fn revoke_does_not_cross_users() {
        let (pool, uid) = setup().await;
        let t = fixture(&uid, "t5", "hash-eee");
        insert(&pool, &t).await.unwrap();
        assert!(!revoke(&pool, "other-user", "t5").await.unwrap());
    }

    #[tokio::test]
    async fn delete_if_revoked_refuses_active_tokens() {
        // Hard-delete is gated on `revoked_at IS NOT NULL` — active tokens
        // can't be purged in one step; the user has to revoke first. Keeps
        // the audit trail honest if a token gets stolen and the attacker
        // tries to scrub it before we notice.
        let (pool, uid) = setup().await;
        let t = fixture(&uid, "t-active", "hash-active");
        insert(&pool, &t).await.unwrap();
        assert!(!delete_if_revoked(&pool, &uid, "t-active").await.unwrap());
        // Row is still there.
        assert!(
            find_active_by_hash(&pool, "hash-active")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn delete_if_revoked_purges_revoked_token() {
        let (pool, uid) = setup().await;
        let t = fixture(&uid, "t-revoked", "hash-revoked");
        insert(&pool, &t).await.unwrap();
        assert!(revoke(&pool, &uid, "t-revoked").await.unwrap());
        assert!(delete_if_revoked(&pool, &uid, "t-revoked").await.unwrap());
        // Idempotent: second call finds nothing to delete.
        assert!(!delete_if_revoked(&pool, &uid, "t-revoked").await.unwrap());
        // And the row is gone from the list.
        let listed = list_for_user(&pool, &uid).await.unwrap();
        assert!(listed.iter().all(|t| t.id != "t-revoked"));
    }

    #[tokio::test]
    async fn delete_if_revoked_does_not_cross_users() {
        let (pool, uid) = setup().await;
        let t = fixture(&uid, "t-mine", "hash-mine");
        insert(&pool, &t).await.unwrap();
        assert!(revoke(&pool, &uid, "t-mine").await.unwrap());
        assert!(
            !delete_if_revoked(&pool, "other-user", "t-mine")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn list_returns_all_tokens_newest_first() {
        let (pool, uid) = setup().await;
        for (i, id) in ["t-a", "t-b", "t-c"].iter().enumerate() {
            let mut t = fixture(&uid, id, &format!("hash-{i}"));
            t.created_at = Timestamp::now() + jiff::SignedDuration::from_secs(i as i64);
            insert(&pool, &t).await.unwrap();
        }
        let list = list_for_user(&pool, &uid).await.unwrap();
        let ids: Vec<&str> = list.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["t-c", "t-b", "t-a"]);
    }

    #[tokio::test]
    async fn tools_enabled_defaults_off_and_round_trips() {
        let (pool, uid) = setup().await;
        // Born off.
        let mut t = fixture(&uid, "t-tools", "hash-tools");
        assert!(!t.tools_enabled);
        insert(&pool, &t).await.unwrap();
        let got = find_active_by_hash(&pool, "hash-tools")
            .await
            .unwrap()
            .unwrap();
        assert!(!got.tools_enabled, "default is off");

        // Flip on, scoped to the owner.
        assert!(
            set_tools_enabled(&pool, &uid, "t-tools", true)
                .await
                .unwrap()
        );
        let got = find_active_by_hash(&pool, "hash-tools")
            .await
            .unwrap()
            .unwrap();
        assert!(got.tools_enabled);

        // A non-owner can't flip it.
        assert!(
            !set_tools_enabled(&pool, "other-user", "t-tools", false)
                .await
                .unwrap()
        );

        // Inserting a token born-on also round-trips.
        t.id = "t-tools-on".into();
        t.hash = "hash-tools-on".into();
        t.tools_enabled = true;
        insert(&pool, &t).await.unwrap();
        assert!(
            find_active_by_hash(&pool, "hash-tools-on")
                .await
                .unwrap()
                .unwrap()
                .tools_enabled
        );
    }

    #[tokio::test]
    async fn deleting_a_token_cascades_its_tool_prefs() {
        use super::super::token_tool_prefs;
        let (pool, uid) = setup().await;
        let t = fixture(&uid, "t-casc", "hash-casc");
        insert(&pool, &t).await.unwrap();
        token_tool_prefs::set(&pool, "t-casc", "rag_search", false)
            .await
            .unwrap();
        assert!(
            !token_tool_prefs::disabled_for_token(&pool, "t-casc")
                .await
                .unwrap()
                .is_empty()
        );

        assert!(revoke(&pool, &uid, "t-casc").await.unwrap());
        assert!(delete_if_revoked(&pool, &uid, "t-casc").await.unwrap());

        // FK ON DELETE CASCADE dropped the prefs along with the token.
        assert!(
            token_tool_prefs::disabled_for_token(&pool, "t-casc")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn touch_updates_last_used_at() {
        let (pool, uid) = setup().await;
        let t = fixture(&uid, "t6", "hash-fff");
        insert(&pool, &t).await.unwrap();
        assert!(
            find_active_by_hash(&pool, "hash-fff")
                .await
                .unwrap()
                .unwrap()
                .last_used_at
                .is_none()
        );
        touch(&pool, "t6").await.unwrap();
        assert!(
            find_active_by_hash(&pool, "hash-fff")
                .await
                .unwrap()
                .unwrap()
                .last_used_at
                .is_some()
        );
    }
}
