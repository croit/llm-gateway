// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Generic worker harness.
//!
//! Glue between session-core's `SessionDriver` trait and the on-disk
//! turn row that one assistant message corresponds to. The handler
//! that accepts a user message in HTTP is responsible for:
//!
//!   1. Persisting the user turn.
//!   2. Persisting the assistant turn (status `in_progress`).
//!   3. Reserving the per-user worker slot via `SessionWorkers`.
//!   4. Spawning `run_session_turn` on a tokio task with a driver +
//!      a `SessionContext` carrying the assistant turn id, the
//!      cancel flag, and the broadcast channel.
//!
//! `run_session_turn` then:
//!
//!   - Calls `driver.run_turn(ctx)`.
//!   - Translates the result + cancel flag into a final `TurnStatus`.
//!   - Stamps a reasoning-elapsed if the model reasoned but never
//!     emitted content (so the renderer shows a stable "Thought for
//!     Xs" instead of a frozen "Thinking…").
//!   - Calls `finalize_turn`.
//!   - Bumps `chat_sessions.updated_at` so the sidebar floats it to
//!     the top on the next render.
//!   - Broadcasts `TurnUpdate::Finalized` so attached HTTP
//!     subscribers send their final patch and close.

use std::sync::atomic::Ordering;

use crate::db::{self, Pool, TurnStatus};
use crate::driver::{SessionContext, SessionDriver};
use crate::workers::TurnUpdate;

/// Drive the lifecycle around one `SessionDriver::run_turn` call.
/// The caller wraps this in `tokio::spawn` so the HTTP handler that
/// accepted the user message doesn't have to wait. The `Pool` is
/// owned (clones are cheap — sqlx pools are `Arc` internally) so the
/// future can outlive the request scope.
pub async fn run_session_turn(pool: Pool, driver: Box<dyn SessionDriver>, ctx: SessionContext) {
    let result = driver.run_turn(ctx.clone()).await;

    let SessionContext {
        session_id,
        assistant_turn_id,
        cancel,
        broadcast,
        ..
    } = ctx;

    // Cancel-vs-natural-finish disambiguation. The driver's `Ok(())`
    // covers both natural finishes and clean cancels (the contract
    // is that drivers don't surface cancel as an error); the cancel
    // flag tells us which it was.
    let (status, error_message) = match result {
        Ok(_) if cancel.load(Ordering::SeqCst) => (TurnStatus::Cancelled, None),
        // A finished turn may still carry a notice — see `TurnOutcome`. It goes
        // in the same column an error would, and the renderer tells them apart
        // by the row's status, so a turn that produced a real answer stays
        // `Completed` (replayable, webhook-ok, compactable) while still saying
        // out loud how it ended.
        Ok(outcome) => (TurnStatus::Completed, outcome.notice),
        Err(err) => {
            // The top-level `Display` often hides the real cause (e.g.
            // `DbError::Query`'s source sqlx error). Walk the full
            // `source()` chain into the log so a terse UI message like
            // "upstream: query" is always traceable server-side.
            let mut chain = err.to_string();
            let mut src = std::error::Error::source(&err);
            while let Some(e) = src {
                // Skip frames already embedded via thiserror's `{0}` so the
                // same cause isn't printed several times over.
                let s = e.to_string();
                if !chain.contains(&s) {
                    chain.push_str(": ");
                    chain.push_str(&s);
                }
                src = e.source();
            }
            tracing::error!(
                %session_id,
                %assistant_turn_id,
                error = %chain,
                "turn failed"
            );
            (TurnStatus::Errored, Some(err.to_string()))
        }
    };

    // Reasoning timer cleanup. If the model emitted `reasoning_*`
    // chunks but never landed visible content (or the cancel
    // pre-empted the first content delta), the row's
    // `reasoning_elapsed_ms` is still NULL — the renderer would show a
    // forever-spinning "Thinking…" pseudo-state. Freeze it now so the
    // bubble reads "Thought for Xs" once the row finalises. Measured
    // from `reasoning_started_at` (the actual first reasoning chunk)
    // when present, falling back to `created_at` for legacy rows.
    if let Ok(Some(turn)) = db::list_turns(&pool, &session_id)
        .await
        .map(|turns| turns.into_iter().find(|t| t.turn.id == assistant_turn_id))
        && turn.turn.reasoning.is_some()
        && turn.turn.reasoning_elapsed_ms.is_none()
    {
        let anchor = turn
            .turn
            .reasoning_started_at
            .unwrap_or(turn.turn.created_at);
        let elapsed_ms = (jiff::Timestamp::now() - anchor).total(jiff::Unit::Millisecond);
        if let Ok(ms) = elapsed_ms {
            let _ = db::set_reasoning_elapsed(&pool, &assistant_turn_id, ms.max(0.0) as i64).await;
        }
    }

    let _ = db::finalize_turn(&pool, &assistant_turn_id, status, error_message.as_deref()).await;
    let _ = db::touch_session(&pool, &session_id).await;
    let _ = broadcast.send(TurnUpdate::Finalized);
}
