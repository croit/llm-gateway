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
        Ok(()) if cancel.load(Ordering::SeqCst) => (TurnStatus::Cancelled, None),
        Ok(()) => (TurnStatus::Completed, None),
        Err(err) => (TurnStatus::Errored, Some(err.to_string())),
    };

    // Reasoning timer cleanup. If the model emitted `reasoning_*`
    // chunks but never landed visible content (or the cancel
    // pre-empted the first content delta), the row's
    // `reasoning_elapsed_ms` is still NULL — the renderer shows a
    // forever-spinning "Thinking…" pseudo-state. Stamp it as
    // "now minus created_at" so the bubble reads as "Thought for
    // Xs" once the row finalises.
    if let Ok(Some(turn)) = db::list_turns(&pool, &session_id)
        .await
        .map(|turns| turns.into_iter().find(|t| t.turn.id == assistant_turn_id))
        && turn.turn.reasoning.is_some()
        && turn.turn.reasoning_elapsed_ms.is_none()
    {
        let created = turn.turn.created_at;
        let elapsed_ms = (jiff::Timestamp::now() - created).total(jiff::Unit::Millisecond);
        if let Ok(ms) = elapsed_ms {
            let _ = db::set_reasoning_elapsed(&pool, &assistant_turn_id, ms as i64).await;
        }
    }

    let _ = db::finalize_turn(&pool, &assistant_turn_id, status, error_message.as_deref()).await;
    let _ = db::touch_session(&pool, &session_id).await;
    let _ = broadcast.send(TurnUpdate::Finalized);
}
