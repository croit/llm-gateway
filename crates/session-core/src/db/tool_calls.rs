// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

pub(crate) async fn next_tool_call_seq(pool: &Pool, turn_id: &str) -> Result<i64, DbError> {
    let row = sqlx::query(
        r#"SELECT COALESCE(MAX(seq), -1) + 1 AS next_seq
           FROM chat_tool_calls
           WHERE turn_id = ?"#,
    )
    .bind(turn_id)
    .fetch_one(pool)
    .await?;
    Ok(row.try_get("next_seq")?)
}

/// Insert a running tool call row. The model emits `id`,
/// `function.name`, and `function.arguments` (potentially across
/// multiple stream deltas) — the worker assembles those server-side
/// and inserts when ready to dispatch the tool.
pub async fn insert_running_tool_call(
    pool: &Pool,
    turn_id: &str,
    id: &str,
    name: &str,
    arguments_json: &str,
) -> Result<ToolCall, DbError> {
    let seq = next_tool_call_seq(pool, turn_id).await?;
    let now = Timestamp::now();
    let call = ToolCall {
        id: id.to_string(),
        turn_id: turn_id.to_string(),
        seq,
        name: name.to_string(),
        arguments_json: arguments_json.to_string(),
        output_json: None,
        status: ToolCallStatus::Running,
        created_at: now,
        completed_at: None,
    };
    sqlx::query(
        r#"INSERT INTO chat_tool_calls
              (id, turn_id, seq, name, arguments_json, status, created_at)
           VALUES (?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&call.id)
    .bind(&call.turn_id)
    .bind(call.seq)
    .bind(&call.name)
    .bind(&call.arguments_json)
    .bind(call.status.as_str())
    .bind(call.created_at.to_string())
    .execute(pool)
    .await?;
    Ok(call)
}

/// Max bytes of `output_json` we persist per tool call. `fetch_url`
/// can hand us a 4 MB text body; storing that verbatim in SQLite
/// per call bloats the DB fast and balloons the rows pulled back on
/// every chat-history list. The MODEL already saw the full payload
/// in the turn loop and synthesised its response off it — and the
/// openai_driver history replay rebuilds the upstream message list
/// from `chat_turns.content` (the assistant's prose), NOT from past
/// tool-call rows. So a cap here is invisible to upstream LLM
/// correctness; only the UI + audit log are affected, and both are
/// fine with a head + a "truncated" note.
///
/// 16 KB head matches the UI render cap (`TOOL_CALL_RENDER_CAP` in
/// `render.rs`); the symmetry means no further truncation is needed
/// downstream.
pub(crate) const PERSISTED_TOOL_OUTPUT_CAP: usize = 16 * 1024;

/// Cap `raw` to the persistence ceiling without splitting a UTF-8
/// codepoint. Strings under the cap pass through unchanged.
pub(crate) fn cap_tool_output(raw: &str) -> std::borrow::Cow<'_, str> {
    if raw.len() <= PERSISTED_TOOL_OUTPUT_CAP {
        return std::borrow::Cow::Borrowed(raw);
    }
    let head_end = raw
        .char_indices()
        .take_while(|(i, _)| *i <= PERSISTED_TOOL_OUTPUT_CAP)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(PERSISTED_TOOL_OUTPUT_CAP);
    let mut out = String::with_capacity(head_end + 128);
    out.push_str(&raw[..head_end]);
    out.push_str(&format!(
        "\n\n…\n(truncated by gateway at persist time: original {} bytes, \
         displayed first {} chars; the model saw the full payload before this \
         row was written)\n",
        raw.len(),
        head_end,
    ));
    std::borrow::Cow::Owned(out)
}

/// Stamp a tool call's output + flip status. Idempotent on the same
/// (id, output) pair: worker calls it exactly once per tool result.
pub async fn complete_tool_call(
    pool: &Pool,
    turn_id: &str,
    id: &str,
    output_json: &str,
    status: ToolCallStatus,
) -> Result<(), DbError> {
    if status == ToolCallStatus::Running {
        return Err(DbError::Decode {
            column: "status",
            source: anyhow::anyhow!("complete_tool_call called with status=running"),
        });
    }
    let capped = cap_tool_output(output_json);
    // Key on the full (turn_id, id) identity — `id` alone is only unique
    // within its turn.
    sqlx::query(
        r#"UPDATE chat_tool_calls
           SET output_json = ?, status = ?, completed_at = ?
           WHERE turn_id = ? AND id = ?"#,
    )
    .bind(capped.as_ref())
    .bind(status.as_str())
    .bind(Timestamp::now().to_string())
    .bind(turn_id)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
