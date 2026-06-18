// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Smoke-test tool: echoes its `message` argument back. Useful for verifying
//! tool-injection + tool-call loop end-to-end without depending on any company
//! integration.

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};

pub struct Echo;

#[derive(Deserialize)]
struct EchoArgs {
    message: String,
}

impl Tool for Echo {
    fn id(&self) -> &str {
        "company_echo"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Echo a message back verbatim. Useful for smoke tests.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["message"],
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The text to echo back."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, _ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: EchoArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{message: string}}: {e}"))
            })?;
            Ok(json!({ "message": args.message }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ctx() -> ToolContext {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: pool,
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

    #[tokio::test]
    async fn echo_returns_the_message() {
        let out = Echo
            .run(ctx().await, json!({"message": "hello"}))
            .await
            .unwrap();
        assert_eq!(out, json!({"message": "hello"}));
    }

    #[tokio::test]
    async fn echo_rejects_missing_message() {
        let err = Echo.run(ctx().await, json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn echo_rejects_wrong_type() {
        let err = Echo
            .run(ctx().await, json!({"message": 42}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn schema_names_match_id() {
        assert_eq!(Echo.id(), Echo.schema().function.name);
    }
}
