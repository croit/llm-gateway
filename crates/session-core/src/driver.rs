// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The `SessionDriver` trait — the seam between session-core's
//! generic worker lifecycle and the surface that actually produces
//! deltas (an HTTP POST to a model backend, or a JSON-RPC exchange
//! with an ACP harness over remoc-vsock).
//!
//! A driver owns one method: `run_turn`. session-core gives it a
//! `SessionContext` (who/where, plus the cancel flag and broadcast
//! channel) and the user's prompt; the driver streams output, calls
//! the persistence callbacks back in `SessionContext`, and returns
//! `Ok(TurnOutcome)` on natural finish/cancel or `Err(TurnError)` on
//! upstream failure. The cancel flag must be polled between chunks (the
//! contract mirrors what the existing chat worker already does).
//!
//! What's deliberately NOT in this trait:
//! - The user message itself. Persisted to the DB before the worker
//!   spawns, so drivers read history (including the just-landed user
//!   turn) via `db::list_turns`.
//! - The persistence calls (`append_content`, `append_reasoning`,
//!   `insert_running_tool_call`, …). Drivers reach the DB through
//!   their own pool reference — the gateway's `OpenAiDriver` holds
//!   `Arc<RamaState>` with `.db`; any future driver would hold its
//!   own state with the same.
//!
//! Why the call surface is this thin: anything drivers share
//! already sits in session-core's DB module + the worker harness.
//! The trait carries the bare minimum the harness needs to drive
//! one turn and roll up the result.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::broadcast;

use crate::workers::TurnUpdate;

/// Errors a driver can surface to the worker harness. Kept as a
/// concrete enum (not a trait object) so callers can pattern-match
/// without dynamic dispatch. The harness translates these into the
/// final `TurnStatus` + `error_message` written to the DB row.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    /// Upstream returned a non-success status or a malformed body.
    /// `message` is what we'd want to surface in the UI's error
    /// alert.
    #[error("upstream: {message}")]
    Upstream { message: String },
    /// Transport-level failure: connection drop, TLS error, framing
    /// error. Distinct from `Upstream` so future retry policy can
    /// pick on transports but not 4xx responses.
    #[error("transport: {message}")]
    Transport { message: String },
    /// A local persistence step failed (SQLite write/read on the turn
    /// row) — NOT an upstream/model problem. Split out from `Upstream`
    /// so the UI/log label is honest ("storage" vs "upstream") and the
    /// message carries the operation name + the real DB error chain.
    #[error("storage: {message}")]
    Persistence { message: String },
    /// Caller passed a malformed `SessionContext` — almost always a
    /// bug in the harness. Not a user-facing condition.
    #[error("invalid request: {message}")]
    Invalid { message: String },
    /// The gateway stopped the turn itself — e.g. the model collapsed
    /// into a repetition loop. `message` is shown verbatim in the UI's
    /// error alert (no prefix, unlike the upstream/transport variants).
    #[error("{message}")]
    Aborted { message: String },
}

/// Everything a driver needs to drive one assistant turn.
///
/// `user_id` is optional so a single-tenant consumer can pass `None`
/// and skip the OIDC subject the gateway threads through. Renderer
/// and persistence code reads through this without branching.
#[derive(Clone)]
pub struct SessionContext {
    /// Caller's identity. `None` for single-user callers.
    pub user_id: Option<String>,
    /// The session this turn belongs to.
    pub session_id: String,
    /// The assistant turn row's id. Pre-allocated by the handler
    /// before the worker spawns so the driver's persistence calls
    /// can target the right row.
    pub assistant_turn_id: String,
    /// Driver-specific selector for what to call (an OpenAI model id
    /// like `gpt-4o-mini` for the gateway).
    pub model: String,
    /// Polled between chunks. Flipping it to `true` is the harness's
    /// way of asking the driver to abort cleanly.
    pub cancel: Arc<AtomicBool>,
    /// Driver fires a `Tick` on this after each persisted delta; the
    /// harness owns the broadcast channel and fans it out to attached
    /// HTTP subscribers.
    pub broadcast: broadcast::Sender<TurnUpdate>,
}

/// Drive one user→assistant turn.
///
/// The contract:
/// - Read prior history from `db::list_turns` (the user message is
///   already there).
/// - Honour `ctx.cancel` between chunks.
/// - On every persisted delta (content, reasoning, tool call insert,
///   tool call complete), emit one `TurnUpdate::Tick` on
///   `ctx.broadcast`.
/// - Returning `Ok(TurnOutcome)` is "natural finish OR cancel"; the
///   harness re-reads the cancel flag to decide between `Completed`
///   and `Cancelled`.
/// - Returning `Err(TurnError)` flips the turn to `errored` and
///   stamps the message on the row.
#[async_trait::async_trait]
pub trait SessionDriver: Send + Sync + 'static {
    async fn run_turn(&self, ctx: SessionContext) -> Result<TurnOutcome, TurnError>;
}

/// How a turn that did NOT fail ended.
///
/// Exists for the one case in between "it worked" and "it failed": the turn
/// produced a real, usable answer *and* something about how it ended has to be
/// said out loud — today, that the model was cut off at its output-token
/// ceiling mid-sentence.
///
/// Such a turn must not be `Errored`. That status is load-bearing well beyond
/// the error alert: `message_for_history` refuses to replay a non-completed
/// assistant turn, so erroring one deletes the partial answer from the model's
/// own context — and an answer the user can still read, that the model can no
/// longer see, is worse than either a clean failure or a plain success (asking
/// it to "continue" makes it start over). Webhook runs turn into 502s with no
/// output, scheduled runs record as failed, and compaction skips the turn.
///
/// So the turn completes, and the notice rides alongside it.
#[derive(Debug, Default, Clone)]
pub struct TurnOutcome {
    /// Shown under the reply and stored on the row. `None` for an ordinary
    /// finish, which is what [`TurnOutcome::default`] gives you.
    pub notice: Option<String>,
}
