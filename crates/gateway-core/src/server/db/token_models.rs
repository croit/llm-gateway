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
//! There are two independent lists — the owner's (set at `/tokens`) and the
//! operator's (set at `/admin/tokens`) — and the effective allowlist is their
//! intersection, so each side may only ever narrow. See migration 0061 for
//! why a set needs two lists where a quota needed only an author.
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

use super::limits::ManagedBy;
use super::{DbError, Pool};

/// Replace one author's list for a token. An empty slice clears it, which
/// restores that side to "no opinion" — and the unrestricted default when the
/// other side has no rows either. Runs in a transaction: a half-applied list
/// would silently widen or narrow the token.
pub async fn set_for_token(
    pool: &Pool,
    token_id: &str,
    models: &[String],
    managed_by: ManagedBy,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    let mut tx = pool.begin().await?;
    // Scoped to this author: clearing the owner's list must not touch the
    // operator's, or self-service would be a way out of an admin restriction.
    sqlx::query("DELETE FROM token_models WHERE token_id = ? AND managed_by = ?")
        .bind(token_id)
        .bind(managed_by.as_str())
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
        sqlx::query(
            "INSERT INTO token_models (token_id, model, managed_by, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(token_id)
        .bind(m)
        .bind(managed_by.as_str())
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Both authors' lists for a token, each `None` when that side has no rows.
/// The editors need them apart; everything else wants [`for_token`].
pub async fn lists_for_token(pool: &Pool, token_id: &str) -> Result<TokenModelLists, DbError> {
    let rows =
        sqlx::query("SELECT model, managed_by FROM token_models WHERE token_id = ? ORDER BY model")
            .bind(token_id)
            .fetch_all(pool)
            .await?;
    let mut owner: Vec<String> = Vec::new();
    let mut admin: Vec<String> = Vec::new();
    for r in &rows {
        let m: String = r.try_get("model")?;
        match ManagedBy::parse(&r.try_get::<String, _>("managed_by")?) {
            ManagedBy::Owner => owner.push(m),
            ManagedBy::Admin => admin.push(m),
        }
    }
    Ok(TokenModelLists {
        owner: (!owner.is_empty()).then_some(owner),
        admin: (!admin.is_empty()).then_some(admin),
    })
}

/// The two lists behind a token's allowlist. `None` means that author has set
/// nothing, which is not the same as an empty list — see [`for_token`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenModelLists {
    pub owner: Option<Vec<String>>,
    pub admin: Option<Vec<String>>,
}

impl TokenModelLists {
    /// The allowlist actually enforced: the intersection of whichever lists
    /// exist, or `None` when neither does.
    ///
    /// Intersection rather than union is the whole point — two authors who can
    /// each only narrow. A union would let an owner re-grant a model the
    /// operator had just removed.
    pub fn effective(&self) -> Option<HashSet<String>> {
        match (&self.owner, &self.admin) {
            (None, None) => None,
            (Some(l), None) | (None, Some(l)) => Some(l.iter().cloned().collect()),
            (Some(o), Some(a)) => {
                let admin: HashSet<&String> = a.iter().collect();
                Some(o.iter().filter(|m| admin.contains(m)).cloned().collect())
            }
        }
    }
}

/// The models this token is restricted to, or `None` when unrestricted.
///
/// `None` and `Some(empty)` are not the same thing and the distinction is the
/// whole contract: `None` means "every model the owner can reach", while an
/// empty set means "nothing at all". An empty set is now reachable — two
/// lists that do not overlap intersect to nothing — and it means exactly what
/// it says: the two authors agree on no model, so the token routes nowhere.
pub async fn for_token(pool: &Pool, token_id: &str) -> Result<Option<HashSet<String>>, DbError> {
    Ok(lists_for_token(pool, token_id).await?.effective())
}

/// Effective allowlists for every token owned by `user_id`, keyed by token id.
/// Tokens with no restriction are absent from the map — one query for a whole
/// `/tokens` page instead of one per row.
pub async fn for_user(pool: &Pool, user_id: &str) -> Result<HashMap<String, Vec<String>>, DbError> {
    let rows = sqlx::query(
        "SELECT tm.token_id AS token_id, tm.model AS model, tm.managed_by AS managed_by
           FROM token_models tm
           JOIN tokens t ON t.id = tm.token_id
          WHERE t.user_id = ?
          ORDER BY tm.model",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    fold_effective(&rows)
}

/// Effective allowlists for every token in the deployment — the admin-wide
/// token list, again in one query.
pub async fn all(pool: &Pool) -> Result<HashMap<String, Vec<String>>, DbError> {
    let rows = sqlx::query("SELECT token_id, model, managed_by FROM token_models ORDER BY model")
        .fetch_all(pool)
        .await?;
    fold_effective(&rows)
}

/// Both authors' lists for every token owned by `user_id` — what the owner's
/// editor renders from. One query for a whole `/tokens` page.
pub async fn lists_for_user(
    pool: &Pool,
    user_id: &str,
) -> Result<HashMap<String, TokenModelLists>, DbError> {
    let rows = sqlx::query(
        "SELECT tm.token_id AS token_id, tm.model AS model, tm.managed_by AS managed_by
           FROM token_models tm
           JOIN tokens t ON t.id = tm.token_id
          WHERE t.user_id = ?
          ORDER BY tm.model",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    fold_lists(&rows)
}

/// Both authors' lists for every token in the deployment, keyed by token id —
/// what the admin editor renders from.
pub async fn lists_all(pool: &Pool) -> Result<HashMap<String, TokenModelLists>, DbError> {
    let rows = sqlx::query("SELECT token_id, model, managed_by FROM token_models ORDER BY model")
        .fetch_all(pool)
        .await?;
    fold_lists(&rows)
}

/// Group `(token_id, model, managed_by)` rows into per-token list pairs.
fn fold_lists(
    rows: &[sqlx::sqlite::SqliteRow],
) -> Result<HashMap<String, TokenModelLists>, DbError> {
    let mut out: HashMap<String, TokenModelLists> = HashMap::new();
    for r in rows {
        let id: String = r.try_get("token_id")?;
        let model: String = r.try_get("model")?;
        let entry = out.entry(id).or_default();
        match ManagedBy::parse(&r.try_get::<String, _>("managed_by")?) {
            ManagedBy::Owner => entry.owner.get_or_insert_with(Vec::new).push(model),
            ManagedBy::Admin => entry.admin.get_or_insert_with(Vec::new).push(model),
        }
    }
    Ok(out)
}

/// Group rows by token, resolve each token's two lists to the effective one,
/// and drop the tokens that end up unrestricted. Shared by the bulk reads so
/// they cannot disagree with [`for_token`] about what a token may reach.
fn fold_effective(
    rows: &[sqlx::sqlite::SqliteRow],
) -> Result<HashMap<String, Vec<String>>, DbError> {
    let lists = fold_lists(rows)?;
    let mut out = HashMap::new();
    for (id, l) in lists {
        if let Some(eff) = l.effective() {
            let mut v: Vec<String> = eff.into_iter().collect();
            v.sort();
            out.insert(id, v);
        }
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
        set_for_token(&pool, "tok-1", &["a".into(), "b".into()], ManagedBy::Owner)
            .await
            .unwrap();
        let got = for_token(&pool, "tok-1").await.unwrap().unwrap();
        assert_eq!(got, HashSet::from(["a".to_string(), "b".to_string()]));

        set_for_token(&pool, "tok-1", &[], ManagedBy::Owner)
            .await
            .unwrap();
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
        set_for_token(&pool, "tok-1", &["a".into(), "b".into()], ManagedBy::Owner)
            .await
            .unwrap();
        set_for_token(&pool, "tok-1", &["b".into(), "c".into()], ManagedBy::Owner)
            .await
            .unwrap();
        let got = for_token(&pool, "tok-1").await.unwrap().unwrap();
        assert_eq!(got, HashSet::from(["b".to_string(), "c".to_string()]));
    }

    /// The operator's list and the owner's are independent, and what the
    /// gateway enforces is their intersection: each side may only narrow.
    ///
    /// The failure this rules out is a union — under which an owner could
    /// re-grant themselves a model the operator had just removed, making the
    /// admin control decorative.
    #[tokio::test]
    async fn the_two_lists_intersect_so_neither_side_can_widen() {
        let pool = pool().await;
        set_for_token(&pool, "tok-1", &["a".into(), "b".into()], ManagedBy::Admin)
            .await
            .unwrap();
        // The owner names one model inside the admin's list and one outside.
        set_for_token(&pool, "tok-1", &["b".into(), "c".into()], ManagedBy::Owner)
            .await
            .unwrap();

        assert_eq!(
            for_token(&pool, "tok-1").await.unwrap().unwrap(),
            HashSet::from(["b".to_string()]),
            "only the model both authors allow"
        );
        // The bulk reads must agree with the single read.
        assert_eq!(
            for_user(&pool, "alice").await.unwrap().get("tok-1"),
            Some(&vec!["b".to_string()])
        );
    }

    /// Clearing one author's list leaves the other's standing — otherwise
    /// self-service would be a way out of an operator's restriction.
    #[tokio::test]
    async fn clearing_the_owners_list_leaves_the_admins_in_force() {
        let pool = pool().await;
        set_for_token(&pool, "tok-1", &["a".into()], ManagedBy::Admin)
            .await
            .unwrap();
        set_for_token(&pool, "tok-1", &["a".into(), "b".into()], ManagedBy::Owner)
            .await
            .unwrap();

        set_for_token(&pool, "tok-1", &[], ManagedBy::Owner)
            .await
            .unwrap();
        assert_eq!(
            for_token(&pool, "tok-1").await.unwrap().unwrap(),
            HashSet::from(["a".to_string()]),
            "the admin's restriction survives the owner clearing theirs"
        );

        let lists = lists_for_token(&pool, "tok-1").await.unwrap();
        assert_eq!(lists.owner, None);
        assert_eq!(lists.admin, Some(vec!["a".to_string()]));
    }

    /// Two lists that share nothing intersect to nothing, and that is a real
    /// state meaning "routes nowhere" — not the unrestricted default.
    #[tokio::test]
    async fn disjoint_lists_deny_everything_rather_than_reverting_to_open() {
        let pool = pool().await;
        set_for_token(&pool, "tok-1", &["a".into()], ManagedBy::Admin)
            .await
            .unwrap();
        set_for_token(&pool, "tok-1", &["b".into()], ManagedBy::Owner)
            .await
            .unwrap();
        let eff = for_token(&pool, "tok-1").await.unwrap();
        assert_eq!(
            eff,
            Some(HashSet::new()),
            "an empty intersection must not read as unrestricted"
        );
    }

    /// The page-level reads must skip unrestricted tokens entirely, so a
    /// caller can tell "no allowlist" from "an allowlist that is empty".
    #[tokio::test]
    async fn the_bulk_reads_omit_unrestricted_tokens() {
        let pool = pool().await;
        set_for_token(&pool, "tok-1", &["a".into()], ManagedBy::Owner)
            .await
            .unwrap();

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
            ManagedBy::Owner,
        )
        .await
        .unwrap();
        let got = for_token(&pool, "tok-1").await.unwrap().unwrap();
        assert_eq!(got, HashSet::from(["a".to_string(), "b".to_string()]));
    }
}
