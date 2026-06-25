// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Scheduled actions: per-user prompts that run automatically on a cron
//! schedule. Each fire opens a fresh chat session driven headlessly by
//! the same `OpenAiDriver` the interactive `/chat` path uses, so a
//! scheduled run is indistinguishable from a normal conversation in the
//! UI once it lands.
//!
//! This module owns the persistence (`scheduled_actions` table + CRUD)
//! and the cron evaluator ([`cron`]); the background loop that fires due
//! actions lives in [`worker`], and the web UI in
//! `rama_server::pages::scheduled`. The table is created by migration
//! `0021_scheduled_actions.sql`.

pub mod cron;
pub mod worker;

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

use crate::server::db::{DbError, Pool};

/// A persisted scheduled action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledAction {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub prompt: String,
    pub model: String,
    /// 5-field cron expression (the source of truth; the UI builder and
    /// the advanced field both write this).
    pub cron: String,
    /// IANA timezone the cron expression is evaluated in.
    pub timezone: String,
    pub tools_enabled: bool,
    /// `true` = each fire reuses the previous run's chat session
    /// (`last_session_id`) so the model sees prior runs as history; `false`
    /// = each fire opens a fresh session. First run (or a deleted prior
    /// session) falls back to a fresh session regardless.
    pub reuse_conversation: bool,
    /// When reusing, how many recent rounds (one round = the run's prompt +
    /// reply = 2 turns) of history to replay — caps unbounded growth.
    pub reuse_rounds: i64,
    /// `false` = paused; the worker ignores it and `next_run_at` is NULL.
    pub enabled: bool,
    /// Precomputed next fire time (UTC). NULL when paused or when the
    /// expression has no future occurrence.
    pub next_run_at: Option<Timestamp>,
    pub last_run_at: Option<Timestamp>,
    /// `"ok"` or `"error"` — the outcome of the most recent run.
    pub last_status: Option<String>,
    /// Chat session opened by the most recent run, for the "open" link.
    pub last_session_id: Option<String>,
    pub last_error: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The validated, ready-to-insert fields for a new action. `next_run_at`
/// is computed by the caller from `cron` + `timezone`.
pub struct NewAction {
    pub user_id: String,
    pub name: String,
    pub prompt: String,
    pub model: String,
    pub cron: String,
    pub timezone: String,
    pub tools_enabled: bool,
    pub reuse_conversation: bool,
    pub reuse_rounds: i64,
    pub next_run_at: Option<Timestamp>,
}

/// The mutable fields of an existing action, as submitted by the edit
/// form. `next_run_at` is recomputed by the caller from the new schedule.
pub struct EditAction {
    pub name: String,
    pub prompt: String,
    pub model: String,
    pub cron: String,
    pub timezone: String,
    pub tools_enabled: bool,
    pub reuse_conversation: bool,
    pub reuse_rounds: i64,
    pub next_run_at: Option<Timestamp>,
}

fn parse_ts(s: String, column: &'static str) -> Result<Timestamp, DbError> {
    s.parse().map_err(|e: jiff::Error| DbError::Decode {
        column,
        source: e.into(),
    })
}

fn parse_opt_ts(s: Option<String>, column: &'static str) -> Result<Option<Timestamp>, DbError> {
    s.map(|s| parse_ts(s, column)).transpose()
}

fn map_row(row: &SqliteRow) -> Result<ScheduledAction, DbError> {
    Ok(ScheduledAction {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        name: row.try_get("name")?,
        prompt: row.try_get("prompt")?,
        model: row.try_get("model")?,
        cron: row.try_get("cron")?,
        timezone: row.try_get("timezone")?,
        tools_enabled: row.try_get::<i64, _>("tools_enabled")? != 0,
        reuse_conversation: row.try_get::<i64, _>("reuse_conversation")? != 0,
        reuse_rounds: row.try_get("reuse_rounds")?,
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        next_run_at: parse_opt_ts(row.try_get("next_run_at")?, "next_run_at")?,
        last_run_at: parse_opt_ts(row.try_get("last_run_at")?, "last_run_at")?,
        last_status: row.try_get("last_status")?,
        last_session_id: row.try_get("last_session_id")?,
        last_error: row.try_get("last_error")?,
        created_at: parse_ts(row.try_get("created_at")?, "created_at")?,
        updated_at: parse_ts(row.try_get("updated_at")?, "updated_at")?,
    })
}

const COLS: &str = "id, user_id, name, prompt, model, cron, timezone, tools_enabled, \
     reuse_conversation, reuse_rounds, enabled, next_run_at, last_run_at, last_status, \
     last_session_id, last_error, created_at, updated_at";

/// Insert a new action. Returns the stored row.
pub async fn create(pool: &Pool, new: NewAction) -> Result<ScheduledAction, DbError> {
    let now = Timestamp::now();
    let row = ScheduledAction {
        id: Uuid::new_v4().to_string(),
        user_id: new.user_id,
        name: new.name,
        prompt: new.prompt,
        model: new.model,
        cron: new.cron,
        timezone: new.timezone,
        tools_enabled: new.tools_enabled,
        reuse_conversation: new.reuse_conversation,
        reuse_rounds: new.reuse_rounds,
        enabled: true,
        next_run_at: new.next_run_at,
        last_run_at: None,
        last_status: None,
        last_session_id: None,
        last_error: None,
        created_at: now,
        updated_at: now,
    };
    sqlx::query(
        r#"INSERT INTO scheduled_actions
              (id, user_id, name, prompt, model, cron, timezone, tools_enabled,
               reuse_conversation, reuse_rounds, enabled, next_run_at, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?)"#,
    )
    .bind(&row.id)
    .bind(&row.user_id)
    .bind(&row.name)
    .bind(&row.prompt)
    .bind(&row.model)
    .bind(&row.cron)
    .bind(&row.timezone)
    .bind(row.tools_enabled as i64)
    .bind(row.reuse_conversation as i64)
    .bind(row.reuse_rounds)
    .bind(row.next_run_at.map(|t| t.to_string()))
    .bind(row.created_at.to_string())
    .bind(row.updated_at.to_string())
    .execute(pool)
    .await?;
    Ok(row)
}

/// All of a user's actions, newest first.
pub async fn list_for_user(pool: &Pool, user_id: &str) -> Result<Vec<ScheduledAction>, DbError> {
    let sql = format!(
        "SELECT {COLS} FROM scheduled_actions WHERE user_id = ? \
         ORDER BY created_at DESC, id ASC"
    );
    let rows = sqlx::query(&sql).bind(user_id).fetch_all(pool).await?;
    rows.iter().map(map_row).collect()
}

/// One action, scoped to its owner (so a user can't read another's).
pub async fn get(pool: &Pool, user_id: &str, id: &str) -> Result<Option<ScheduledAction>, DbError> {
    let sql = format!("SELECT {COLS} FROM scheduled_actions WHERE id = ? AND user_id = ?");
    let row = sqlx::query(&sql)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(map_row).transpose()
}

/// Apply an edit, scoped to the owner. Returns `true` if a row matched.
pub async fn update(
    pool: &Pool,
    user_id: &str,
    id: &str,
    edit: EditAction,
) -> Result<bool, DbError> {
    let affected = sqlx::query(
        r#"UPDATE scheduled_actions
           SET name = ?, prompt = ?, model = ?, cron = ?, timezone = ?,
               tools_enabled = ?, reuse_conversation = ?, reuse_rounds = ?,
               next_run_at = ?, updated_at = ?
           WHERE id = ? AND user_id = ?"#,
    )
    .bind(&edit.name)
    .bind(&edit.prompt)
    .bind(&edit.model)
    .bind(&edit.cron)
    .bind(&edit.timezone)
    .bind(edit.tools_enabled as i64)
    .bind(edit.reuse_conversation as i64)
    .bind(edit.reuse_rounds)
    .bind(edit.next_run_at.map(|t| t.to_string()))
    .bind(Timestamp::now().to_string())
    .bind(id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Pause or resume an action. When resuming, the caller passes the
/// freshly-computed `next_run_at`; when pausing it passes `None`, which
/// takes the row out of the worker's due query.
pub async fn set_enabled(
    pool: &Pool,
    user_id: &str,
    id: &str,
    enabled: bool,
    next_run_at: Option<Timestamp>,
) -> Result<bool, DbError> {
    let affected = sqlx::query(
        r#"UPDATE scheduled_actions
           SET enabled = ?, next_run_at = ?, updated_at = ?
           WHERE id = ? AND user_id = ?"#,
    )
    .bind(enabled as i64)
    .bind(next_run_at.map(|t| t.to_string()))
    .bind(Timestamp::now().to_string())
    .bind(id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Delete an action, scoped to the owner. Returns `true` if a row matched.
pub async fn delete(pool: &Pool, user_id: &str, id: &str) -> Result<bool, DbError> {
    let affected = sqlx::query("DELETE FROM scheduled_actions WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

/// Every enabled action whose `next_run_at` is at or before `now` —
/// i.e. due to fire. The `now` bind is floored to whole seconds so it
/// shares the canonical `…:SSZ` RFC3339 format the minute-aligned
/// `next_run_at` uses, keeping the string comparison chronologically
/// exact (no sub-second drift across the boundary).
pub async fn due_actions(pool: &Pool, now: Timestamp) -> Result<Vec<ScheduledAction>, DbError> {
    let now_floored = now.strftime("%Y-%m-%dT%H:%M:%SZ").to_string();
    let sql = format!(
        "SELECT {COLS} FROM scheduled_actions \
         WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ? \
         ORDER BY next_run_at ASC"
    );
    let rows = sqlx::query(&sql).bind(now_floored).fetch_all(pool).await?;
    rows.iter().map(map_row).collect()
}

/// Advance only `next_run_at`. The worker "claims" a due action by
/// pushing this to the next future occurrence *before* it runs, so a
/// slow run (longer than the poll interval) or a crash mid-run can never
/// fire the same occurrence twice.
pub async fn set_next_run(
    pool: &Pool,
    id: &str,
    next_run_at: Option<Timestamp>,
) -> Result<(), DbError> {
    sqlx::query("UPDATE scheduled_actions SET next_run_at = ?, updated_at = ? WHERE id = ?")
        .bind(next_run_at.map(|t| t.to_string()))
        .bind(Timestamp::now().to_string())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record the outcome of a run and advance `next_run_at`. `status` is
/// `"ok"` or `"error"`; `session_id` is the chat the run opened (kept
/// even on error so the user can inspect the partial conversation).
pub async fn mark_ran(
    pool: &Pool,
    id: &str,
    status: &str,
    session_id: Option<&str>,
    next_run_at: Option<Timestamp>,
    error: Option<&str>,
) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE scheduled_actions
           SET last_run_at = ?, last_status = ?, last_session_id = ?,
               last_error = ?, next_run_at = ?, updated_at = ?
           WHERE id = ?"#,
    )
    .bind(Timestamp::now().to_string())
    .bind(status)
    .bind(session_id)
    .bind(error)
    .bind(next_run_at.map(|t| t.to_string()))
    .bind(Timestamp::now().to_string())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh_db() -> Pool {
        // `open` runs the migration set, which includes 0021 (the
        // scheduled_actions table).
        crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap()
    }

    async fn seed_user(pool: &Pool, id: &str) {
        // The FK to users(id) means we need a row to attach actions to.
        let now = Timestamp::now();
        crate::server::db::users::upsert(
            pool,
            &crate::server::db::users::User {
                id: id.to_string(),
                email: format!("{id}@example.com"),
                name: None,
                roles: vec![],
                created_at: now,
                updated_at: now,
                timezone: Some("Europe/Berlin".to_string()),
            },
        )
        .await
        .unwrap();
    }

    fn sample(user_id: &str, next: Option<Timestamp>) -> NewAction {
        NewAction {
            user_id: user_id.to_string(),
            name: "Daily digest".to_string(),
            prompt: "Summarize the news.".to_string(),
            model: "qwen".to_string(),
            cron: "0 9 * * *".to_string(),
            timezone: "Europe/Berlin".to_string(),
            tools_enabled: true,
            reuse_conversation: false,
            reuse_rounds: 5,
            next_run_at: next,
        }
    }

    #[tokio::test]
    async fn create_then_get_round_trips() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        let next = "2026-06-19T07:00:00Z".parse::<Timestamp>().unwrap();
        let created = create(&pool, sample("u1", Some(next))).await.unwrap();
        let got = get(&pool, "u1", &created.id).await.unwrap().unwrap();
        assert_eq!(got, created);
        assert_eq!(got.next_run_at, Some(next));
        assert!(got.enabled && got.tools_enabled);
    }

    #[tokio::test]
    async fn get_is_scoped_to_owner() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        seed_user(&pool, "u2").await;
        let created = create(&pool, sample("u1", None)).await.unwrap();
        assert!(get(&pool, "u2", &created.id).await.unwrap().is_none());
        assert!(!delete(&pool, "u2", &created.id).await.unwrap());
        assert!(get(&pool, "u1", &created.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn due_actions_selects_only_enabled_and_due() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        let past = "2026-06-19T06:00:00Z".parse::<Timestamp>().unwrap();
        let future = "2999-01-01T00:00:00Z".parse::<Timestamp>().unwrap();
        let due = create(&pool, sample("u1", Some(past))).await.unwrap();
        let not_yet = create(&pool, sample("u1", Some(future))).await.unwrap();
        let paused = create(&pool, sample("u1", Some(past))).await.unwrap();
        set_enabled(&pool, "u1", &paused.id, false, None)
            .await
            .unwrap();

        let now = "2026-06-19T06:30:00Z".parse::<Timestamp>().unwrap();
        let ids: Vec<String> = due_actions(&pool, now)
            .await
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec![due.id.clone()]);
        assert!(!ids.contains(&not_yet.id));
        assert!(!ids.contains(&paused.id));
    }

    #[tokio::test]
    async fn mark_ran_advances_next_run_and_records_status() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        let past = "2026-06-19T06:00:00Z".parse::<Timestamp>().unwrap();
        let a = create(&pool, sample("u1", Some(past))).await.unwrap();
        let advanced = "2026-06-20T07:00:00Z".parse::<Timestamp>().unwrap();
        mark_ran(&pool, &a.id, "ok", Some("sess-1"), Some(advanced), None)
            .await
            .unwrap();
        let got = get(&pool, "u1", &a.id).await.unwrap().unwrap();
        assert_eq!(got.next_run_at, Some(advanced));
        assert_eq!(got.last_status.as_deref(), Some("ok"));
        assert_eq!(got.last_session_id.as_deref(), Some("sess-1"));
        assert!(got.last_run_at.is_some());
    }
}
