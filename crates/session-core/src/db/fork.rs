// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

/// Copy an entire conversation into `new_user_id`'s account as a fresh,
/// **private** session (re-sharing is the new owner's decision). Title
/// and turn history are copied 1-to-1; every turn (and its tool calls)
/// gets a fresh id. Runs in one transaction.
///
/// Attachment markers in the copied turn text are rewritten so their
/// `/chat/attachment/<turn>/<file>` proxy URLs point at the *new* turn
/// ids — otherwise the fork's bubbles would reference the original
/// owner's turns and break the moment they un-share or delete. The
/// returned [`AttachmentCopy`] list tells the caller which blob objects
/// to duplicate (deduped, so a file referenced twice copies once); the
/// bytes themselves live in S3, which this crate doesn't touch.
///
/// An `in_progress` turn (a shared chat forked mid-stream) is copied as
/// `errored`, never live — the fork has no worker driving it, so a
/// copied spinner would hang forever.
pub async fn fork_session(
    pool: &Pool,
    src: &Session,
    new_user_id: &str,
) -> Result<(Session, Vec<AttachmentCopy>), DbError> {
    let src_turns = list_turns(pool, &src.id).await?;

    // Pre-mint every new turn id up front: a composer attachment's proxy
    // URL keys off the assistant turn id, which can be a *different* turn
    // than the one whose text carries the marker, so we need the whole
    // old→new map available while rewriting any single turn.
    let id_map: std::collections::HashMap<String, String> = src_turns
        .iter()
        .map(|t| (t.turn.id.clone(), Uuid::new_v4().to_string()))
        .collect();

    let now = Timestamp::now();
    let new_session = Session {
        id: Uuid::new_v4().to_string(),
        user_id: new_user_id.to_string(),
        title: src.title.clone(),
        created_at: now,
        updated_at: now,
        shared: false,
        // A fork starts unpinned — pinning, like re-sharing, is the new
        // owner's decision.
        pinned: false,
    };

    // Collect the blob copies as we go, deduped on (source turn, file):
    // the same object can be referenced by markers in more than one turn.
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut copies: Vec<AttachmentCopy> = Vec::new();
    let mut record = |text: &str| {
        for att in crate::attachments::parse_markers(text) {
            let Some(old_turn) = crate::attachments::proxy_url_turn_id(&att.url) else {
                continue;
            };
            let Some(new_turn) = id_map.get(old_turn) else {
                continue;
            };
            if seen.insert((old_turn.to_string(), att.filename.clone())) {
                copies.push(AttachmentCopy {
                    from_turn_id: old_turn.to_string(),
                    to_turn_id: new_turn.clone(),
                    filename: att.filename,
                });
            }
        }
    };

    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"INSERT INTO chat_sessions (id, user_id, title, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?)"#,
    )
    .bind(&new_session.id)
    .bind(&new_session.user_id)
    .bind(new_session.title.as_deref())
    .bind(new_session.created_at.to_string())
    .bind(new_session.updated_at.to_string())
    .execute(&mut *tx)
    .await?;

    for tw in &src_turns {
        let turn = &tw.turn;
        let new_turn_id = id_map.get(&turn.id).expect("minted for every src turn");

        let user_content = turn.user_content.as_ref().map(|t| {
            record(t);
            crate::attachments::remap_attachment_turn_ids(t, &id_map)
        });
        let content = turn.content.as_ref().map(|t| {
            record(t);
            crate::attachments::remap_attachment_turn_ids(t, &id_map)
        });

        // Never copy an in-progress turn as live — no worker drives the
        // fork, so it would spin forever. Stamp it errored + completed.
        let (status, completed_at) = if turn.status == TurnStatus::InProgress {
            (TurnStatus::Errored, Some(now))
        } else {
            (turn.status, turn.completed_at)
        };

        sqlx::query(
            r#"INSERT INTO chat_turns
                  (id, session_id, seq, role, user_content, model, content,
                   reasoning, reasoning_elapsed_ms, reasoning_started_at,
                   status, error_message,
                   created_at, completed_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(new_turn_id)
        .bind(&new_session.id)
        .bind(turn.seq)
        .bind(turn.role.as_str())
        .bind(user_content)
        .bind(turn.model.as_deref())
        .bind(content)
        .bind(turn.reasoning.as_deref())
        .bind(turn.reasoning_elapsed_ms)
        .bind(turn.reasoning_started_at.map(|t| t.to_string()))
        .bind(status.as_str())
        .bind(turn.error_message.as_deref())
        .bind(turn.created_at.to_string())
        .bind(completed_at.map(|t| t.to_string()))
        .execute(&mut *tx)
        .await?;

        for tc in &tw.tool_calls {
            // Tool-call identity is (turn_id, id); the copy lands under a fresh
            // turn id, so the source id is preserved without any risk of
            // colliding with the original or a prior fork.
            sqlx::query(
                r#"INSERT INTO chat_tool_calls
                      (id, turn_id, seq, name, arguments_json, output_json,
                       status, created_at, completed_at)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            )
            .bind(&tc.id)
            .bind(new_turn_id)
            .bind(tc.seq)
            .bind(&tc.name)
            .bind(&tc.arguments_json)
            .bind(tc.output_json.as_deref())
            .bind(tc.status.as_str())
            .bind(tc.created_at.to_string())
            .bind(tc.completed_at.map(|t| t.to_string()))
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;

    Ok((new_session, copies))
}
