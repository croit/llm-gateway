// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-user durable memory: the `remember` and `recall` tools.
//!
//! `remember` stores a short free-text fact about the caller; `recall`
//! pulls those facts back in a later conversation. Both are scoped to
//! `ctx.user_id`, so the model can only ever read/write the current
//! user's own memories — there is no cross-user path. State lives in
//! `db::user_memories`.
//!
//! These are intentionally low-risk: no code execution, no network, no
//! filesystem — just the gateway's own SQLite, keyed by the
//! authenticated user.

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};
use crate::server::db::user_memories::{self, MemoryKind};

/// Parse a caller-supplied `kind` string into a [`MemoryKind`].
/// `None`/absent → `Fact` (the generic bucket); an unrecognised string
/// is a hard error so typos surface instead of silently misfiling.
fn parse_kind(raw: Option<&str>) -> Result<MemoryKind, ToolError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(MemoryKind::Fact),
        Some(s) => MemoryKind::parse(s).ok_or_else(|| {
            ToolError::InvalidArgs(format!(
                "unknown kind `{s}` — use one of preference / project / fact"
            ))
        }),
    }
}

/// Safety cap on how many memories `recall` hands back in one call.
/// Recall intentionally returns *everything* (newest first) so the
/// model never has to guess a good query; this just bounds a runaway
/// store. Far above any realistic per-user count.
const MAX_RECALL_LIMIT: i64 = 200;

/// Upper bound on a single stored fact. Memory is for short facts, not
/// pasted documents — that's what attachments are for.
const MAX_CONTENT_LEN: usize = 2_000;

// ---------------------------------------------------------------------------
// remember

pub struct Remember;

#[derive(Deserialize)]
struct RememberArgs {
    content: String,
    #[serde(default)]
    kind: Option<String>,
}

impl Tool for Remember {
    fn id(&self) -> &str {
        "remember"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Store a short, durable fact about the user so you can recall it in future \
             conversations — e.g. their preferences, ongoing projects, or names they ask you to \
             keep. Use it when the user shares something worth remembering long-term. Do not \
             store secrets, passwords, or sensitive personal data unless the user explicitly \
             asks you to.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["content"],
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The fact to remember, as a single concise sentence \
                                        (e.g. 'Prefers answers in metric units')."
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["preference", "project", "fact"],
                        "description": "How to classify this memory: 'preference' for how the \
                                        user likes things, 'project' for context about what \
                                        they're working on, 'fact' for any other stable detail. \
                                        Defaults to 'fact'."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: RememberArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{content: string}}: {e}"))
            })?;
            let kind = parse_kind(args.kind.as_deref())?;
            let content = args.content.trim();
            if content.is_empty() {
                return Err(ToolError::InvalidArgs("content must not be empty".into()));
            }
            if content.len() > MAX_CONTENT_LEN {
                return Err(ToolError::InvalidArgs(format!(
                    "content too long ({} chars); keep memories under {MAX_CONTENT_LEN}",
                    content.len()
                )));
            }
            let row = user_memories::insert(&ctx.db, &ctx.user_id, kind, content)
                .await
                .map_err(|e| ToolError::Failed(format!("storing memory: {e}")))?;
            Ok(json!({
                "status": "remembered",
                "id": row.id,
                "kind": row.kind.as_str(),
                "content": row.content,
            }))
        })
    }
}

// ---------------------------------------------------------------------------
// recall

pub struct Recall;

impl Tool for Recall {
    fn id(&self) -> &str {
        "recall"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Retrieve everything you've remembered about this user (their preferences, project \
             context, and facts). Takes no arguments and returns all stored memories, newest \
             first — you don't need to craft a query. Call it whenever the user refers to \
             themselves, their preferences, or earlier context, then use whatever is relevant. \
             An empty result means nothing has been remembered yet.",
            json!({
                "type": "object",
                "properties": {}
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, _args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            // Deliberately ignore any arguments and return the user's
            // full memory set (newest first, bounded by a safety cap).
            // No filtering — the model reasons over everything rather
            // than guessing a query that has to lexically match.
            let rows = user_memories::recall_recent(&ctx.db, &ctx.user_id, None, MAX_RECALL_LIMIT)
                .await
                .map_err(|e| ToolError::Failed(format!("recalling memories: {e}")))?;

            let memories: Vec<Value> = rows
                .into_iter()
                .map(|m| {
                    json!({
                        "kind": m.kind.as_str(),
                        "content": m.content,
                        "remembered_at": m.created_at.to_string(),
                    })
                })
                .collect();
            Ok(json!({
                "count": memories.len(),
                "memories": memories,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;

    async fn ctx(pool: &db::Pool, user_id: &str) -> ToolContext {
        ToolContext {
            user_id: user_id.into(),
            roles: vec![],
            db: pool.clone(),
            s3: None,
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
        }
    }

    async fn fresh() -> db::Pool {
        db::open(std::path::Path::new(":memory:")).await.unwrap()
    }

    #[tokio::test]
    async fn remember_then_recall_roundtrips() {
        let pool = fresh().await;
        Remember
            .run(
                ctx(&pool, "alice").await,
                json!({"content": "prefers metric units"}),
            )
            .await
            .unwrap();
        let out = Recall
            .run(ctx(&pool, "alice").await, Value::Null)
            .await
            .unwrap();
        assert_eq!(out["count"], 1);
        assert_eq!(out["memories"][0]["content"], "prefers metric units");
    }

    #[tokio::test]
    async fn recall_returns_all_memories_ignoring_args() {
        let pool = fresh().await;
        let c = ctx(&pool, "alice").await;
        Remember
            .run(c.clone(), json!({"content": "runs a Ceph cluster"}))
            .await
            .unwrap();
        Remember
            .run(c.clone(), json!({"content": "likes dark mode"}))
            .await
            .unwrap();
        // A stray `query` arg is ignored — recall always returns all.
        let out = Recall.run(c, json!({"query": "ceph"})).await.unwrap();
        assert_eq!(out["count"], 2);
    }

    #[tokio::test]
    async fn recall_is_scoped_to_the_caller() {
        let pool = fresh().await;
        Remember
            .run(
                ctx(&pool, "alice").await,
                json!({"content": "alice secret"}),
            )
            .await
            .unwrap();
        let out = Recall
            .run(ctx(&pool, "bob").await, Value::Null)
            .await
            .unwrap();
        assert_eq!(out["count"], 0);
    }

    #[tokio::test]
    async fn remember_stores_kind_and_recall_reports_it() {
        let pool = fresh().await;
        let c = ctx(&pool, "alice").await;
        Remember
            .run(
                c.clone(),
                json!({"content": "metric units", "kind": "preference"}),
            )
            .await
            .unwrap();
        let out = Recall.run(c, Value::Null).await.unwrap();
        assert_eq!(out["count"], 1);
        assert_eq!(out["memories"][0]["kind"], "preference");
        assert_eq!(out["memories"][0]["content"], "metric units");
    }

    #[tokio::test]
    async fn remember_rejects_unknown_kind() {
        let pool = fresh().await;
        let err = Remember
            .run(
                ctx(&pool, "alice").await,
                json!({"content": "x", "kind": "bogus"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn remember_rejects_empty_content() {
        let pool = fresh().await;
        let err = Remember
            .run(ctx(&pool, "alice").await, json!({"content": "   "}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "got {err:?}");
    }

    #[test]
    fn schema_names_match_ids() {
        assert_eq!(Remember.id(), Remember.schema().function.name);
        assert_eq!(Recall.id(), Recall.schema().function.name);
    }
}
