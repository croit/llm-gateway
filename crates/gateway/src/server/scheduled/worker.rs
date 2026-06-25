// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The background loop that fires due scheduled actions.
//!
//! Mirrors the RAG indexer's shape (`spawn` → forever loop → `drain_once`
//! per tick). Each tick selects every action whose `next_run_at` has
//! passed, *claims* it (advances `next_run_at` to the next occurrence
//! before running, so a slow run or a crash can't double-fire), then
//! spawns the run. A run opens a fresh chat session and drives it
//! headlessly through the same `OpenAiDriver` the interactive `/chat`
//! path uses (see [`crate::openai_driver::build_tool_context`]) — so the
//! result is an ordinary conversation the user can open afterwards.
//!
//! Catch-up policy: if the gateway was down across one or more fire
//! times, `next_after(now)` jumps straight to the next *future*
//! occurrence, so the missed slots collapse into a single catch-up run on
//! the first tick after startup rather than a backlog burst.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use session_core::db as chat;
use session_core::db::TurnStatus;
use uuid::Uuid;

use super::{ScheduledAction, cron::Cron};
use crate::rama_server::state::RamaState;

/// How often to poll for due actions. Cron granularity is one minute, so
/// a 30s tick guarantees we never miss a minute boundary by more than the
/// tick itself.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn the scheduler loop. Runs until the process exits. Never panics:
/// a failed pass is logged and retried on the next tick.
pub fn spawn(state: Arc<RamaState>) {
    tokio::spawn(async move {
        loop {
            if let Err(err) = drain_once(&state).await {
                tracing::warn!(error = %err, "scheduled-actions pass failed");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

/// One poll pass: claim every due action and spawn its run.
async fn drain_once(state: &Arc<RamaState>) -> Result<(), super::DbError> {
    let now = Timestamp::now();
    let due = super::due_actions(&state.db, now).await?;
    for action in due {
        // Compute the next occurrence and claim the slot *now*, before
        // running — concurrent ticks (or a restart) then see a future
        // `next_run_at` and won't re-select this action.
        let next = next_occurrence(&action, now);
        if let Err(err) = super::set_next_run(&state.db, &action.id, next).await {
            tracing::warn!(action = %action.id, error = %err, "claiming scheduled action");
            continue;
        }
        let state = state.clone();
        tokio::spawn(async move {
            run_action(state, action, next).await;
        });
    }
    Ok(())
}

/// Parse the action's cron + timezone and return the first occurrence
/// strictly after `now`. `None` if the expression is invalid (defensive —
/// the UI validates on save) or unsatisfiable, which pauses future fires.
fn next_occurrence(action: &ScheduledAction, now: Timestamp) -> Option<Timestamp> {
    let cron = Cron::parse(&action.cron).ok()?;
    let tz = TimeZone::get(&action.timezone).unwrap_or(TimeZone::UTC);
    cron.next_after(now, &tz)
}

/// Run one scheduled action end-to-end: open a chat session, persist the
/// prompt + an in-progress assistant turn, drive it to completion
/// headlessly, then record the outcome.
async fn run_action(state: Arc<RamaState>, action: ScheduledAction, next: Option<Timestamp>) {
    // The owner's RBAC roles gate the run's tools. A `None` here means the
    // user row vanished between selection and run (FK cascade should have
    // deleted the action too — defensive); record it and stop.
    let user = match crate::server::db::users::find_by_id(&state.db, &action.user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            let _ = super::mark_ran(
                &state.db,
                &action.id,
                "error",
                None,
                next,
                Some("schedule owner no longer exists"),
            )
            .await;
            return;
        }
        Err(err) => {
            tracing::warn!(action = %action.id, error = %err, "loading schedule owner");
            return;
        }
    };
    match try_run_action(&state, &action, &user).await {
        Ok((session_id, assistant_turn_id)) => {
            // Read the run's assistant turn to classify the outcome.
            let (status, error) = outcome_for(&state, &session_id, &assistant_turn_id).await;
            if let Err(err) = super::mark_ran(
                &state.db,
                &action.id,
                status,
                Some(&session_id),
                next,
                error.as_deref(),
            )
            .await
            {
                tracing::warn!(action = %action.id, error = %err, "recording scheduled run");
            }
        }
        Err(err) => {
            let msg = err.to_string();
            tracing::warn!(action = %action.id, error = %msg, "scheduled run failed to start");
            if let Err(e) =
                super::mark_ran(&state.db, &action.id, "error", None, next, Some(&msg)).await
            {
                tracing::warn!(action = %action.id, error = %e, "recording scheduled run");
            }
        }
    }
}

/// Open the chat session for one run and append the prompt + an in-progress
/// assistant turn. Returns `(session_id, assistant_turn_id)`.
///
/// In the default mode every call mints a brand-new session, so repeated
/// runs of the *same* action never collide — reusing the action id as the
/// turn id would PRIMARY KEY-clash on the second fire.
///
/// When `action.reuse_conversation` is set, the run instead appends into the
/// session the previous run opened (`last_session_id`), so prior runs become
/// conversation history. The first run (no prior session) and the
/// deleted-session case both fall back to minting a fresh session. The turn
/// ids are always freshly minted, so reuse never PRIMARY KEY-clashes either.
async fn open_run_session(
    db: &crate::server::db::Pool,
    action: &ScheduledAction,
) -> Result<(String, String), super::DbError> {
    let session_id = match reusable_session(db, action).await {
        Some(id) => id,
        None => {
            let session = chat::create_session(db, &action.user_id).await?;
            chat::set_session_title(db, &session.id, &action.name).await?;
            session.id
        }
    };

    let user_turn_id = Uuid::new_v4().to_string();
    chat::create_user_turn(db, &session_id, &user_turn_id, &action.prompt).await?;

    let assistant_turn_id = Uuid::new_v4().to_string();
    chat::create_assistant_turn_in_progress(db, &session_id, &assistant_turn_id, &action.model)
        .await?;
    Ok((session_id, assistant_turn_id))
}

/// The previous run's session id, but only when reuse is enabled *and* that
/// session still exists (the user may have deleted the chat). `None` means
/// the caller should mint a fresh session.
async fn reusable_session(
    db: &crate::server::db::Pool,
    action: &ScheduledAction,
) -> Option<String> {
    if !action.reuse_conversation {
        return None;
    }
    let last = action.last_session_id.as_deref()?;
    match chat::get_session(db, &action.user_id, last).await {
        Ok(Some(_)) => Some(last.to_string()),
        // Missing (deleted) or a read error: fall back to a fresh session
        // rather than failing the run on a stale pointer.
        _ => None,
    }
}

/// The fallible core of a run: everything up to and including driving the
/// turn to completion. Returns `(session_id, assistant_turn_id)` so the
/// outcome can be recorded against the run (and linked from the UI).
async fn try_run_action(
    state: &Arc<RamaState>,
    action: &ScheduledAction,
    user: &crate::server::db::users::User,
) -> Result<(String, String), super::DbError> {
    let (session_id, assistant_turn_id) = open_run_session(&state.db, action).await?;

    // Tools follow the user's normal RBAC grant when enabled; an empty
    // role set when disabled means the driver offers no tools at all.
    let roles = if action.tools_enabled {
        user.roles.clone()
    } else {
        Vec::new()
    };
    let tool_ctx = crate::openai_driver::build_tool_context(
        state,
        action.user_id.clone(),
        roles,
        session_id.clone(),
        assistant_turn_id.clone(),
        None, // headless: no client IP
        None, // headless: no interactive feedback channel
    );
    // In reuse mode, replay only the most recent `reuse_rounds` rounds
    // (one round = the run's prompt + reply = 2 turns) so a long-lived
    // action's context can't grow without bound. `None` = no cap, which is
    // the default-mode (fresh-session) behaviour and matches interactive
    // /chat.
    let history_limit = action
        .reuse_conversation
        .then(|| (action.reuse_rounds.max(0) as usize).saturating_mul(2));
    let driver = Box::new(crate::openai_driver::OpenAiDriver {
        state: state.clone(),
        tool_ctx,
        source: crate::server::db::usage::UsageSource::Scheduled,
        history_limit,
    });

    // No registry slot and a throwaway broadcast channel: a scheduled run
    // has no live viewer to tail or cancel it. The DB is the source of
    // truth, so dropping every frame is fine.
    let (broadcast, _rx) = tokio::sync::broadcast::channel(16);
    let ctx = session_core::driver::SessionContext {
        user_id: Some(action.user_id.clone()),
        session_id: session_id.clone(),
        assistant_turn_id: assistant_turn_id.clone(),
        model: action.model.clone(),
        cancel: Arc::new(AtomicBool::new(false)),
        broadcast,
    };
    session_core::worker::run_session_turn(state.db.clone(), driver, ctx).await;
    Ok((session_id, assistant_turn_id))
}

/// Classify a finished run by reading its assistant turn's final status.
/// Returns `("ok" | "error", Option<error message>)`.
async fn outcome_for(
    state: &RamaState,
    session_id: &str,
    turn_id: &str,
) -> (&'static str, Option<String>) {
    match chat::get_turn(&state.db, session_id, turn_id).await {
        Ok(Some(turn)) => match turn.status {
            TurnStatus::Completed => ("ok", None),
            _ => (
                "error",
                turn.error_message
                    .or(Some("run did not complete".to_string())),
            ),
        },
        Ok(None) => ("error", Some("no assistant turn produced".to_string())),
        Err(_) => ("error", Some("could not read run result".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::users::User;
    use crate::server::scheduled::{NewAction, create};

    async fn fresh_db() -> crate::server::db::Pool {
        crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap()
    }

    async fn seed_user(pool: &crate::server::db::Pool, id: &str) {
        let now = Timestamp::now();
        crate::server::db::users::upsert(
            pool,
            &User {
                id: id.to_string(),
                email: format!("{id}@example.com"),
                name: None,
                roles: vec![],
                created_at: now,
                updated_at: now,
                timezone: Some("Europe/Berlin".to_string()),
            },
        )
        .await
        .unwrap();
    }

    fn new_action(reuse_conversation: bool) -> NewAction {
        NewAction {
            user_id: "u1".to_string(),
            name: "Every minute".to_string(),
            prompt: "hi".to_string(),
            model: "qwen".to_string(),
            cron: "* * * * *".to_string(),
            timezone: "Europe/Berlin".to_string(),
            tools_enabled: false,
            reuse_conversation,
            reuse_rounds: 5,
            next_run_at: None,
        }
    }

    /// Default mode: each fire of the same action must open a fresh session +
    /// turns. The original bug reused the action id as the assistant turn id,
    /// so the second run PRIMARY KEY-clashed on `chat_turns.id` and never ran.
    #[tokio::test]
    async fn repeated_runs_open_distinct_sessions_and_turns() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        let action = create(&pool, new_action(false)).await.unwrap();

        let (s1, t1) = open_run_session(&pool, &action).await.unwrap();
        // Second fire of the SAME action — must not collide.
        let (s2, t2) = open_run_session(&pool, &action).await.unwrap();

        assert_ne!(s1, s2, "each run gets its own session");
        assert_ne!(t1, t2, "each run gets its own assistant turn");
        assert_ne!(
            t1, action.id,
            "assistant turn id must not reuse the action id"
        );
        assert_ne!(t2, action.id);
    }

    /// Reuse mode: the second fire must land in the *same* session the first
    /// run opened (so prior runs become history), but still mint a fresh
    /// assistant turn. The action's `last_session_id` is what the worker
    /// stamps via `mark_ran` between runs, so we simulate that here.
    #[tokio::test]
    async fn reuse_mode_second_run_reuses_session_distinct_turns() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        let mut action = create(&pool, new_action(true)).await.unwrap();

        let (s1, t1) = open_run_session(&pool, &action).await.unwrap();
        // The worker records the opened session on the action between fires.
        action.last_session_id = Some(s1.clone());
        let (s2, t2) = open_run_session(&pool, &action).await.unwrap();

        assert_eq!(s1, s2, "reuse mode keeps the same session across runs");
        assert_ne!(t1, t2, "each run still gets its own assistant turn");
    }

    /// Reuse mode falls back to a fresh session when the prior chat was
    /// deleted (a stale `last_session_id` must not error the run).
    #[tokio::test]
    async fn reuse_mode_falls_back_when_prior_session_gone() {
        let pool = fresh_db().await;
        seed_user(&pool, "u1").await;
        let mut action = create(&pool, new_action(true)).await.unwrap();
        action.last_session_id = Some("does-not-exist".to_string());

        let (s, _t) = open_run_session(&pool, &action).await.unwrap();
        assert_ne!(s, "does-not-exist", "stale pointer must not be reused");
        // The freshly minted session exists and is owned by the user.
        assert!(chat::get_session(&pool, "u1", &s).await.unwrap().is_some());
    }
}
