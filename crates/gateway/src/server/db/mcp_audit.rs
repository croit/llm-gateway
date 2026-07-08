// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Append-only audit of MCP connector tool calls.
//!
//! One row per invocation of a tool belonging to a connector that has
//! `audit` enabled (see `mcp_catalog_connectors.audit`, migration 0036). The
//! acting user's email is denormalised onto the row and the table has no
//! foreign keys, so — like `impersonation_audit` — the trail stays readable
//! after a user or connector is deleted and can't be erased by a cascade.
//!
//! This is the accountability half of MCP misuse controls: with a shared bot
//! (one Discord identity for the whole gateway) the human behind an action
//! only exists here. Writes are best-effort at the call site — a failed audit
//! write logs a warning but never fails the tool call.

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

use super::{DbError, Pool};

/// Longest argument JSON we store; longer is truncated with a marker so a big
/// payload can't bloat the audit table.
const MAX_ARGS_LEN: usize = 4000;

/// One recorded MCP tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolEvent {
    pub id: String,
    pub user_id: String,
    pub user_email: String,
    pub connector_key: String,
    pub tool_id: String,
    pub arguments: Option<String>,
    pub outcome: String,
    pub error: Option<String>,
    pub session_id: Option<String>,
    pub created_at: Timestamp,
}

fn map_row(row: &SqliteRow) -> Result<McpToolEvent, DbError> {
    let created_at: String = row.try_get("created_at")?;
    let created_at: Timestamp = created_at
        .parse()
        .map_err(|e: jiff::Error| DbError::Decode {
            column: "created_at",
            source: e.into(),
        })?;
    Ok(McpToolEvent {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        user_email: row.try_get("user_email")?,
        connector_key: row.try_get("connector_key")?,
        tool_id: row.try_get("tool_id")?,
        arguments: row.try_get("arguments")?,
        outcome: row.try_get("outcome")?,
        error: row.try_get("error")?,
        session_id: row.try_get("session_id")?,
        created_at,
    })
}

/// Truncate an argument string to [`MAX_ARGS_LEN`] on a char boundary,
/// appending a marker when cut.
fn truncate_args(args: &str) -> String {
    if args.len() <= MAX_ARGS_LEN {
        return args.to_string();
    }
    let mut end = MAX_ARGS_LEN;
    while !args.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &args[..end])
}

/// Record one MCP tool call. Best-effort: callers log and carry on if this
/// fails — the tool call itself is authoritative, not the audit write.
///
/// The acting user's email is looked up and denormalised so the row survives
/// user deletion; an unknown/deleted user records an empty email.
#[allow(clippy::too_many_arguments)]
pub async fn record(
    pool: &Pool,
    user_id: &str,
    connector_key: &str,
    tool_id: &str,
    arguments: Option<&str>,
    outcome: &str,
    error: Option<&str>,
    session_id: Option<&str>,
) -> Result<(), DbError> {
    let user_email: String = sqlx::query("SELECT email FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(pool)
        .await?
        .and_then(|r| r.try_get::<String, _>("email").ok())
        .unwrap_or_default();
    let arguments = arguments.map(truncate_args);
    sqlx::query(
        "INSERT INTO mcp_tool_audit
           (id, user_id, user_email, connector_key, tool_id, arguments, outcome, error, session_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(user_email)
    .bind(connector_key)
    .bind(tool_id)
    .bind(&arguments)
    .bind(outcome)
    .bind(error)
    .bind(session_id)
    .bind(Timestamp::now().to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// The most recent `limit` audited tool calls, newest first.
pub async fn recent(pool: &Pool, limit: i64) -> Result<Vec<McpToolEvent>, DbError> {
    let rows = sqlx::query("SELECT * FROM mcp_tool_audit ORDER BY created_at DESC, id LIMIT ?")
        .bind(limit)
        .fetch_all(pool)
        .await?;
    rows.iter().map(map_row).collect()
}

/// The most recent `limit` audited calls for one connector, newest first.
pub async fn recent_for_connector(
    pool: &Pool,
    connector_key: &str,
    limit: i64,
) -> Result<Vec<McpToolEvent>, DbError> {
    let rows = sqlx::query(
        "SELECT * FROM mcp_tool_audit WHERE connector_key = ? ORDER BY created_at DESC, id LIMIT ?",
    )
    .bind(connector_key)
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

        record(
            &pool,
            "u1",
            "discord",
            "mcp__discord__send_private_message",
            Some(r#"{"userId":"1","message":"hi"}"#),
            "ok",
            None,
            Some("sess1"),
        )
        .await
        .unwrap();
        record(
            &pool,
            "u1",
            "discord",
            "mcp__discord__create_webhook",
            Some(r#"{"channelId":"2","name":"x"}"#),
            "error",
            Some("Missing permission: MANAGE_WEBHOOKS"),
            None,
        )
        .await
        .unwrap();

        let ev = recent(&pool, 10).await.unwrap();
        assert_eq!(ev.len(), 2);
        // Newest first: the error (recorded last) leads.
        assert_eq!(ev[0].outcome, "error");
        assert_eq!(
            ev[0].error.as_deref(),
            Some("Missing permission: MANAGE_WEBHOOKS")
        );
        assert_eq!(ev[1].outcome, "ok");
        assert!(
            ev.iter()
                .all(|e| e.connector_key == "discord" && e.user_id == "u1")
        );
    }

    #[tokio::test]
    async fn recent_for_connector_filters() {
        let pool = pool().await;
        record(
            &pool,
            "u",
            "discord",
            "mcp__discord__send_message",
            None,
            "ok",
            None,
            None,
        )
        .await
        .unwrap();
        record(
            &pool,
            "u",
            "github",
            "mcp__github__create_issue",
            None,
            "ok",
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            recent_for_connector(&pool, "discord", 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            recent_for_connector(&pool, "github", 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(recent(&pool, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn arguments_are_truncated() {
        let pool = pool().await;
        let big = "x".repeat(MAX_ARGS_LEN + 500);
        record(&pool, "u", "discord", "t", Some(&big), "ok", None, None)
            .await
            .unwrap();
        let ev = recent(&pool, 1).await.unwrap();
        let stored = ev[0].arguments.as_deref().unwrap();
        assert!(stored.len() < big.len());
        assert!(stored.ends_with("… [truncated]"));
    }
}
