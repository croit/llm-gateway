// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! UI-managed skill→role grants — the DB side of the `/admin/skills`
//! "Granted to" editor.
//!
//! Skills are uploaded and deleted live from the admin page, so who may use
//! them is managed there too. These rows are an **additive overlay** on top of
//! the static `[[roles]].skills` config:
//! [`crate::server::rbac::Resolver::allowed_skills`] unions them with the
//! config grants. The config grants stay authoritative and read-only (a role
//! with `skills = ["*"]` keeps every skill regardless of this table); the UI
//! only ever adds or removes rows here. Filtering to currently-loaded skills
//! happens in the resolver, so a dangling row for a deleted skill is harmless.
//!
//! Schema: `migrations/0025_skill_role_grants.sql`.

use jiff::Timestamp;
use sqlx::Row;

use super::{DbError, Pool};

/// Every `(skill_name, role_id)` overlay grant. Used to seed the resolver
/// overlay at startup and to reload it after an edit (cheap — the set is
/// bounded by #skills × #roles).
pub async fn all(pool: &Pool) -> Result<Vec<(String, String)>, DbError> {
    let rows = sqlx::query("SELECT skill_name, role_id FROM skill_role_grants")
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get::<String, _>("skill_name")?,
                r.try_get::<String, _>("role_id")?,
            ))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(DbError::from)
}

/// Role ids currently granted `skill_name` via the UI overlay (config grants
/// not included — those live in `[[roles]].skills`).
pub async fn roles_for_skill(pool: &Pool, skill_name: &str) -> Result<Vec<String>, DbError> {
    let rows =
        sqlx::query("SELECT role_id FROM skill_role_grants WHERE skill_name = ? ORDER BY role_id")
            .bind(skill_name)
            .fetch_all(pool)
            .await?;
    rows.iter()
        .map(|r| r.try_get::<String, _>("role_id").map_err(DbError::from))
        .collect()
}

/// Replace the full set of overlay role grants for `skill_name` with `roles`
/// (deduplicated). Runs in a transaction so a concurrent reader never sees a
/// half-applied edit. An empty `roles` clears every overlay grant for the
/// skill.
pub async fn set_for_skill(pool: &Pool, skill_name: &str, roles: &[String]) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM skill_role_grants WHERE skill_name = ?")
        .bind(skill_name)
        .execute(&mut *tx)
        .await?;
    let mut seen: Vec<&str> = Vec::new();
    for role in roles {
        if seen.contains(&role.as_str()) {
            continue;
        }
        seen.push(role.as_str());
        sqlx::query(
            "INSERT INTO skill_role_grants (skill_name, role_id, granted_at) VALUES (?, ?, ?)",
        )
        .bind(skill_name)
        .bind(role)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Drop all overlay grants for `skill_name`. Called when a skill is deleted so
/// a later re-upload of the same name doesn't silently resurrect the old
/// access.
pub async fn delete_skill(pool: &Pool, skill_name: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM skill_role_grants WHERE skill_name = ?")
        .bind(skill_name)
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

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[tokio::test]
    async fn set_and_read_back() {
        let pool = fresh().await;
        set_for_skill(&pool, "brand", &v(&["eng", "admin"]))
            .await
            .unwrap();
        let mut roles = roles_for_skill(&pool, "brand").await.unwrap();
        roles.sort();
        assert_eq!(roles, v(&["admin", "eng"]));
    }

    #[tokio::test]
    async fn set_replaces_previous() {
        let pool = fresh().await;
        set_for_skill(&pool, "brand", &v(&["eng"])).await.unwrap();
        set_for_skill(&pool, "brand", &v(&["admin"])).await.unwrap();
        assert_eq!(
            roles_for_skill(&pool, "brand").await.unwrap(),
            v(&["admin"])
        );
    }

    #[tokio::test]
    async fn empty_clears_grants() {
        let pool = fresh().await;
        set_for_skill(&pool, "brand", &v(&["eng"])).await.unwrap();
        set_for_skill(&pool, "brand", &[]).await.unwrap();
        assert!(roles_for_skill(&pool, "brand").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn set_dedupes_input() {
        let pool = fresh().await;
        set_for_skill(&pool, "brand", &v(&["eng", "eng", "eng"]))
            .await
            .unwrap();
        assert_eq!(roles_for_skill(&pool, "brand").await.unwrap(), v(&["eng"]));
    }

    #[tokio::test]
    async fn all_returns_every_pair() {
        let pool = fresh().await;
        set_for_skill(&pool, "brand", &v(&["eng"])).await.unwrap();
        set_for_skill(&pool, "legal", &v(&["admin"])).await.unwrap();
        let mut all = all(&pool).await.unwrap();
        all.sort();
        assert_eq!(
            all,
            vec![
                ("brand".to_string(), "eng".to_string()),
                ("legal".to_string(), "admin".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn delete_skill_clears_its_grants_only() {
        let pool = fresh().await;
        set_for_skill(&pool, "brand", &v(&["eng"])).await.unwrap();
        set_for_skill(&pool, "legal", &v(&["admin"])).await.unwrap();
        delete_skill(&pool, "brand").await.unwrap();
        assert!(roles_for_skill(&pool, "brand").await.unwrap().is_empty());
        assert_eq!(
            roles_for_skill(&pool, "legal").await.unwrap(),
            v(&["admin"])
        );
    }
}
