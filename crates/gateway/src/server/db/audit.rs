// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Append-only audit of admin impersonation.
//!
//! Every `/admin/users/{id}/impersonate` (start) and `/impersonate/stop`
//! writes one row here. Emails are denormalised onto the row so the trail
//! stays readable even after a user is deleted — and so the audit can't be
//! erased by an `ON DELETE CASCADE` (the table deliberately has no FKs).
//! See `migrations/0018_impersonation.sql`.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

use super::{DbError, Pool};

/// Whether the audited event started or ended an impersonation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Start,
    Stop,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Start => "start",
            Action::Stop => "stop",
        }
    }
}

/// One impersonation audit row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpersonationEvent {
    pub id: String,
    pub actor_id: String,
    pub actor_email: String,
    pub target_id: String,
    pub target_email: String,
    pub action: String,
    pub created_at: Timestamp,
}

fn map_row(row: &SqliteRow) -> Result<ImpersonationEvent, DbError> {
    let created_at: String = row.try_get("created_at")?;
    let created_at: Timestamp = created_at
        .parse()
        .map_err(|e: jiff::Error| DbError::Decode {
            column: "created_at",
            source: e.into(),
        })?;
    Ok(ImpersonationEvent {
        id: row.try_get("id")?,
        actor_id: row.try_get("actor_id")?,
        actor_email: row.try_get("actor_email")?,
        target_id: row.try_get("target_id")?,
        target_email: row.try_get("target_email")?,
        action: row.try_get("action")?,
        created_at,
    })
}

/// Record one impersonation start/stop. Best-effort at the call site:
/// the action itself (minting/dropping the session) is what matters, so
/// callers log a warning and carry on if this write fails rather than
/// failing the request.
#[allow(clippy::too_many_arguments)]
pub async fn record(
    pool: &Pool,
    actor_id: &str,
    actor_email: &str,
    target_id: &str,
    target_email: &str,
    action: Action,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO impersonation_audit
           (id, actor_id, actor_email, target_id, target_email, action, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(actor_id)
    .bind(actor_email)
    .bind(target_id)
    .bind(target_email)
    .bind(action.as_str())
    .bind(Timestamp::now().to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// The most recent `limit` impersonation events, newest first. Shown at
/// the foot of `/admin/users` so operators can see the recent trail.
pub async fn recent(pool: &Pool, limit: i64) -> Result<Vec<ImpersonationEvent>, DbError> {
    let rows =
        sqlx::query("SELECT * FROM impersonation_audit ORDER BY created_at DESC, id LIMIT ?")
            .bind(limit)
            .fetch_all(pool)
            .await?;
    rows.iter().map(map_row).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> Pool {
        super::super::open(std::path::Path::new(":memory:"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn record_then_recent_round_trips_newest_first() {
        let pool = pool().await;
        assert!(recent(&pool, 10).await.unwrap().is_empty());

        record(&pool, "admin", "admin@x", "alice", "alice@x", Action::Start)
            .await
            .unwrap();
        record(&pool, "admin", "admin@x", "alice", "alice@x", Action::Stop)
            .await
            .unwrap();

        let events = recent(&pool, 10).await.unwrap();
        assert_eq!(events.len(), 2);
        // Both rows present; the trail records actor → target + action.
        assert!(events.iter().any(|e| e.action == "start"));
        assert!(events.iter().any(|e| e.action == "stop"));
        assert!(
            events
                .iter()
                .all(|e| e.actor_id == "admin" && e.target_id == "alice")
        );
    }

    #[tokio::test]
    async fn recent_honours_limit() {
        let pool = pool().await;
        for _ in 0..5 {
            record(&pool, "a", "a@x", "b", "b@x", Action::Start)
                .await
                .unwrap();
        }
        assert_eq!(recent(&pool, 3).await.unwrap().len(), 3);
    }
}
