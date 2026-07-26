// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! DB CRUD for `comfyui_jobs` — tracks async ComfyUI workflow submissions
//! so the scheduler worker can poll for completion and re-host the result.

use jiff::Timestamp;
use serde::Serialize;
use sqlx::Row;

use crate::server::db::Pool;

/// One job row.
#[derive(Debug, Clone, Serialize)]
pub struct ComfyuiJob {
    pub id: i64,
    pub prompt_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub user_id: String,
    pub workflow_id: String,
    pub output_kind: String,
    pub output_node_id: String,
    pub filename_prefix: String,
    pub status: String,
    pub error_message: Option<String>,
    pub output_filename: Option<String>,
    pub output_mime: Option<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
}

/// Insert a new pending job. Called by the tool right after submitting
/// the workflow to ComfyUI.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    db: &Pool,
    prompt_id: &str,
    session_id: &str,
    turn_id: &str,
    user_id: &str,
    workflow_id: &str,
    output_kind: &str,
    output_node_id: &str,
    filename_prefix: &str,
) -> Result<i64, sqlx::Error> {
    let now = Timestamp::now().to_string();
    let row = sqlx::query(
        "INSERT INTO comfyui_jobs (prompt_id, session_id, turn_id, user_id, workflow_id, \
         output_kind, output_node_id, filename_prefix, status, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?) RETURNING id",
    )
    .bind(prompt_id)
    .bind(session_id)
    .bind(turn_id)
    .bind(user_id)
    .bind(workflow_id)
    .bind(output_kind)
    .bind(output_node_id)
    .bind(filename_prefix)
    .bind(&now)
    .fetch_one(db)
    .await?;
    row.try_get("id")
}

/// All jobs with `status = 'pending'` — what the scheduler polls.
pub async fn pending(db: &Pool) -> Result<Vec<ComfyuiJob>, sqlx::Error> {
    let rows =
        sqlx::query("SELECT * FROM comfyui_jobs WHERE status = 'pending' ORDER BY created_at ASC")
            .fetch_all(db)
            .await?;
    Ok(rows.iter().map(row_to_job).collect())
}

/// Count of jobs with `status = 'pending'`. Used by the tool to
/// enforce `[comfyui] max_concurrent_jobs` before submitting a new
/// workflow — cheaper than loading all rows.
pub async fn pending_count(db: &Pool) -> Result<i64, sqlx::Error> {
    let row = sqlx::query("SELECT COUNT(*) as count FROM comfyui_jobs WHERE status = 'pending'")
        .fetch_one(db)
        .await?;
    row.try_get::<i64, _>("count")
}

/// Load one job so the tool invocation can remain pending until the scheduler
/// records a terminal result. This is what lets the normal LLM tool loop send
/// the completed result back to the model and continue with its next action.
pub async fn get(db: &Pool, id: i64) -> Result<Option<ComfyuiJob>, sqlx::Error> {
    let row = sqlx::query("SELECT * FROM comfyui_jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(row.as_ref().map(row_to_job))
}

/// Mark a job as completed and record the output metadata.
pub async fn complete(
    db: &Pool,
    id: i64,
    output_filename: &str,
    output_mime: &str,
) -> Result<(), sqlx::Error> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        "UPDATE comfyui_jobs SET status = 'completed', output_filename = ?, output_mime = ?, \
         completed_at = ? WHERE id = ?",
    )
    .bind(output_filename)
    .bind(output_mime)
    .bind(&now)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Mark a job as failed with an error message.
pub async fn fail(db: &Pool, id: i64, error: &str) -> Result<(), sqlx::Error> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        "UPDATE comfyui_jobs SET status = 'failed', error_message = ?, completed_at = ? \
         WHERE id = ?",
    )
    .bind(error)
    .bind(&now)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Mark a job as timed out. Distinct from [`fail`] so the admin UI (and a
/// waiting tool call) can tell a deadline overrun apart from a hard ComfyUI
/// failure — the `status` column enumerates `timeout` for exactly this.
pub async fn timeout(db: &Pool, id: i64, error: &str) -> Result<(), sqlx::Error> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        "UPDATE comfyui_jobs SET status = 'timeout', error_message = ?, completed_at = ? \
         WHERE id = ?",
    )
    .bind(error)
    .bind(&now)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Recent jobs (any status) for the admin UI. Limited to the last N.
pub async fn recent(db: &Pool, limit: i64) -> Result<Vec<ComfyuiJob>, sqlx::Error> {
    let rows = sqlx::query("SELECT * FROM comfyui_jobs ORDER BY created_at DESC LIMIT ?")
        .bind(limit)
        .fetch_all(db)
        .await?;
    Ok(rows.iter().map(row_to_job).collect())
}

fn row_to_job(row: &sqlx::sqlite::SqliteRow) -> ComfyuiJob {
    ComfyuiJob {
        id: row.try_get("id").unwrap_or(0),
        prompt_id: row.try_get("prompt_id").unwrap_or_default(),
        session_id: row.try_get("session_id").unwrap_or_default(),
        turn_id: row.try_get("turn_id").unwrap_or_default(),
        user_id: row.try_get("user_id").unwrap_or_default(),
        workflow_id: row.try_get("workflow_id").unwrap_or_default(),
        output_kind: row.try_get("output_kind").unwrap_or_default(),
        output_node_id: row.try_get("output_node_id").unwrap_or_default(),
        filename_prefix: row.try_get("filename_prefix").unwrap_or_default(),
        status: row.try_get("status").unwrap_or_default(),
        error_message: row.try_get("error_message").unwrap_or(None),
        output_filename: row.try_get("output_filename").unwrap_or(None),
        output_mime: row.try_get("output_mime").unwrap_or(None),
        created_at: row.try_get("created_at").unwrap_or_default(),
        completed_at: row.try_get("completed_at").unwrap_or(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> Pool {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        // The migration runner already ran on :memory: — the table exists.
        pool
    }

    #[tokio::test]
    async fn create_and_pending_roundtrip() {
        let db = db().await;
        let id = create(
            &db,
            "p-1",
            "s-1",
            "t-1",
            "u-1",
            "text_to_image",
            "image",
            "9",
            "llmgw-t2i",
        )
        .await
        .unwrap();
        assert!(id > 0);
        let jobs = pending(&db).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].prompt_id, "p-1");
        assert_eq!(jobs[0].status, "pending");
    }

    #[tokio::test]
    async fn complete_updates_status() {
        let db = db().await;
        let id = create(
            &db,
            "p-2",
            "s-2",
            "t-2",
            "u-2",
            "text_to_image",
            "image",
            "9",
            "llmgw-t2i",
        )
        .await
        .unwrap();
        complete(&db, id, "llmgw-t2i_001.png", "image/png")
            .await
            .unwrap();
        let jobs = pending(&db).await.unwrap();
        assert!(jobs.is_empty(), "completed job should not be pending");
        let recent = recent(&db, 10).await.unwrap();
        assert_eq!(recent[0].status, "completed");
        assert_eq!(
            recent[0].output_filename.as_deref(),
            Some("llmgw-t2i_001.png")
        );
    }

    #[tokio::test]
    async fn get_returns_the_current_job_state() {
        let db = db().await;
        let id = create(
            &db,
            "p-get",
            "s-get",
            "t-get",
            "u-get",
            "text_to_image",
            "image",
            "9",
            "llmgw-t2i",
        )
        .await
        .unwrap();
        let job = get(&db, id).await.unwrap().unwrap();
        assert_eq!(job.id, id);
        assert_eq!(job.status, "pending");
        assert!(get(&db, id + 1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn fail_updates_status() {
        let db = db().await;
        let id = create(
            &db,
            "p-3",
            "s-3",
            "t-3",
            "u-3",
            "text_to_image",
            "image",
            "9",
            "llmgw-t2i",
        )
        .await
        .unwrap();
        fail(&db, id, "ComfyUI crashed").await.unwrap();
        let jobs = pending(&db).await.unwrap();
        assert!(jobs.is_empty());
        let recent = recent(&db, 10).await.unwrap();
        assert_eq!(recent[0].status, "failed");
        assert_eq!(recent[0].error_message.as_deref(), Some("ComfyUI crashed"));
    }
}
