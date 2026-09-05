// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-user registry of in-flight session worker tasks.
//!
//! A single user has at most one worker streaming at a time. The
//! worker writes deltas (content, reasoning, tool calls) to SQLite
//! as they arrive from the upstream — an OpenAI-compatible HTTP
//! stream for the gateway — and emits a `Tick` on its broadcast
//! channel after each DB write.
//! HTTP subscribers (the original `/chat/{id}/messages` POST plus any
//! `GET /chat/{id}/tail` reconnects) re-read the DB on each tick and
//! re-emit the relevant patches.
//!
//! The DB-is-source-of-truth design is the simple-on-purpose answer
//! to the subscribe-vs-write race: subscribers always render whatever's
//! in the row at recv-time, so a missed tick just means the next tick
//! catches up. There's no in-memory snapshot to keep in sync with the
//! DB.
//!
//! Workers run to completion even when every subscriber has dropped —
//! a backgrounded phone reconnects later and finds the finished turn
//! in the DB; a still-streaming worker shows the rest live via tail.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::broadcast;

/// Heartbeat for live subscribers. The worker emits one of these after
/// every DB write so subscribers know to re-read.
///
/// Not `Copy` because [`TurnUpdate::Inject`] carries an `Arc`. `Clone`
/// is enough — the broadcast channel clones per subscriber, and an `Arc`
/// clone is just a refcount bump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnUpdate {
    Tick,
    Finalized,
    SidebarChanged,
    Inject(Arc<rama::bytes::Bytes>),
    /// Transient info banner shown in the chat bubble (e.g. "vision fallback
    /// activated — image described by {model}"). Subscribers render it as a
    /// daisyUI alert via a datastar patch.
    InfoMessage(String),
}

/// One live session worker, indexed by user id in `SessionWorkers`.
/// Holds the cancel flag the worker polls between upstream chunks
/// plus the broadcast channel subscribers attach to.
#[derive(Clone)]
pub struct ActiveWorker {
    /// DB id of the assistant turn this worker is filling in. Used by
    /// HTTP handlers to confirm a /tail attach is for the right turn
    /// (e.g. user navigated to a different session mid-stream).
    pub turn_id: String,
    /// Session this worker belongs to. Same purpose as `turn_id`.
    pub session_id: String,
    /// Worker polls this flag between upstream chunks. `POST
    /// /chat/{id}/cancel` flips it; the stop button on the composer
    /// flips it; `register()` flips the *previous* worker's flag when
    /// a fresh submit lands.
    pub cancel: Arc<AtomicBool>,
    /// Broadcast back to all attached subscribers. Capacity is
    /// generous — bursts of deltas land in tight loops and we don't
    /// want lagged subscribers (a slow phone over LTE) to miss frames.
    pub broadcast: broadcast::Sender<TurnUpdate>,
}

/// Result of trying to register a fresh worker.
pub enum RegisterOutcome {
    /// No worker was running — caller may spawn one with `worker`.
    Registered { worker: ActiveWorker },
    /// A worker was already running. Caller should refuse the new
    /// submit (return 409 / toast). The existing worker is returned so
    /// the caller can decide whether to subscribe to it instead.
    Busy { existing: ActiveWorker },
}

/// Capacity of the per-worker broadcast channel. ~256 frames buffered
/// before slow subscribers start seeing `RecvError::Lagged`. Each
/// frame is `TurnUpdate` (16-byte enum) so the buffer is < 4 KB total.
const BROADCAST_CAPACITY: usize = 256;

/// User-id → ActiveWorker. Wrapped in a Mutex (not RwLock) because
/// every access is short and we want strict order with `register`'s
/// "fire previous + insert new" sequence.
///
/// Single-tenant callers can pass a constant per-process id for
/// `user_id` — the registry doesn't care what the string contains.
#[derive(Default)]
pub struct SessionWorkers {
    inner: Mutex<HashMap<String, ActiveWorker>>,
}

impl SessionWorkers {
    /// Try to register a new worker for `user_id`. If one's already
    /// active, return `Busy` with a clone of its handle and leave the
    /// existing entry untouched — callers refuse the submit, they
    /// don't quietly cancel.
    ///
    /// Differs from the old `CancelRegistry::register` which *always*
    /// cancelled the prior worker and inserted the new one. That
    /// behaviour caused the duplication-on-retry bug: a datastar
    /// `@post` retry after a network blip would race a brand-new
    /// worker against the still-finishing previous one. We want the
    /// strict "one worker per user" invariant now.
    pub fn register(&self, user_id: &str, turn_id: &str, session_id: &str) -> RegisterOutcome {
        let mut g = self.inner.lock().unwrap();
        if let Some(existing) = g.get(user_id) {
            return RegisterOutcome::Busy {
                existing: existing.clone(),
            };
        }
        let (broadcast, _) = broadcast::channel(BROADCAST_CAPACITY);
        let worker = ActiveWorker {
            turn_id: turn_id.to_string(),
            session_id: session_id.to_string(),
            cancel: Arc::new(AtomicBool::new(false)),
            broadcast,
        };
        g.insert(user_id.to_string(), worker.clone());
        RegisterOutcome::Registered { worker }
    }

    /// How many turns are running right now — what a shutdown waits on.
    ///
    /// A turn outlives the HTTP connection that started it (that is what makes
    /// resume-on-reconnect work), so draining connections says nothing about
    /// whether work is still in flight. This is the number that does.
    pub fn active_count(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Ask every running turn to stop at its next checkpoint.
    ///
    /// Used on shutdown once the drain deadline is in sight: a turn that
    /// notices the flag finalises its own row, which is strictly better than
    /// being killed mid-write and swept as `errored` on the next boot.
    pub fn cancel_all(&self) -> usize {
        let g = self.inner.lock().unwrap();
        for w in g.values() {
            w.cancel.store(true, Ordering::SeqCst);
        }
        g.len()
    }

    /// Hand out the current worker (if any) — used by the tail handler
    /// to attach to a still-running stream.
    pub fn get(&self, user_id: &str) -> Option<ActiveWorker> {
        self.inner.lock().unwrap().get(user_id).cloned()
    }

    /// Flip the cancel flag on the user's current worker (if any).
    /// Used by `POST /chat/{id}/cancel`. No-op when no worker is
    /// active.
    pub fn cancel(&self, user_id: &str) {
        if let Some(w) = self.inner.lock().unwrap().get(user_id) {
            w.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Remove the worker entry iff it's the same one we registered.
    /// Matching by `Arc::ptr_eq` on the cancel flag keeps a slow
    /// finalising worker from yanking a newer worker's entry —
    /// belt-and-braces, since `register` now refuses to insert when a
    /// worker exists.
    pub fn clear(&self, user_id: &str, worker: &ActiveWorker) {
        let mut g = self.inner.lock().unwrap();
        if let Some(current) = g.get(user_id)
            && Arc::ptr_eq(&current.cancel, &worker.cancel)
        {
            g.remove(user_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a shutdown waits on. A turn outlives its HTTP connection, so
    /// draining connections proves nothing about work in flight — this count
    /// is the only thing that does.
    #[test]
    fn active_count_tracks_registered_turns() {
        let r = SessionWorkers::default();
        assert_eq!(r.active_count(), 0);

        let RegisterOutcome::Registered { worker: w1 } = r.register("u1", "t1", "s1") else {
            panic!("expected a fresh registration");
        };
        r.register("u2", "t2", "s2");
        assert_eq!(r.active_count(), 2);

        r.clear("u1", &w1);
        assert_eq!(r.active_count(), 1, "a finished turn stops being counted");
    }

    /// At the drain deadline every straggler is asked to stop, so it finalises
    /// its own row instead of being killed mid-write and swept as an orphan on
    /// the next boot.
    #[test]
    fn cancel_all_flags_every_running_turn() {
        let r = SessionWorkers::default();
        let RegisterOutcome::Registered { worker: a } = r.register("u1", "t1", "s1") else {
            panic!("registered");
        };
        let RegisterOutcome::Registered { worker: b } = r.register("u2", "t2", "s2") else {
            panic!("registered");
        };
        assert!(!a.cancel.load(Ordering::SeqCst));
        assert!(!b.cancel.load(Ordering::SeqCst));

        assert_eq!(r.cancel_all(), 2, "reports how many it signalled");
        assert!(a.cancel.load(Ordering::SeqCst));
        assert!(b.cancel.load(Ordering::SeqCst));
    }

    /// Cancelling nothing is not an error — the common shutdown, on an idle
    /// gateway, must not log or behave as if work were lost.
    #[test]
    fn cancel_all_on_an_idle_registry_is_a_no_op() {
        let r = SessionWorkers::default();
        assert_eq!(r.cancel_all(), 0);
        assert_eq!(r.active_count(), 0);
    }

    #[test]
    fn register_returns_registered_when_empty() {
        let r = SessionWorkers::default();
        let outcome = r.register("u1", "turn-1", "sess-1");
        assert!(matches!(outcome, RegisterOutcome::Registered { .. }));
    }

    #[test]
    fn register_returns_busy_when_user_has_active_worker() {
        let r = SessionWorkers::default();
        let _first = r.register("u1", "turn-1", "sess-1");
        let outcome = r.register("u1", "turn-2", "sess-1");
        match outcome {
            RegisterOutcome::Busy { existing } => assert_eq!(existing.turn_id, "turn-1"),
            _ => panic!("expected Busy"),
        }
    }

    #[test]
    fn cancel_flips_the_flag() {
        let r = SessionWorkers::default();
        let RegisterOutcome::Registered { worker } = r.register("u1", "t", "s") else {
            unreachable!()
        };
        assert!(!worker.cancel.load(Ordering::SeqCst));
        r.cancel("u1");
        assert!(worker.cancel.load(Ordering::SeqCst));
    }

    #[test]
    fn cancel_on_unknown_user_is_a_noop() {
        let r = SessionWorkers::default();
        r.cancel("nobody"); // must not panic
    }

    #[test]
    fn clear_removes_only_matching_worker() {
        let r = SessionWorkers::default();
        let RegisterOutcome::Registered { worker: first } = r.register("u1", "t1", "s") else {
            unreachable!()
        };
        // Pretend a second register raced through (it wouldn't, given
        // `Busy`, but exercise the ptr_eq guard anyway).
        r.clear("u1", &first);
        assert!(r.get("u1").is_none());

        let RegisterOutcome::Registered { worker: second } = r.register("u1", "t2", "s") else {
            unreachable!()
        };
        r.clear("u1", &first); // wrong token: must not remove second
        assert!(r.get("u1").is_some());
        r.clear("u1", &second);
        assert!(r.get("u1").is_none());
    }

    #[test]
    fn broadcast_round_trips_tick_then_finalized() {
        let r = SessionWorkers::default();
        let RegisterOutcome::Registered { worker } = r.register("u1", "t", "s") else {
            unreachable!()
        };
        let mut rx = worker.broadcast.subscribe();
        worker.broadcast.send(TurnUpdate::Tick).unwrap();
        worker.broadcast.send(TurnUpdate::Finalized).unwrap();
        assert_eq!(rx.try_recv().unwrap(), TurnUpdate::Tick);
        assert_eq!(rx.try_recv().unwrap(), TurnUpdate::Finalized);
    }
}
