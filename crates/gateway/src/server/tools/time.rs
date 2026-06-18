// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Returns the current time in the caller's timezone. Falls back to
//! UTC if neither the caller's user record nor an explicit `timezone`
//! argument tells us where they are.
//!
//! The user's timezone lives in `users.timezone`, populated by
//! `POST /api/v0/me/timezone` once `app.js` reads
//! `Intl.DateTimeFormat().resolvedOptions().timeZone` on first authed
//! page load. The tool queries that row by `ctx.user_id` on each
//! invocation — cheap (microseconds against in-process SQLite) and
//! keeps the `ToolContext` shape free of per-tool scalars.
//!
//! Output is a struct rather than a formatted string so models that
//! do arithmetic ("how many days until Friday?") have `unix_seconds`
//! and models that format human-readable output have `iso8601`/`local`.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};
use crate::server::db::users;

pub struct CurrentTimestamp;

#[derive(Default, Deserialize)]
struct TimestampArgs {
    /// Optional IANA timezone override (`Europe/Berlin`). When unset
    /// we use the caller's stored timezone, falling back to UTC.
    #[serde(default)]
    timezone: Option<String>,
}

impl Tool for CurrentTimestamp {
    fn id(&self) -> &str {
        "get_current_timestamp"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Return the current date and time. Uses the user's timezone \
             by default; pass an explicit IANA timezone to override.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "timezone": {
                        "type": "string",
                        "description": "Optional IANA timezone override (e.g. 'Europe/Berlin', \
                                        'America/Los_Angeles'). Defaults to the user's timezone, \
                                        falling back to UTC."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            // Treat `null` like `{}` — many models call zero-arg tools
            // with the literal null instead of an empty object.
            let args: TimestampArgs = if args.is_null() {
                TimestampArgs::default()
            } else {
                serde_json::from_value(args).map_err(|e| {
                    ToolError::InvalidArgs(format!("expected optional timezone: {e}"))
                })?
            };

            // Precedence: explicit argument > user record > UTC. We
            // swallow DB errors here — a transient sqlite hiccup is
            // not worth failing the whole tool call when UTC is a
            // perfectly reasonable last-ditch answer.
            let stored_tz = users::find_by_id(&ctx.db, &ctx.user_id)
                .await
                .ok()
                .flatten()
                .and_then(|u| u.timezone);
            let requested = args
                .timezone
                .as_deref()
                .or(stored_tz.as_deref())
                .unwrap_or("UTC");

            let (label, tz) = match requested {
                "UTC" | "utc" => ("UTC".to_string(), TimeZone::UTC),
                name => {
                    let tz = TimeZone::get(name).map_err(|e| {
                        ToolError::InvalidArgs(format!("unknown timezone `{name}`: {e}"))
                    })?;
                    (name.to_string(), tz)
                }
            };

            let now = Timestamp::now();
            let zoned = now.to_zoned(tz);
            Ok(json!({
                "iso8601": zoned.timestamp().to_string(),
                "unix_seconds": now.as_second(),
                "timezone": label,
                "local": zoned.strftime("%Y-%m-%d %H:%M:%S %Z").to_string(),
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;
    use jiff::Timestamp;

    async fn fresh_db() -> db::Pool {
        db::open(std::path::Path::new(":memory:")).await.unwrap()
    }

    /// Helper: seed a user row (optionally with a stored timezone) and
    /// build a `ToolContext` pointing at it.
    async fn ctx_for_user(pool: &db::Pool, user_id: &str, stored_tz: Option<&str>) -> ToolContext {
        let now = Timestamp::now();
        users::upsert(
            pool,
            &users::User {
                id: user_id.into(),
                email: format!("{user_id}@example.com"),
                name: None,
                roles: vec![],
                created_at: now,
                updated_at: now,
                timezone: None,
            },
        )
        .await
        .unwrap();
        if let Some(tz) = stored_tz {
            users::set_timezone(pool, user_id, tz).await.unwrap();
        }
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

    #[tokio::test]
    async fn defaults_to_utc_when_user_has_no_stored_timezone() {
        let pool = fresh_db().await;
        let ctx = ctx_for_user(&pool, "u", None).await;
        let out = CurrentTimestamp.run(ctx, json!({})).await.unwrap();
        assert_eq!(out["timezone"], "UTC");
        assert!(out["iso8601"].as_str().unwrap().ends_with('Z'));
        assert!(out["unix_seconds"].is_number());
    }

    #[tokio::test]
    async fn uses_stored_timezone_when_no_arg() {
        let pool = fresh_db().await;
        let ctx = ctx_for_user(&pool, "u", Some("Europe/Berlin")).await;
        let out = CurrentTimestamp.run(ctx, json!({})).await.unwrap();
        assert_eq!(out["timezone"], "Europe/Berlin");
    }

    #[tokio::test]
    async fn arg_overrides_stored_timezone() {
        let pool = fresh_db().await;
        let ctx = ctx_for_user(&pool, "u", Some("Europe/Berlin")).await;
        let out = CurrentTimestamp
            .run(ctx, json!({"timezone": "America/Los_Angeles"}))
            .await
            .unwrap();
        assert_eq!(out["timezone"], "America/Los_Angeles");
    }

    #[tokio::test]
    async fn rejects_unknown_timezone() {
        let pool = fresh_db().await;
        let ctx = ctx_for_user(&pool, "u", None).await;
        let err = CurrentTimestamp
            .run(ctx, json!({"timezone": "Atlantis/Lost"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn null_args_treated_as_empty_object() {
        let pool = fresh_db().await;
        let ctx = ctx_for_user(&pool, "u", None).await;
        let out = CurrentTimestamp.run(ctx, Value::Null).await.unwrap();
        assert_eq!(out["timezone"], "UTC");
    }

    #[test]
    fn schema_names_match_id() {
        assert_eq!(
            CurrentTimestamp.id(),
            CurrentTimestamp.schema().function.name
        );
    }
}
