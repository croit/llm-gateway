// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `list_attachments` — enumerate every file in the conversation.
//!
//! Models regenerate assets they already produced because the
//! `<turn>/<file>` ids scattered across earlier turns are hard to keep
//! track of. This tool gives one cheap, always-current inventory of the
//! session's attachments — user uploads and tool outputs alike (sandbox
//! artifacts, rendered documents, generated images and QR codes) — so the
//! model can find and REUSE an existing file (by id or filename, see
//! `chat_attachments::resolve_attachment`) instead of building it again.
//!
//! Reads only the session's own turn markers (the same enumeration the
//! sandbox staging trusts for session scoping); no S3 access.

use serde_json::{Value, json};
use shared::api::ToolDef;

use gateway_core::server::chat_attachments;
use gateway_core::server::tools::{Tool, ToolContext, ToolError, ToolFuture};

pub struct ListAttachments;

impl Tool for ListAttachments {
    fn id(&self) -> &str {
        "list_attachments"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "List every file in this conversation — user uploads AND files \
             your earlier tool calls produced (sandbox artifacts, rendered \
             documents, generated images/QR codes) — newest first, with each \
             file's id, filename, mime type, and size. Use it to REUSE an \
             existing file instead of regenerating it: pass the id (or just \
             the filename) to `attachments` in sandbox/render calls, to \
             `fetch_attachment` to read it, or as a `logo_attachment_id`.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, _args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let session_id = ctx.session_id.as_deref().ok_or_else(|| {
                ToolError::Failed(
                    "list_attachments only works inside a chat session — there is no \
                     conversation to enumerate here"
                        .into(),
                )
            })?;
            let atts = chat_attachments::list_session_attachments(&ctx.db, session_id)
                .await
                .map_err(|e| ToolError::Failed(format!("listing attachments: {e}")))?;
            // The enumeration is oldest-first (turn order); newest-first is
            // the useful reading order for "what did I just make".
            let items: Vec<Value> = atts
                .iter()
                .rev()
                .map(|a| {
                    json!({
                        "id": a.id,
                        "filename": a.filename,
                        "mime": a.mime,
                        "size": a.size,
                    })
                })
                .collect();
            Ok(json!({
                "count": items.len(),
                "attachments": items,
                "note": "Reuse these instead of regenerating: pass an `id` (or just the \
                         filename — newest match wins) to `attachments`, `fetch_attachment`, \
                         or `logo_attachment_id`.",
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names_match_id() {
        assert_eq!(ListAttachments.id(), ListAttachments.schema().function.name);
    }

    async fn pool_with_session() -> gateway_core::server::db::Pool {
        let pool = gateway_core::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        sqlx::query(
            r#"INSERT INTO users (id, email, created_at, updated_at)
               VALUES ('u1', 'u1@example.com', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO chat_sessions (id, user_id, created_at, updated_at)
               VALUES ('s1', 'u1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn insert_turn(
        pool: &gateway_core::server::db::Pool,
        id: &str,
        seq: i64,
        role: &str,
        content: &str,
    ) {
        let (user_content, content_col) = if role == "user" {
            (Some(content), None)
        } else {
            (None, Some(content))
        };
        sqlx::query(
            r#"INSERT INTO chat_turns (id, session_id, seq, role, user_content, content,
                                       status, created_at)
               VALUES (?, 's1', ?, ?, ?, ?, 'completed', '2026-01-01T00:00:00Z')"#,
        )
        .bind(id)
        .bind(seq)
        .bind(role)
        .bind(user_content)
        .bind(content_col)
        .execute(pool)
        .await
        .unwrap();
    }

    fn ctx(pool: gateway_core::server::db::Pool, session: Option<&str>) -> ToolContext {
        ToolContext {
            user_id: "u1".into(),
            roles: vec![],
            db: pool,
            s3: None,
            assistant_turn_id: None,
            session_id: session.map(str::to_string),
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
            image_gen: None,
            sandbox_lease: None,
            crypto: None,
        }
    }

    #[tokio::test]
    async fn lists_uploads_and_tool_outputs_newest_first() {
        let pool = pool_with_session().await;
        // t1: a user upload; t2: an assistant turn whose content carries a
        // tool-produced attachment marker (the same marker the sandbox /
        // generate_qr_code delivery splices in).
        let up = session_core::attachments::marker_line(
            "data.csv",
            "text/csv",
            "/chat/attachment/t1/data.csv",
            10,
        );
        insert_turn(&pool, "t1", 0, "user", &format!("here you go\n\n{up}\n")).await;
        let qr = session_core::attachments::marker_line(
            "qr-code.png",
            "image/png",
            "/chat/attachment/t2/qr-code.png",
            288,
        );
        insert_turn(&pool, "t2", 1, "assistant", &format!("done\n\n{qr}\n")).await;

        let out = ListAttachments
            .run(ctx(pool, Some("s1")), json!({}))
            .await
            .unwrap();
        assert_eq!(out["count"], 2);
        let items = out["attachments"].as_array().unwrap();
        // Newest first: the tool output before the older upload.
        assert_eq!(items[0]["id"], "t2/qr-code.png");
        assert_eq!(items[0]["mime"], "image/png");
        assert_eq!(items[1]["id"], "t1/data.csv");
    }

    #[tokio::test]
    async fn errors_cleanly_without_a_session() {
        let pool = pool_with_session().await;
        let err = ListAttachments
            .run(ctx(pool, None), json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)), "{err:?}");
    }
}
