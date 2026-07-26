// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-user durable memories — the store behind the `remember` /
//! `recall` tools and the `/memory` management page.
//!
//! Strictly per-user: every query is keyed by `user_id`, so callers can
//! only ever touch their own rows. Each memory is classified by
//! [`MemoryKind`] (preference / project / fact) so the store stays
//! explicit and groupable. Recall is recency + substring match (no
//! embeddings) — cheap and good enough for the handful of facts a user
//! accumulates.
//!
//! Schema lives in `migrations/0008_user_memories.sql` +
//! `migrations/0009_user_memories_kind.sql`.

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

use super::{DbError, Pool};

/// What a memory is about. Keeps the store structured: the model tags
/// each fact when it `remember`s, the `/memory` page groups by it, and
/// `recall` can filter by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    Preference,
    Project,
    Fact,
}

impl MemoryKind {
    /// Render/iteration order for the UI and grouping.
    pub const ALL: [MemoryKind; 3] = [
        MemoryKind::Preference,
        MemoryKind::Project,
        MemoryKind::Fact,
    ];

    /// Stable string stored in the DB column + accepted from tool args.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Preference => "preference",
            MemoryKind::Project => "project",
            MemoryKind::Fact => "fact",
        }
    }

    /// Human-readable section heading for the /memory page.
    pub fn label(self) -> &'static str {
        match self {
            MemoryKind::Preference => "Preferences",
            MemoryKind::Project => "Project context",
            MemoryKind::Fact => "Facts",
        }
    }

    /// Parse a caller-supplied kind, rejecting anything unknown. Used
    /// for tool args + the /memory form, where a bad value should be a
    /// clear error.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Parse a value read back from the DB, defaulting to `Fact` for
    /// anything unexpected — a stray row should never fail a whole
    /// listing.
    fn from_db(s: &str) -> Self {
        Self::parse(s).unwrap_or(MemoryKind::Fact)
    }
}

/// One stored fact about a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub id: String,
    pub kind: MemoryKind,
    pub content: String,
    pub created_at: Timestamp,
}

fn map_row(row: &SqliteRow) -> Result<Memory, DbError> {
    let id: String = row.try_get("id")?;
    let kind: String = row.try_get("kind")?;
    let content: String = row.try_get("content")?;
    let created_at_s: String = row.try_get("created_at")?;
    let created_at: Timestamp = created_at_s
        .parse()
        .map_err(|e: jiff::Error| DbError::Decode {
            column: "created_at",
            source: e.into(),
        })?;
    Ok(Memory {
        id,
        kind: MemoryKind::from_db(&kind),
        content,
        created_at,
    })
}

/// Store a memory for `user_id`. Exact-duplicate content for the same
/// user is collapsed: rather than pile up identical rows we update the
/// existing row's kind + `updated_at` and return it (re-remembering the
/// same fact under a new kind reclassifies it).
pub async fn insert(
    pool: &Pool,
    user_id: &str,
    kind: MemoryKind,
    content: &str,
) -> Result<Memory, DbError> {
    let now = Timestamp::now();
    let now_s = now.to_string();

    if let Some(existing) = sqlx::query(
        r#"SELECT id, kind, content, created_at FROM user_memories
           WHERE user_id = ? AND content = ?"#,
    )
    .bind(user_id)
    .bind(content)
    .fetch_optional(pool)
    .await?
    .as_ref()
    .map(map_row)
    .transpose()?
    {
        sqlx::query("UPDATE user_memories SET kind = ?, updated_at = ? WHERE id = ?")
            .bind(kind.as_str())
            .bind(&now_s)
            .bind(&existing.id)
            .execute(pool)
            .await?;
        return Ok(Memory { kind, ..existing });
    }

    let id = Uuid::new_v4().to_string();
    sqlx::query(
        r#"INSERT INTO user_memories (id, user_id, kind, content, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&id)
    .bind(user_id)
    .bind(kind.as_str())
    .bind(content)
    .bind(&now_s)
    .bind(&now_s)
    .execute(pool)
    .await?;
    Ok(Memory {
        id,
        kind,
        content: content.to_string(),
        created_at: now,
    })
}

/// All of the user's memories, newest first — the /memory page's
/// listing. Capped at `limit` as a sanity bound.
pub async fn list_for_user(pool: &Pool, user_id: &str, limit: i64) -> Result<Vec<Memory>, DbError> {
    let rows = sqlx::query(
        r#"SELECT id, kind, content, created_at FROM user_memories
           WHERE user_id = ?
           ORDER BY created_at DESC
           LIMIT ?"#,
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(map_row).collect()
}

/// Recall: the user's most recent memories, optionally filtered to one
/// `kind`, newest first, capped at `limit`.
pub async fn recall_recent(
    pool: &Pool,
    user_id: &str,
    kind: Option<MemoryKind>,
    limit: i64,
) -> Result<Vec<Memory>, DbError> {
    let rows = match kind {
        Some(k) => {
            sqlx::query(
                r#"SELECT id, kind, content, created_at FROM user_memories
                   WHERE user_id = ? AND kind = ?
                   ORDER BY created_at DESC LIMIT ?"#,
            )
            .bind(user_id)
            .bind(k.as_str())
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query(
                r#"SELECT id, kind, content, created_at FROM user_memories
                   WHERE user_id = ?
                   ORDER BY created_at DESC LIMIT ?"#,
            )
            .bind(user_id)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };
    rows.iter().map(map_row).collect()
}

/// Fetch one memory, scoped to its owner. `None` if it doesn't exist or
/// belongs to someone else.
pub async fn get(pool: &Pool, user_id: &str, id: &str) -> Result<Option<Memory>, DbError> {
    let row = sqlx::query(
        r#"SELECT id, kind, content, created_at FROM user_memories
           WHERE id = ? AND user_id = ?"#,
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_row).transpose()
}

/// Edit a memory's content (and optionally reclassify it). Scoped by
/// `user_id` so one user can never edit another's row. Returns the
/// updated row, or `None` if no such row belongs to the user.
pub async fn update(
    pool: &Pool,
    user_id: &str,
    id: &str,
    kind: MemoryKind,
    content: &str,
) -> Result<Option<Memory>, DbError> {
    let now = Timestamp::now().to_string();
    let affected = sqlx::query(
        r#"UPDATE user_memories SET kind = ?, content = ?, updated_at = ?
           WHERE id = ? AND user_id = ?"#,
    )
    .bind(kind.as_str())
    .bind(content)
    .bind(&now)
    .bind(id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    if affected == 0 {
        return Ok(None);
    }
    get(pool, user_id, id).await
}

/// Delete a memory, scoped by `user_id`. Returns whether a row was
/// removed (`false` = not found / not owned).
pub async fn delete(pool: &Pool, user_id: &str, id: &str) -> Result<bool, DbError> {
    let affected = sqlx::query("DELETE FROM user_memories WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
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
    async fn insert_keeps_kind_and_lists() {
        let pool = fresh().await;
        insert(&pool, "alice", MemoryKind::Preference, "metric units")
            .await
            .unwrap();
        let rows = list_for_user(&pool, "alice", 50).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, MemoryKind::Preference);
        assert_eq!(rows[0].content, "metric units");
    }

    #[tokio::test]
    async fn memories_are_scoped_per_user() {
        let pool = fresh().await;
        insert(&pool, "alice", MemoryKind::Fact, "alice fact")
            .await
            .unwrap();
        assert!(list_for_user(&pool, "bob", 50).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn duplicate_content_reclassifies_in_place() {
        let pool = fresh().await;
        let first = insert(&pool, "alice", MemoryKind::Fact, "runs ceph")
            .await
            .unwrap();
        let second = insert(&pool, "alice", MemoryKind::Project, "runs ceph")
            .await
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.kind, MemoryKind::Project);
        let rows = list_for_user(&pool, "alice", 50).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, MemoryKind::Project);
    }

    #[tokio::test]
    async fn recall_filters_by_kind() {
        let pool = fresh().await;
        insert(&pool, "alice", MemoryKind::Preference, "dark mode")
            .await
            .unwrap();
        insert(&pool, "alice", MemoryKind::Project, "ceph cluster aurora")
            .await
            .unwrap();
        let prefs = recall_recent(&pool, "alice", Some(MemoryKind::Preference), 50)
            .await
            .unwrap();
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs[0].content, "dark mode");
    }

    #[tokio::test]
    async fn update_edits_only_owners_row() {
        let pool = fresh().await;
        let m = insert(&pool, "alice", MemoryKind::Fact, "old")
            .await
            .unwrap();
        // Wrong owner → no-op.
        assert!(
            update(&pool, "bob", &m.id, MemoryKind::Fact, "hacked")
                .await
                .unwrap()
                .is_none()
        );
        // Owner → updates.
        let updated = update(&pool, "alice", &m.id, MemoryKind::Preference, "new")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.content, "new");
        assert_eq!(updated.kind, MemoryKind::Preference);
    }

    #[tokio::test]
    async fn delete_is_scoped_to_owner() {
        let pool = fresh().await;
        let m = insert(&pool, "alice", MemoryKind::Fact, "x").await.unwrap();
        assert!(!delete(&pool, "bob", &m.id).await.unwrap());
        assert!(delete(&pool, "alice", &m.id).await.unwrap());
        assert!(list_for_user(&pool, "alice", 50).await.unwrap().is_empty());
    }
}
