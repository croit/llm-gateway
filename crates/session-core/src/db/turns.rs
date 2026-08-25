// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

/// Bump `updated_at` so the session floats to the top of the sidebar.
/// Called after a new turn lands.
pub async fn touch_session(pool: &Pool, session_id: &str) -> Result<(), DbError> {
    sqlx::query(r#"UPDATE chat_sessions SET updated_at = ? WHERE id = ?"#)
        .bind(Timestamp::now().to_string())
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Set the session title. Called once when the first user message lands
/// (auto-title = first user message truncated); the user may overwrite
/// later from the UI.
pub async fn set_session_title(pool: &Pool, session_id: &str, title: &str) -> Result<(), DbError> {
    sqlx::query(r#"UPDATE chat_sessions SET title = ? WHERE id = ?"#)
        .bind(title)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Turns

pub(crate) async fn next_turn_seq(pool: &Pool, session_id: &str) -> Result<i64, DbError> {
    let row = sqlx::query(
        r#"SELECT COALESCE(MAX(seq), -1) + 1 AS next_seq
           FROM chat_turns
           WHERE session_id = ?"#,
    )
    .bind(session_id)
    .fetch_one(pool)
    .await?;
    Ok(row.try_get("next_seq")?)
}

/// Insert a user-role turn (already 'completed' — the user's message is
/// final the moment we receive it). Returns the new turn.
///
/// `turn_id` is caller-supplied so attachments uploaded under that id's
/// S3 prefix have a stable key the chat-page render-refresh can find
/// later. Pre-generate at the handler entry and pass both here and to
/// the upload step.
pub async fn create_user_turn(
    pool: &Pool,
    session_id: &str,
    turn_id: &str,
    content: &str,
) -> Result<Turn, DbError> {
    let seq = next_turn_seq(pool, session_id).await?;
    let now = Timestamp::now();
    let turn = Turn {
        id: turn_id.to_string(),
        session_id: session_id.to_string(),
        seq,
        role: TurnRole::User,
        user_content: Some(content.to_string()),
        model: None,
        content: None,
        reasoning: None,
        reasoning_elapsed_ms: None,
        reasoning_started_at: None,
        status: TurnStatus::Completed,
        error_message: None,
        created_at: now,
        completed_at: Some(now),
    };
    sqlx::query(
        r#"INSERT INTO chat_turns
              (id, session_id, seq, role, user_content, status, created_at, completed_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&turn.id)
    .bind(&turn.session_id)
    .bind(turn.seq)
    .bind(turn.role.as_str())
    .bind(strip_fts_sentinels(content).as_ref())
    .bind(turn.status.as_str())
    .bind(turn.created_at.to_string())
    .bind(turn.completed_at.map(|t| t.to_string()))
    .execute(pool)
    .await?;
    Ok(turn)
}

/// Insert an assistant-role turn in `in_progress` state. The worker
/// fills in content/reasoning incrementally and calls `finalize_turn`
/// at the end.
///
/// The turn id is supplied by the caller so it can reserve the
/// per-user worker slot under that id *before* persisting anything —
/// see `chat_message_send`. If reservation fails, no DB row is
/// created and the id is simply discarded.
pub async fn create_assistant_turn_in_progress(
    pool: &Pool,
    session_id: &str,
    turn_id: &str,
    model: &str,
) -> Result<Turn, DbError> {
    let seq = next_turn_seq(pool, session_id).await?;
    let now = Timestamp::now();
    let turn = Turn {
        id: turn_id.to_string(),
        session_id: session_id.to_string(),
        seq,
        role: TurnRole::Assistant,
        user_content: None,
        model: Some(model.to_string()),
        content: None,
        reasoning: None,
        reasoning_elapsed_ms: None,
        reasoning_started_at: None,
        status: TurnStatus::InProgress,
        error_message: None,
        created_at: now,
        completed_at: None,
    };
    sqlx::query(
        r#"INSERT INTO chat_turns
              (id, session_id, seq, role, model, status, created_at)
           VALUES (?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&turn.id)
    .bind(&turn.session_id)
    .bind(turn.seq)
    .bind(turn.role.as_str())
    .bind(model)
    .bind(turn.status.as_str())
    .bind(turn.created_at.to_string())
    .execute(pool)
    .await?;
    Ok(turn)
}

/// Append to an in-progress assistant turn's `content`. Worker batches
/// these every ~100ms; SQLite handles the small-string concat fine but
/// we don't want one write per token.
pub async fn append_content(pool: &Pool, turn_id: &str, chunk: &str) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE chat_turns
           SET content = COALESCE(content, '') || ?
           WHERE id = ?"#,
    )
    .bind(strip_fts_sentinels(chunk).as_ref())
    .bind(turn_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Overwrite an in-progress assistant turn's `content` wholesale.
/// Unlike [`append_content`], this *replaces* the column — used when a
/// tool needs to rewrite prior markers rather than only add to them
/// (e.g. a typst re-render superseding the earlier render's chip within
/// the same turn). The live view re-renders full turn content from the
/// DB on every tick, so the rewrite is reflected without a delta-accrual
/// mismatch. Pairs with [`get_content`].
pub async fn set_content(pool: &Pool, turn_id: &str, content: &str) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE chat_turns
           SET content = ?
           WHERE id = ?"#,
    )
    .bind(strip_fts_sentinels(content).as_ref())
    .bind(turn_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Read the current `content` of a turn — used by tools that need
/// to inspect already-spliced attachment markers (e.g. to pick a
/// non-colliding filename for a same-turn re-upload). Returns `None`
/// for a missing row, `Some(String::new())` for a row whose content
/// is still SQL-NULL.
pub async fn get_content(pool: &Pool, turn_id: &str) -> Result<Option<String>, DbError> {
    let row = sqlx::query(r#"SELECT content FROM chat_turns WHERE id = ?"#)
        .bind(turn_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| {
        r.try_get::<Option<String>, _>("content")
            .ok()
            .flatten()
            .unwrap_or_default()
    }))
}

/// Append to an in-progress assistant turn's `reasoning`. Same
/// batching pattern as `append_content`.
pub async fn append_reasoning(pool: &Pool, turn_id: &str, chunk: &str) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE chat_turns
           SET reasoning = COALESCE(reasoning, '') || ?
           WHERE id = ?"#,
    )
    .bind(chunk)
    .bind(turn_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Anchor the reasoning timer. Called once, when the model emits its
/// first reasoning chunk. The client-side `<thinking-timer>` counts up
/// from this instant; a mid-stream reload / late SSE subscriber reads it
/// to resume the count instead of restarting at 0. The `IS NULL` guard
/// keeps it set-once even if the driver calls it more than once.
pub async fn set_reasoning_started(
    pool: &Pool,
    turn_id: &str,
    started_at: Timestamp,
) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE chat_turns SET reasoning_started_at = ?
           WHERE id = ? AND reasoning_started_at IS NULL"#,
    )
    .bind(started_at.to_string())
    .bind(turn_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Freeze the reasoning timer at its final duration. Called once, the
/// moment the model emits its first visible content delta (= it has
/// stopped reasoning), and at finalization for a reasoning-only turn.
/// This is the authoritative value the settled "Thought for X.Ys" label
/// renders from.
pub async fn set_reasoning_elapsed(
    pool: &Pool,
    turn_id: &str,
    elapsed_ms: i64,
) -> Result<(), DbError> {
    sqlx::query(r#"UPDATE chat_turns SET reasoning_elapsed_ms = ? WHERE id = ?"#)
        .bind(elapsed_ms)
        .bind(turn_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record the largest `prompt_tokens` the upstream reported across an
/// assistant turn's rounds — a model-tokenizer-accurate measure of how
/// big the session's replayed context had grown by the end of the turn.
/// The gateway's compaction trigger reads it back via
/// [`latest_context_tokens`]. Column added in migration 0032.
pub async fn set_context_tokens(
    pool: &Pool,
    turn_id: &str,
    context_tokens: i64,
) -> Result<(), DbError> {
    sqlx::query(r#"UPDATE chat_turns SET context_tokens = ? WHERE id = ?"#)
        .bind(context_tokens)
        .bind(turn_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The most recently measured context size for a session — the
/// `context_tokens` of the latest turn that carried one. `None` when no
/// turn in the session has a measurement yet (e.g. the upstream never
/// reported usage). Used by the gateway's auto-compaction threshold check.
pub async fn latest_context_tokens(pool: &Pool, session_id: &str) -> Result<Option<i64>, DbError> {
    let row = sqlx::query(
        r#"SELECT context_tokens
           FROM chat_turns
           WHERE session_id = ? AND context_tokens IS NOT NULL
           ORDER BY seq DESC
           LIMIT 1"#,
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    row.map(|r| r.try_get::<i64, _>("context_tokens"))
        .transpose()
        .map_err(Into::into)
}

/// End-of-stream: flip the turn's status and stamp `completed_at`. The
/// worker calls this exactly once per assistant turn whether the turn
/// ended naturally, via cancel, or with an error.
pub async fn finalize_turn(
    pool: &Pool,
    turn_id: &str,
    status: TurnStatus,
    error_message: Option<&str>,
) -> Result<(), DbError> {
    if status == TurnStatus::InProgress {
        return Err(DbError::Decode {
            column: "status",
            source: anyhow::anyhow!("finalize_turn called with status=in_progress"),
        });
    }
    sqlx::query(
        r#"UPDATE chat_turns
           SET status = ?, error_message = ?, completed_at = ?
           WHERE id = ?"#,
    )
    .bind(status.as_str())
    .bind(error_message)
    .bind(Timestamp::now().to_string())
    .bind(turn_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// All turns in a session, oldest first, each carrying its tool calls
/// (also oldest first). Used by the renderer for both initial page
/// load and the reconnect-tail path.
pub async fn list_turns(pool: &Pool, session_id: &str) -> Result<Vec<TurnWithTools>, DbError> {
    let turn_rows = sqlx::query(
        r#"SELECT id, session_id, seq, role, user_content, model, content,
                  reasoning, reasoning_elapsed_ms, reasoning_started_at,
                  status, error_message,
                  created_at, completed_at
           FROM chat_turns
           WHERE session_id = ?
           ORDER BY seq ASC"#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    let turns: Vec<Turn> = turn_rows.iter().map(map_turn).collect::<Result<_, _>>()?;
    if turns.is_empty() {
        return Ok(Vec::new());
    }

    // One query for all tool calls in the session, then bucketed by
    // turn_id. Cheaper than N+1.
    let tool_rows = sqlx::query(
        r#"SELECT tc.id, tc.turn_id, tc.seq, tc.name, tc.arguments_json,
                  tc.output_json, tc.status, tc.created_at, tc.completed_at
           FROM chat_tool_calls tc
           JOIN chat_turns t ON t.id = tc.turn_id
           WHERE t.session_id = ?
           ORDER BY tc.turn_id, tc.seq ASC"#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    let mut by_turn: std::collections::HashMap<String, Vec<ToolCall>> =
        std::collections::HashMap::new();
    for r in &tool_rows {
        let tc = map_tool_call(r)?;
        by_turn.entry(tc.turn_id.clone()).or_default().push(tc);
    }

    Ok(turns
        .into_iter()
        .map(|turn| TurnWithTools {
            tool_calls: by_turn.remove(&turn.id).unwrap_or_default(),
            turn,
        })
        .collect())
}

/// The single in-flight assistant turn for a session, if any. Used by
/// the tail-subscription handler to decide whether to attach to a
/// running worker.
pub async fn in_flight_turn(pool: &Pool, session_id: &str) -> Result<Option<Turn>, DbError> {
    let row = sqlx::query(
        r#"SELECT id, session_id, seq, role, user_content, model, content,
                  reasoning, reasoning_elapsed_ms, reasoning_started_at,
                  status, error_message,
                  created_at, completed_at
           FROM chat_turns
           WHERE session_id = ? AND status = 'in_progress'
           ORDER BY seq DESC
           LIMIT 1"#,
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_turn).transpose()
}

/// Fetch one turn by id, scoped to its session. `None` if it doesn't
/// exist or belongs to another session. Used by retry/edit to look up
/// the target turn's `seq` and role before truncating.
pub async fn get_turn(
    pool: &Pool,
    session_id: &str,
    turn_id: &str,
) -> Result<Option<Turn>, DbError> {
    let row = sqlx::query(
        r#"SELECT id, session_id, seq, role, user_content, model, content,
                  reasoning, reasoning_elapsed_ms, reasoning_started_at,
                  status, error_message,
                  created_at, completed_at
           FROM chat_turns
           WHERE session_id = ? AND id = ?"#,
    )
    .bind(session_id)
    .bind(turn_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(map_turn).transpose()
}

/// Fetch one turn by id together with its tool calls, scoped to its
/// session. This is the per-tick read for the streaming SSE loop: the
/// loop only ever mutates one assistant turn, so re-reading the whole
/// conversation (`list_turns`) per delta was pure overhead — and grew
/// with conversation length on every tick.
pub async fn get_turn_with_tools(
    pool: &Pool,
    session_id: &str,
    turn_id: &str,
) -> Result<Option<TurnWithTools>, DbError> {
    let Some(turn) = get_turn(pool, session_id, turn_id).await? else {
        return Ok(None);
    };
    let tool_rows = sqlx::query(
        r#"SELECT id, turn_id, seq, name, arguments_json, output_json,
                  status, created_at, completed_at
           FROM chat_tool_calls
           WHERE turn_id = ?
           ORDER BY seq ASC"#,
    )
    .bind(turn_id)
    .fetch_all(pool)
    .await?;
    let tool_calls: Vec<ToolCall> = tool_rows
        .iter()
        .map(map_tool_call)
        .collect::<Result<_, _>>()?;
    Ok(Some(TurnWithTools { turn, tool_calls }))
}

/// Replace a user turn's text (the "edit" action). Scoped to the
/// session + `role = 'user'` so it can never rewrite an assistant turn.
/// Returns whether a row was updated.
pub async fn update_user_turn_content(
    pool: &Pool,
    session_id: &str,
    turn_id: &str,
    content: &str,
) -> Result<bool, DbError> {
    let affected = sqlx::query(
        r#"UPDATE chat_turns SET user_content = ?
           WHERE id = ? AND session_id = ? AND role = 'user'"#,
    )
    .bind(strip_fts_sentinels(content).as_ref())
    .bind(turn_id)
    .bind(session_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Delete every turn in the session from `from_seq` onwards (inclusive).
/// Backs retry/edit: drop the target turn and everything below it before
/// regenerating. Tool-call rows cascade via the FK. Returns the number
/// of turns removed.
pub async fn delete_turns_from_seq(
    pool: &Pool,
    session_id: &str,
    from_seq: i64,
) -> Result<u64, DbError> {
    let affected = sqlx::query("DELETE FROM chat_turns WHERE session_id = ? AND seq >= ?")
        .bind(session_id)
        .bind(from_seq)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected)
}

/// Flip every still-`in_progress` assistant turn to `errored`. Used
/// at startup to evict orphans left behind by a crash / SIGKILL — no
/// worker is going to come back and finish them.
///
/// Returns the number of rows actually touched (useful for a one-shot
/// log line and for the startup test). Idempotent on a clean DB.
pub async fn sweep_in_progress_at_startup(pool: &Pool) -> Result<u64, DbError> {
    let now = Timestamp::now().to_string();
    let affected = sqlx::query(
        r#"UPDATE chat_turns
           SET status = 'errored',
               error_message = COALESCE(error_message,
                                        'Stream interrupted — the server restarted before this response finished.'),
               completed_at = ?
           WHERE status = 'in_progress' AND role = 'assistant'"#,
    )
    .bind(&now)
    .execute(pool)
    .await?
    .rows_affected();
    // The turn sweep above doesn't touch `chat_tool_calls`. A row left at
    // 'running' renders as "Calling" forever even once its turn is errored —
    // the orphaned-tool-call bug. At startup no worker can come back to finish
    // any of them, so flip every still-running row to errored too.
    sqlx::query(
        r#"UPDATE chat_tool_calls
           SET status = 'errored',
               output_json = COALESCE(output_json,
                                      'Tool call interrupted — the server restarted before it finished.'),
               completed_at = ?
           WHERE status = 'running'"#,
    )
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(affected)
}

/// Mark in-progress assistant turns in `session_id` as errored,
/// except for `exempt_turn_id` (the one a live worker is still
/// driving). Called from the render path so a stale orphan from a
/// previous crash / `Busy`-path leak doesn't keep showing the
/// thinking spinner.
///
/// Returns the number of rows flipped.
pub async fn mark_orphaned_in_progress_as_errored(
    pool: &Pool,
    session_id: &str,
    exempt_turn_id: Option<&str>,
) -> Result<u64, DbError> {
    let now = Timestamp::now().to_string();
    let message = "Stream interrupted — no worker is producing this response.";
    let affected = if let Some(exempt) = exempt_turn_id {
        sqlx::query(
            r#"UPDATE chat_turns
               SET status = 'errored',
                   error_message = COALESCE(error_message, ?),
                   completed_at = ?
               WHERE session_id = ?
                 AND status = 'in_progress'
                 AND role = 'assistant'
                 AND id != ?"#,
        )
        .bind(message)
        .bind(&now)
        .bind(session_id)
        .bind(exempt)
        .execute(pool)
        .await?
        .rows_affected()
    } else {
        sqlx::query(
            r#"UPDATE chat_turns
               SET status = 'errored',
                   error_message = COALESCE(error_message, ?),
                   completed_at = ?
               WHERE session_id = ?
                 AND status = 'in_progress'
                 AND role = 'assistant'"#,
        )
        .bind(message)
        .bind(&now)
        .bind(session_id)
        .execute(pool)
        .await?
        .rows_affected()
    };
    // Flip running tool-call rows belonging to the turns we just errored —
    // otherwise they render as "Calling" forever (same orphaned-tool-call bug
    // the startup sweep fixes, but scoped to this session). The live worker's
    // own turn (`exempt`) is preserved so a genuinely in-flight call keeps
    // showing "Calling".
    let tool_msg = "Tool call interrupted — no worker is producing this response.";
    if let Some(exempt) = exempt_turn_id {
        sqlx::query(
            r#"UPDATE chat_tool_calls
               SET status = 'errored',
                   output_json = COALESCE(output_json, ?),
                   completed_at = ?
               WHERE status = 'running'
                 AND turn_id IN (
                     SELECT id FROM chat_turns
                     WHERE session_id = ? AND role = 'assistant' AND id != ?
                 )"#,
        )
        .bind(tool_msg)
        .bind(&now)
        .bind(session_id)
        .bind(exempt)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            r#"UPDATE chat_tool_calls
               SET status = 'errored',
                   output_json = COALESCE(output_json, ?),
                   completed_at = ?
               WHERE status = 'running'
                 AND turn_id IN (
                     SELECT id FROM chat_turns
                     WHERE session_id = ? AND role = 'assistant'
                 )"#,
        )
        .bind(tool_msg)
        .bind(&now)
        .bind(session_id)
        .execute(pool)
        .await?;
    }
    Ok(affected)
}

// ---------------------------------------------------------------------------
// Tool calls
