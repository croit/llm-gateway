// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Mid-turn "ask the browser, then wait" hub for the chat path.
//!
//! A tool [`register`](FeedbackHub::register)s a oneshot keyed by the
//! assistant turn id, pushes a prompt to the browser over the turn's SSE
//! stream, and awaits the reply an API endpoint delivers via
//! [`resolve`](FeedbackHub::resolve). Keyed by turn id because that's the unit
//! the browser already references (the bubble is `#turn-{id}`).
//!
//! Two hubs exist, one per reply shape:
//!
//! - `FeedbackHub<BrowserFix>` — `get_user_location` asking for a precise
//!   position, answered by `POST /api/v0/me/location/feedback/{turn_id}`.
//! - `FeedbackHub<AskReply>` — `ask_user` asking a question, answered by
//!   `POST /api/v0/me/ask/feedback/{turn_id}`.
//!
//! The hub is generic over its payload rather than carrying one enum with a
//! variant per use case: the parking/resolving logic never inspects the value,
//! and a shared enum would let the location endpoint resolve an `ask_user`
//! wait with a position (and vice versa) — a type error worth keeping
//! impossible. The alternative considered was extending [`BrowserFix`] with an
//! `Answer` variant; that keeps one map but makes every consumer match on
//! variants it can never receive.
//!
//! Best-effort end to end: if the user never answers, the tool's wait times
//! out, [`cancel`](FeedbackHub::cancel)s its slot, and falls back to whatever
//! it can do without an answer. Only ever reachable on the chat path — proxy /
//! bearer callers have no browser to ask.

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

/// What the browser sent back for a pending [`ask_user`] question.
///
/// [`ask_user`]: https://docs.rs/gateway-tools
#[derive(Clone, Debug, PartialEq)]
pub enum AskReply {
    /// The user answered. `choices` holds the labels of any options they
    /// picked; `text` holds free-text they typed. Both can be present (an
    /// option plus a clarification), and at least one is non-empty.
    Answered {
        choices: Vec<String>,
        text: Option<String>,
    },
    /// The user dismissed the question without answering.
    Dismissed,
}

/// Turn-id → the channel a waiting tool is parked on. Plain `Mutex`:
/// every critical section is a single map op with no `.await` held.
pub struct FeedbackHub<T> {
    pending: Mutex<HashMap<String, oneshot::Sender<T>>>,
}

// Hand-written rather than derived: `#[derive(Default)]` would require
// `T: Default`, which no reply type has (nor should).
impl<T> Default for FeedbackHub<T> {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }
}

impl<T> FeedbackHub<T> {
    /// Register interest in a reply for `turn_id`, returning the receiver
    /// to await. A second registration for the same turn supersedes the
    /// first (its sender drops → the earlier awaiter sees `Canceled` and
    /// falls back), which is the right behaviour for a retry on the same
    /// turn id.
    pub fn register(&self, turn_id: &str) -> oneshot::Receiver<T> {
        let (tx, rx) = oneshot::channel();
        self.lock().insert(turn_id.to_string(), tx);
        rx
    }

    /// Deliver a reply to whoever is awaiting `turn_id`. Returns whether
    /// someone was actually waiting (`false` = nothing pending, e.g. the
    /// tool already timed out and gave up — the caller can ignore that).
    pub fn resolve(&self, turn_id: &str, reply: T) -> bool {
        match self.lock().remove(turn_id) {
            Some(tx) => tx.send(reply).is_ok(),
            None => false,
        }
    }

    /// Drop any pending registration for `turn_id` — the tool gave up
    /// waiting, so a late reply has nowhere to go.
    pub fn cancel(&self, turn_id: &str) {
        self.lock().remove(turn_id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<T>>> {
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
        let hub: FeedbackHub<BrowserFix> = FeedbackHub::default();
        assert!(!hub.resolve("nope", BrowserFix::Declined));
    }

    #[tokio::test]
    async fn cancel_drops_the_waiter() {
        let hub: FeedbackHub<BrowserFix> = FeedbackHub::default();
        let rx = hub.register("turn-2");
        hub.cancel("turn-2");
        // Sender dropped → awaiting the receiver errors rather than hangs.
        assert!(rx.await.is_err());
        // And a later resolve finds nothing pending.
        assert!(!hub.resolve("turn-2", BrowserFix::Declined));
    }

    #[tokio::test]
    async fn ask_replies_round_trip_through_their_own_hub() {
        let hub: FeedbackHub<AskReply> = FeedbackHub::default();
        let rx = hub.register("turn-3");
        assert!(hub.resolve(
            "turn-3",
            AskReply::Answered {
                choices: vec!["Postgres".into()],
                text: None,
            }
        ));
        assert_eq!(
            rx.await.unwrap(),
            AskReply::Answered {
                choices: vec!["Postgres".into()],
                text: None,
            }
        );
    }

    #[tokio::test]
    async fn the_two_hubs_are_independent() {
        // Same turn id parked on both hubs: resolving one must not disturb
        // the other. With a single shared map + enum this would be one slot,
        // and whichever endpoint answered first would un-park the wrong tool.
        let ask: FeedbackHub<AskReply> = FeedbackHub::default();
        let loc: FeedbackHub<BrowserFix> = FeedbackHub::default();
        let ask_rx = ask.register("turn-4");
        let loc_rx = loc.register("turn-4");

        assert!(loc.resolve("turn-4", BrowserFix::Declined));
        assert_eq!(loc_rx.await.unwrap(), BrowserFix::Declined);

        // The ask side is still parked and still answerable.
        assert!(ask.resolve("turn-4", AskReply::Dismissed));
        assert_eq!(ask_rx.await.unwrap(), AskReply::Dismissed);
    }
}
