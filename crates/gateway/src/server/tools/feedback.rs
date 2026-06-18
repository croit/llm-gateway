// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Mid-turn "ask the browser, then wait" hub for the chat path.
//!
//! `get_user_location` uses it to request a precise position while a
//! chat turn is in flight: the tool [`register`](FeedbackHub::register)s
//! a oneshot keyed by the assistant turn id, pushes a prompt to the
//! browser over the turn's SSE stream, and awaits the reply that
//! `POST /api/v0/me/location/feedback/{turn_id}` delivers via
//! [`resolve`](FeedbackHub::resolve). Keyed by turn id because that's
//! the unit the browser already references (the bubble is `#turn-{id}`).
//!
//! Best-effort end to end: if the user never answers, the tool's wait
//! times out, [`cancel`](FeedbackHub::cancel)s its slot, and falls back
//! to coarse GeoIP. Only ever `Some` on the chat path — proxy / bearer
//! callers have no browser to ask.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::oneshot;

/// What the browser sent back for a pending location request.
#[derive(Clone, Debug, PartialEq)]
pub enum BrowserFix {
    /// A precise position the user agreed to share.
    Position {
        lat: f64,
        lon: f64,
        accuracy: Option<f64>,
    },
    /// The user declined (or dismissed) the prompt.
    Declined,
}

/// Turn-id → the channel a waiting tool is parked on. Plain `Mutex`:
/// every critical section is a single map op with no `.await` held.
#[derive(Default)]
pub struct FeedbackHub {
    pending: Mutex<HashMap<String, oneshot::Sender<BrowserFix>>>,
}

impl FeedbackHub {
    /// Register interest in a reply for `turn_id`, returning the receiver
    /// to await. A second registration for the same turn supersedes the
    /// first (its sender drops → the earlier awaiter sees `Canceled` and
    /// falls back), which is the right behaviour for a retry on the same
    /// turn id.
    pub fn register(&self, turn_id: &str) -> oneshot::Receiver<BrowserFix> {
        let (tx, rx) = oneshot::channel();
        self.lock().insert(turn_id.to_string(), tx);
        rx
    }

    /// Deliver a reply to whoever is awaiting `turn_id`. Returns whether
    /// someone was actually waiting (`false` = nothing pending, e.g. the
    /// tool already timed out and gave up — the caller can ignore that).
    pub fn resolve(&self, turn_id: &str, fix: BrowserFix) -> bool {
        match self.lock().remove(turn_id) {
            Some(tx) => tx.send(fix).is_ok(),
            None => false,
        }
    }

    /// Drop any pending registration for `turn_id` — the tool gave up
    /// waiting, so a late reply has nowhere to go.
    pub fn cancel(&self, turn_id: &str) {
        self.lock().remove(turn_id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<BrowserFix>>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_then_resolve_delivers() {
        let hub = FeedbackHub::default();
        let rx = hub.register("turn-1");
        assert!(hub.resolve(
            "turn-1",
            BrowserFix::Position {
                lat: 1.0,
                lon: 2.0,
                accuracy: Some(10.0)
            }
        ));
        assert_eq!(
            rx.await.unwrap(),
            BrowserFix::Position {
                lat: 1.0,
                lon: 2.0,
                accuracy: Some(10.0)
            }
        );
    }

    #[tokio::test]
    async fn resolve_unknown_turn_is_false() {
        let hub = FeedbackHub::default();
        assert!(!hub.resolve("nope", BrowserFix::Declined));
    }

    #[tokio::test]
    async fn cancel_drops_the_waiter() {
        let hub = FeedbackHub::default();
        let rx = hub.register("turn-2");
        hub.cancel("turn-2");
        // Sender dropped → awaiting the receiver errors rather than hangs.
        assert!(rx.await.is_err());
        // And a later resolve finds nothing pending.
        assert!(!hub.resolve("turn-2", BrowserFix::Declined));
    }
}
