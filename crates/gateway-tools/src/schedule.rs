// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `schedule_action` / `list_scheduled_actions` / `delete_scheduled_action` —
//! let the model set up recurring prompts on the user's behalf.
//!
//! The cron stack itself is not new: the `/scheduled` page, the
//! `scheduled_actions` table and the worker that fires due actions have all
//! been running. What was missing was any way for the model to reach it, so
//! "remind me about the backup report on Mondays" could only be done by the
//! user clicking through a form — in the middle of a conversation where the
//! model already knew exactly what to schedule.
//!
//! ## Why these tools are careful
//!
//! A scheduled action later runs **as the user**, unattended, forever. That
//! makes it a different kind of side effect from every other tool here: a
//! model steered by an attacker-controlled web page or attachment could
//! otherwise plant a recurring prompt that outlives the conversation it was
//! injected into. The `/webhooks` page takes the same position for the same
//! reason (its actions default to tools off, because an anonymous caller
//! feeds text to a model running as you).
//!
//! So:
//!
//!   * **Tools are always off** for an action created here. The action can
//!     ask the model to write something; it cannot make it act. Turning tools
//!     on is a deliberate click on `/scheduled`.
//!   * **Creating and deleting need a human "yes"**, via
//!     [`crate::ask_user::confirm`]. That also makes them chat-only —
//!     `requires_chat_session` keeps them off the `/v1` tool list, and a
//!     scheduled action can't create more scheduled actions (the headless
//!     worker has a session but nobody watching it, so the confirmation gets
//!     no answer and the write is refused).
//!   * **Everything is scoped to `ctx.user_id`**, never to an id from the
//!     arguments — the store's functions all take the owner as a parameter.
//!
//! Listing is read-only and stays available everywhere.
//!
//! ## Why the response carries a preview
//!
//! `Cron::describe()` plus the next three run times are the same confirmation
//! the UI shows. Without them the model can only echo the cron expression back
//! at the user, and a wrong-but-valid expression (`0 0 * * 0` when they said
//! Monday) stays undetected until the first missed run.

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use gateway_core::server::db::users;
use gateway_runtime::server::scheduled::{self, NewAction, cron::Cron};
use gateway_runtime::server::tools::{Tool, ToolContext, ToolError, ToolFuture};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::ask_user::{Confirmation, confirm};

/// Bounds mirroring the `/scheduled` form's own validation, so an action
/// created here is one the edit form can also round-trip.
const MAX_NAME_LEN: usize = 128;
const MAX_PROMPT_LEN: usize = 8000;

/// How many upcoming run times to return. Three is what the UI previews:
/// enough to see the *pattern* (daily vs weekdays vs Mondays), which is what
/// catches a wrong expression.
const PREVIEW_RUNS: usize = 3;

/// Ceiling on the per-tool timeout, since these tools wait for a human. Must
/// exceed `ask_user`'s own wait or the runner would cancel the tool while the
/// confirmation card is still up.
const MAX_DURATION_SECS: u64 = 210;

// ---------------------------------------------------------------------------
// shared helpers

fn max_duration() -> Option<std::time::Duration> {
    Some(std::time::Duration::from_secs(MAX_DURATION_SECS))
}

/// The user's stored timezone, or UTC. Same source `get_current_timestamp`
/// reads, so "every day at 07:00" means the same thing in both tools.
async fn default_timezone(ctx: &ToolContext) -> String {
    users::find_by_id(&ctx.db, &ctx.user_id)
        .await
        .ok()
        .flatten()
        .and_then(|u| u.timezone)
        .unwrap_or_else(|| "UTC".to_string())
}

fn resolve_tz(name: &str) -> Result<TimeZone, ToolError> {
    TimeZone::get(name).map_err(|_| {
        ToolError::InvalidArgs(format!(
            "unknown timezone `{name}` — use an IANA name like `Europe/Berlin` or `UTC`"
        ))
    })
}

/// Render an action for the model: the fields it needs to describe or change
/// the schedule, plus the same human preview the UI shows.
fn action_json(action: &scheduled::ScheduledAction) -> Value {
    let tz = TimeZone::get(&action.timezone).unwrap_or(TimeZone::UTC);
    let parsed = Cron::parse(&action.cron).ok();
    let upcoming: Vec<String> = parsed
        .as_ref()
        .map(|c| {
            c.upcoming(Timestamp::now(), &tz, PREVIEW_RUNS)
                .iter()
                .map(|t| {
                    t.to_zoned(tz.clone())
                        .strftime("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": action.id,
        "name": action.name,
        "prompt": action.prompt,
        "model": action.model,
        "cron": action.cron,
        "timezone": action.timezone,
        "schedule": parsed.as_ref().map(Cron::describe),
        "next_runs": upcoming,
        "enabled": action.enabled,
        "tools_enabled": action.tools_enabled,
        "last_run_at": action.last_run_at.map(|t| t.to_string()),
        "last_status": action.last_status,
    })
}

/// The wording of the "no human answered" refusal, shared by create + delete.
fn no_answer_error(what: &str) -> ToolError {
    ToolError::Failed(format!(
        "nobody confirmed, so nothing was {what}. Scheduled actions run as the user \
         later on, unattended, so this needs an explicit yes from a person who is \
         watching. Tell the user what you were about to do and ask them to confirm \
         in a normal message, or point them at the /scheduled page."
    ))
}

// ---------------------------------------------------------------------------
// schedule_action

pub struct ScheduleAction;

#[derive(Deserialize)]
struct CreateArgs {
    name: String,
    prompt: String,
    cron: String,
    #[serde(default)]
    timezone: Option<String>,
}

impl Tool for ScheduleAction {
    fn id(&self) -> &str {
        "schedule_action"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        max_duration()
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Schedule a prompt to run automatically on a recurring schedule — \
             \"every Monday at 8, summarise last week's tickets\", \"remind me \
             about the backup report on the first of the month\". Each run opens \
             a conversation the user can read afterwards, using the model you are \
             running on now. \
             \
             The user has to confirm before anything is saved, so use this when \
             they asked for something recurring — not to set yourself reminders. \
             The scheduled run has NO tools available (it can write, not act); if \
             the task needs tools, say so and point the user at the /scheduled \
             page. Say the schedule back to the user using the `schedule` and \
             `next_runs` in the result, so a wrong day or hour gets caught now \
             rather than at the first run.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "prompt", "cron"],
                "properties": {
                    "name": {
                        "type": "string",
                        "description": format!(
                            "Short label the user will recognise in their list, e.g. \
                             \"Weekly ticket summary\". Max {MAX_NAME_LEN} characters."
                        )
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The message to send at each run. Write it as a \
                                        standalone instruction — the run starts a fresh \
                                        conversation and cannot see this one, so include \
                                        every detail it needs."
                    },
                    "cron": {
                        "type": "string",
                        "description": "5-field cron expression: minute hour day-of-month \
                                        month day-of-week. `0 8 * * 1` = Mondays at 08:00, \
                                        `30 6 * * 1-5` = weekdays at 06:30, `0 9 1 * *` = \
                                        the 1st of each month at 09:00. Ranges, lists and \
                                        `*/n` steps are supported. Day-of-week: 0 or 7 = \
                                        Sunday, 1 = Monday."
                    },
                    "timezone": {
                        "type": "string",
                        "description": "IANA timezone the schedule is evaluated in, e.g. \
                                        `Europe/Berlin`. Defaults to the user's own \
                                        timezone — only set it if they ask for another."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: CreateArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{name: string, prompt: string, cron: string, \
                     timezone?: string}}: {e}"
                ))
            })?;

            let name = args.name.trim();
            if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
                return Err(ToolError::InvalidArgs(format!(
                    "`name` must be 1-{MAX_NAME_LEN} characters"
                )));
            }
            let prompt = args.prompt.trim();
            if prompt.is_empty() || prompt.chars().count() > MAX_PROMPT_LEN {
                return Err(ToolError::InvalidArgs(format!(
                    "`prompt` must be 1-{MAX_PROMPT_LEN} characters"
                )));
            }

            // The model this turn runs on. Without it we'd have to invent a
            // pool id, and an action pointing at a model that doesn't exist
            // fails silently at 06:00 rather than here.
            let model = ctx.model.clone().ok_or_else(|| {
                ToolError::Failed(
                    "this request path doesn't know which model it is running, so the \
                     action would have no model to run with. Ask the user to create it \
                     on the /scheduled page."
                        .into(),
                )
            })?;

            let timezone = match args.timezone.as_deref().map(str::trim) {
                Some(tz) if !tz.is_empty() => tz.to_string(),
                _ => default_timezone(&ctx).await,
            };
            let tz = resolve_tz(&timezone)?;

            // Parse before asking for confirmation: a bad expression is the
            // model's problem to fix, and the error names the field, so it can
            // correct itself instead of guessing. Bothering the user with a
            // card for a prompt that can't be saved would be worse.
            let cron = Cron::parse(&args.cron).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "{e}. Fields are: minute hour day-of-month month day-of-week."
                ))
            })?;
            let next_runs = cron.upcoming(Timestamp::now(), &tz, PREVIEW_RUNS);
            if next_runs.is_empty() {
                return Err(ToolError::InvalidArgs(format!(
                    "`{}` never occurs (e.g. February 30th), so it would never run. \
                     Pick a schedule that has a next occurrence.",
                    cron.as_str()
                )));
            }
            let pretty: Vec<String> = next_runs
                .iter()
                .map(|t| {
                    t.to_zoned(tz.clone())
                        .strftime("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .collect();

            // The user sees what will be created — schedule in words, the next
            // run, and the timezone — before it exists.
            let question = format!(
                "Schedule “{name}”? {} First run {} ({timezone}). It will run without \
                 tools, and you can change or delete it under Scheduled.",
                cron.describe(),
                pretty[0],
            );
            match confirm(
                &ctx,
                &question,
                "Schedule this?",
                "Yes, schedule it",
                "No, don't",
            )
            .await
            {
                Confirmation::Approved => {}
                Confirmation::Declined { text } => {
                    return Ok(json!({
                        "created": false,
                        "reason": "declined",
                        "user_said": text,
                        "status": "The user did not want this scheduled. Do not create it, \
                                   and do not ask again unless they bring it up. If they \
                                   said what to change, offer the corrected version.",
                    }));
                }
                Confirmation::NoAnswer => return Err(no_answer_error("scheduled")),
            }

            let action = scheduled::create(
                &ctx.db,
                NewAction {
                    user_id: ctx.user_id.clone(),
                    name: name.to_string(),
                    prompt: prompt.to_string(),
                    model,
                    cron: cron.as_str().to_string(),
                    timezone,
                    // Never from the model: see the module docs. A model-created
                    // action can write, not act.
                    tools_enabled: false,
                    // Each run starts fresh, so a run can't accumulate context
                    // from earlier runs the user never looked at.
                    reuse_conversation: false,
                    reuse_rounds: 1,
                    next_run_at: Some(next_runs[0]),
                },
            )
            .await
            .map_err(|e| ToolError::Failed(format!("saving the scheduled action: {e}")))?;

            let mut out = action_json(&action);
            out["created"] = json!(true);
            out["status"] = json!(
                "Scheduled. Tell the user the schedule in words and when it first runs, \
                 and that it runs without tools. They can change or remove it under \
                 Scheduled."
            );
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------------------
// list_scheduled_actions

pub struct ListScheduledActions;

impl Tool for ListScheduledActions {
    fn id(&self) -> &str {
        "list_scheduled_actions"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "List the user's scheduled actions — id, name, prompt, schedule in \
             words, the next runs, and how the last run went. Use it to answer \
             \"what have I got scheduled?\", to check whether something is \
             already scheduled before creating a duplicate, or to find the id to \
             pass to `delete_scheduled_action`.",
            json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, _args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let actions = scheduled::list_for_user(&ctx.db, &ctx.user_id)
                .await
                .map_err(|e| ToolError::Failed(format!("listing scheduled actions: {e}")))?;
            let items: Vec<Value> = actions.iter().map(action_json).collect();
            Ok(json!({
                "actions": items,
                "count": items.len(),
            }))
        })
    }
}

// ---------------------------------------------------------------------------
// delete_scheduled_action

pub struct DeleteScheduledAction;

#[derive(Deserialize)]
struct DeleteArgs {
    id: String,
}

impl Tool for DeleteScheduledAction {
    fn id(&self) -> &str {
        "delete_scheduled_action"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        max_duration()
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Delete one of the user's scheduled actions, by the `id` from \
             `list_scheduled_actions`. The user has to confirm first. Deletion is \
             permanent — the action and its schedule are gone, though the \
             conversations its past runs produced stay. Only use it when the user \
             asked for something to stop.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The action's id, from `list_scheduled_actions`."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: DeleteArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{id: string}}: {e}")))?;
            let id = args.id.trim();
            if id.is_empty() {
                return Err(ToolError::InvalidArgs("`id` is required".into()));
            }

            // Scoped read first: this both tells us the name (for a
            // confirmation the user can actually judge) and makes another
            // user's action indistinguishable from a nonexistent one.
            let action = scheduled::get(&ctx.db, &ctx.user_id, id)
                .await
                .map_err(|e| ToolError::Failed(format!("reading the scheduled action: {e}")))?
                .ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "no scheduled action `{id}` — call `list_scheduled_actions` \
                         to see the ids"
                    ))
                })?;

            let question = format!(
                "Delete the scheduled action “{}”? It runs {} This can't be undone.",
                action.name,
                Cron::parse(&action.cron)
                    .map(|c| c.describe())
                    .unwrap_or_else(|_| format!("on `{}`.", action.cron)),
            );
            match confirm(
                &ctx,
                &question,
                "Delete this schedule?",
                "Yes, delete it",
                "No, keep it",
            )
            .await
            {
                Confirmation::Approved => {}
                Confirmation::Declined { text } => {
                    return Ok(json!({
                        "deleted": false,
                        "reason": "declined",
                        "user_said": text,
                        "id": action.id,
                        "name": action.name,
                        "status": "The user kept it. Leave it alone.",
                    }));
                }
                Confirmation::NoAnswer => return Err(no_answer_error("deleted")),
            }

            let removed = scheduled::delete(&ctx.db, &ctx.user_id, id)
                .await
                .map_err(|e| ToolError::Failed(format!("deleting the scheduled action: {e}")))?;
            Ok(json!({
                "deleted": removed,
                "id": action.id,
                "name": action.name,
                "status": if removed {
                    "Deleted. It will not run again."
                } else {
                    "It was already gone — nothing to delete."
                },
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_core::server::db;

    async fn seeded() -> db::Pool {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        sqlx::query(
            r#"INSERT INTO users (id, email, timezone, created_at, updated_at)
               VALUES ('u1', 'u1@example.com', 'Europe/Berlin',
                       '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// A context off the chat path: a model, but no `chat_feedback`, so a
    /// confirmation can never be answered.
    fn ctx_unattended(pool: &db::Pool) -> ToolContext {
        ToolContext {
            user_id: "u1".into(),
            model: Some("qwen-32b".into()),
            ..ToolContext::for_test(pool.clone())
        }
    }

    fn create_args() -> Value {
        json!({
            "name": "Weekly summary",
            "prompt": "Summarise last week's tickets.",
            "cron": "0 8 * * 1",
        })
    }

    /// The safety property: with nobody watching, nothing is written. This is
    /// what stops a scheduled action (or a /v1 caller) from planting more
    /// scheduled actions.
    #[tokio::test]
    async fn unattended_creation_is_refused_and_writes_nothing() {
        let pool = seeded().await;
        let err = ScheduleAction
            .run(ctx_unattended(&pool), create_args())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nobody confirmed"), "{err}");
        assert!(
            scheduled::list_for_user(&pool, "u1")
                .await
                .unwrap()
                .is_empty(),
            "an unconfirmed action must not be stored"
        );
    }

    /// Same for deletion — an unanswered confirmation must leave the action
    /// in place.
    #[tokio::test]
    async fn unattended_deletion_is_refused_and_keeps_the_action() {
        let pool = seeded().await;
        let action = scheduled::create(
            &pool,
            NewAction {
                user_id: "u1".into(),
                name: "Nightly".into(),
                prompt: "check".into(),
                model: "qwen-32b".into(),
                cron: "0 3 * * *".into(),
                timezone: "UTC".into(),
                tools_enabled: false,
                reuse_conversation: false,
                reuse_rounds: 1,
                next_run_at: None,
            },
        )
        .await
        .unwrap();

        let err = DeleteScheduledAction
            .run(ctx_unattended(&pool), json!({"id": action.id}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nobody confirmed"), "{err}");
        assert_eq!(
            scheduled::list_for_user(&pool, "u1").await.unwrap().len(),
            1
        );
    }

    /// A context that looks like a live chat turn: a broadcast channel with a
    /// subscriber (so the card is considered deliverable) and the two feedback
    /// hubs. Returns the context plus the ask hub, so a test can answer the
    /// confirmation the way `POST /api/v0/me/ask/feedback/{turn}` does.
    fn ctx_watched(
        pool: &db::Pool,
    ) -> (
        ToolContext,
        std::sync::Arc<
            gateway_runtime::server::tools::feedback::FeedbackHub<
                gateway_runtime::server::tools::feedback::AskReply,
            >,
        >,
        tokio::sync::broadcast::Receiver<session_core::workers::TurnUpdate>,
    ) {
        use gateway_runtime::server::tools::{ChatFeedback, feedback::FeedbackHub};
        let (broadcast, rx) = tokio::sync::broadcast::channel(16);
        let ask_hub = std::sync::Arc::new(FeedbackHub::default());
        let ctx = ToolContext {
            user_id: "u1".into(),
            model: Some("qwen-32b".into()),
            assistant_turn_id: Some("t1".into()),
            session_id: Some("s1".into()),
            chat_feedback: Some(ChatFeedback {
                broadcast,
                hub: std::sync::Arc::new(FeedbackHub::default()),
                ask_hub: ask_hub.clone(),
                secure: true,
            }),
            ..ToolContext::for_test(pool.clone())
        };
        // The receiver is returned (not dropped) so `receiver_count()` stays
        // non-zero for the life of the test — dropping it would make the tool
        // conclude nobody is watching.
        (ctx, ask_hub, rx)
    }

    /// Answer the confirmation card once the tool has parked on the hub.
    /// Mirrors what the feedback endpoint does.
    async fn answer(
        hub: &gateway_runtime::server::tools::feedback::FeedbackHub<
            gateway_runtime::server::tools::feedback::AskReply,
        >,
        choice: &str,
    ) {
        use gateway_runtime::server::tools::feedback::AskReply;
        for _ in 0..200 {
            if hub.resolve(
                "t1",
                AskReply::Answered {
                    choices: vec![choice.to_string()],
                    text: None,
                },
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the tool never parked on the ask hub");
    }

    /// The confirmed path: the action is written, it carries the turn's model
    /// and the user's timezone, tools are off, and the result hands the model
    /// the human schedule + next runs to read back.
    #[tokio::test]
    async fn a_confirmed_action_is_created_with_tools_off_and_a_preview() {
        let pool = seeded().await;
        let (ctx, hub, _rx) = ctx_watched(&pool);

        let (out, ()) = tokio::join!(ScheduleAction.run(ctx, create_args()), async {
            answer(&hub, "Yes, schedule it").await
        });
        let out = out.unwrap();

        assert_eq!(out["created"], true, "{out:?}");
        assert_eq!(out["cron"], "0 8 * * 1", "{out:?}");
        assert_eq!(
            out["timezone"], "Europe/Berlin",
            "must default to users.timezone: {out:?}"
        );
        assert_eq!(
            out["model"], "qwen-32b",
            "must inherit the turn's model: {out:?}"
        );
        assert_eq!(
            out["tools_enabled"], false,
            "a model-created action must never have tools: {out:?}"
        );
        assert!(out["schedule"].is_string(), "{out:?}");
        assert_eq!(out["next_runs"].as_array().unwrap().len(), PREVIEW_RUNS);

        // And it really is in the store, owned by the caller.
        let stored = scheduled::list_for_user(&pool, "u1").await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "Weekly summary");
        assert!(!stored[0].tools_enabled);
        assert!(stored[0].next_run_at.is_some(), "must be armed to fire");
    }

    /// Declining writes nothing and tells the model to drop it — the case
    /// where the confirmation is doing its actual job.
    #[tokio::test]
    async fn a_declined_action_is_not_created() {
        let pool = seeded().await;
        let (ctx, hub, _rx) = ctx_watched(&pool);

        let (out, ()) = tokio::join!(ScheduleAction.run(ctx, create_args()), async {
            answer(&hub, "No, don't").await
        });
        let out = out.unwrap();

        assert_eq!(out["created"], false, "{out:?}");
        assert_eq!(out["reason"], "declined", "{out:?}");
        assert!(
            scheduled::list_for_user(&pool, "u1")
                .await
                .unwrap()
                .is_empty(),
            "a declined action must not be stored"
        );
    }

    /// Free text is a change request, not consent: "yes, but at 07:00" must
    /// not be read as approval.
    #[tokio::test]
    async fn typing_instead_of_clicking_yes_is_not_approval() {
        use gateway_runtime::server::tools::feedback::AskReply;
        let pool = seeded().await;
        let (ctx, hub, _rx) = ctx_watched(&pool);

        let (out, ()) = tokio::join!(ScheduleAction.run(ctx, create_args()), async {
            for _ in 0..200 {
                if hub.resolve(
                    "t1",
                    AskReply::Answered {
                        choices: vec![],
                        text: Some("yes but make it 07:00".into()),
                    },
                ) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            panic!("the tool never parked on the ask hub");
        });
        let out = out.unwrap();

        assert_eq!(out["created"], false, "{out:?}");
        assert_eq!(
            out["user_said"], "yes but make it 07:00",
            "the correction must reach the model: {out:?}"
        );
        assert!(
            scheduled::list_for_user(&pool, "u1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Deletion goes through the same gate, and a confirmed one really removes
    /// the row.
    #[tokio::test]
    async fn a_confirmed_deletion_removes_the_action() {
        let pool = seeded().await;
        let action = scheduled::create(
            &pool,
            NewAction {
                user_id: "u1".into(),
                name: "Nightly".into(),
                prompt: "check".into(),
                model: "qwen-32b".into(),
                cron: "0 3 * * *".into(),
                timezone: "UTC".into(),
                tools_enabled: false,
                reuse_conversation: false,
                reuse_rounds: 1,
                next_run_at: None,
            },
        )
        .await
        .unwrap();
        let (ctx, hub, _rx) = ctx_watched(&pool);

        let (out, ()) = tokio::join!(
            DeleteScheduledAction.run(ctx, json!({"id": action.id})),
            async { answer(&hub, "Yes, delete it").await }
        );
        assert_eq!(out.unwrap()["deleted"], true);
        assert!(
            scheduled::list_for_user(&pool, "u1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A bad cron expression is rejected with the parse error, before any
    /// confirmation — so the model can fix it itself rather than asking the
    /// user about a schedule that can't be saved.
    #[tokio::test]
    async fn an_invalid_cron_expression_reports_the_parse_error() {
        let pool = seeded().await;
        for bad in ["0 8 * *", "99 8 * * 1", "not a cron"] {
            let err = ScheduleAction
                .run(
                    ctx_unattended(&pool),
                    json!({"name": "x", "prompt": "y", "cron": bad}),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("cron") || err.contains("minute") || err.contains("5 fields"),
                "for {bad:?}: {err}"
            );
            // Crucially not the confirmation error: we never got that far.
            assert!(!err.contains("nobody confirmed"), "for {bad:?}: {err}");
        }
    }

    /// An expression that parses but never occurs would be a schedule that
    /// silently never fires.
    #[tokio::test]
    async fn a_schedule_with_no_occurrence_is_rejected() {
        let pool = seeded().await;
        let err = ScheduleAction
            .run(
                ctx_unattended(&pool),
                json!({"name": "x", "prompt": "y", "cron": "0 0 30 2 *"}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("never occurs"), "{err}");
    }

    /// Off a path that knows the model there is nothing sane to schedule
    /// with, and the failure must name the reason rather than inventing a
    /// pool id that fails at 06:00.
    #[tokio::test]
    async fn without_a_known_model_it_refuses_early() {
        let pool = seeded().await;
        let ctx = ToolContext {
            user_id: "u1".into(),
            ..ToolContext::for_test(pool.clone())
        };
        let err = ScheduleAction
            .run(ctx, create_args())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("which model"), "{err}");
    }

    #[tokio::test]
    async fn an_unknown_timezone_is_rejected() {
        let pool = seeded().await;
        let err = ScheduleAction
            .run(
                ctx_unattended(&pool),
                json!({
                    "name": "x", "prompt": "y", "cron": "0 8 * * 1",
                    "timezone": "Mars/Olympus"
                }),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown timezone"), "{err}");
    }

    /// Listing is per-user and carries the human preview the model needs to
    /// describe a schedule without re-deriving cron semantics.
    #[tokio::test]
    async fn listing_is_scoped_to_the_caller_and_includes_a_preview() {
        let pool = seeded().await;
        sqlx::query(
            r#"INSERT INTO users (id, email, created_at, updated_at)
               VALUES ('u2', 'u2@example.com', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        for (user, name) in [("u1", "Mine"), ("u2", "Theirs")] {
            scheduled::create(
                &pool,
                NewAction {
                    user_id: user.into(),
                    name: name.into(),
                    prompt: "p".into(),
                    model: "qwen-32b".into(),
                    cron: "0 8 * * 1".into(),
                    timezone: "Europe/Berlin".into(),
                    tools_enabled: false,
                    reuse_conversation: false,
                    reuse_rounds: 1,
                    next_run_at: None,
                },
            )
            .await
            .unwrap();
        }

        let out = ListScheduledActions
            .run(ctx_unattended(&pool), Value::Null)
            .await
            .unwrap();
        assert_eq!(out["count"], 1, "{out:?}");
        let row = &out["actions"][0];
        assert_eq!(row["name"], "Mine");
        // `describe()`'s own wording ("At 08:00, on Mon."), not a re-derivation
        // here — the point is that the human summary reaches the model at all,
        // so it never has to explain cron semantics to the user itself.
        let schedule = row["schedule"].as_str().unwrap();
        assert!(
            schedule.contains("08:00") && schedule.contains("Mon"),
            "describe() must reach the model: {row:?}"
        );
        assert_eq!(
            row["next_runs"].as_array().unwrap().len(),
            PREVIEW_RUNS,
            "{row:?}"
        );
        assert_eq!(
            row["tools_enabled"], false,
            "actions must never advertise tools they don't have: {row:?}"
        );
    }

    /// Another user's action is reported as nonexistent, not as forbidden —
    /// the same no-existence-leak behaviour the RAG collections have.
    #[tokio::test]
    async fn deleting_another_users_action_looks_like_a_missing_id() {
        let pool = seeded().await;
        sqlx::query(
            r#"INSERT INTO users (id, email, created_at, updated_at)
               VALUES ('u2', 'u2@example.com', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let theirs = scheduled::create(
            &pool,
            NewAction {
                user_id: "u2".into(),
                name: "Theirs".into(),
                prompt: "p".into(),
                model: "m".into(),
                cron: "0 8 * * 1".into(),
                timezone: "UTC".into(),
                tools_enabled: false,
                reuse_conversation: false,
                reuse_rounds: 1,
                next_run_at: None,
            },
        )
        .await
        .unwrap();

        let err = DeleteScheduledAction
            .run(ctx_unattended(&pool), json!({"id": theirs.id}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no scheduled action"), "{err}");
        assert!(!err.contains("nobody confirmed"), "no confirmation: {err}");
        // Still there.
        assert_eq!(
            scheduled::list_for_user(&pool, "u2").await.unwrap().len(),
            1
        );
    }
}
