// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Shared chat handler scaffolding.
//!
//! Lifts the chat-page machinery out of the gateway into
//! `session-core` so a future second consumer can mount the same
//! chat surface without forking ~1000 lines of nearly-identical
//! code. What lives here:
//!
//! - The SSE-stream lifecycle around an in-flight turn —
//!   `spawn_session_stream_response` opens the per-request channel,
//!   subscribes to the worker's broadcast, and emits
//!   `mode outer` element-patches keyed to `#turn-<uuid>` as the
//!   assistant row mutates in the DB.
//! - `emit_current_state` — single DB read + single
//!   `render_assistant_turn` + one SSE event. Called both
//!   when ticks land and when subscribers join mid-stream so a
//!   fresh client sees state immediately.
//! - `cancel_turn` — pure flag flip on the worker entry; the
//!   handler-level cookie/auth check is per-binary on top.
//! - Tiny shared response constructors (`empty_sse_response`,
//!   `sse_error_response`).
//!
//! What stays per-binary:
//! - Auth gate.
//! - Sidebar-row repaints — each consumer passes a sidebar-emit
//!   callback into `spawn_session_stream_response`.
//! - Submit parsing.
//! - Driver construction.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use rama::http::{Body, Response, StatusCode, header};
use tokio::sync::broadcast;

use crate::chrome::{sse_patch, sse_signals};
use crate::db::{self, Pool};
use crate::i18n::Lang;
use crate::render::{TurnStream, render_thinking_body};
use crate::workers::{SessionWorkers, TurnUpdate};

/// Minimum spacing between two turn patches on one subscriber's
/// wire. Upstream chunks arrive far faster than a human perceives;
/// coalescing them caps the event rate (and the per-event envelope
/// overhead) without visibly changing liveness — the trailing flush
/// guarantees the final state always lands.
const PATCH_COALESCE: Duration = Duration::from_millis(120);

/// Sender end of the per-request SSE channel. Handlers fill it from
/// the background task spawned by `spawn_session_stream_response`.
pub type SseTx =
    rama::futures::channel::mpsc::UnboundedSender<Result<rama::bytes::Bytes, std::io::Error>>;

/// Type-erased per-binary sidebar emitter. The streaming loop calls
/// it whenever a `TurnUpdate::SidebarChanged` arrives so the binary
/// can repatch its sidebar. The returned future yields the sender
/// back so the loop can keep using it on subsequent ticks.
pub type SidebarEmitter =
    Box<dyn Fn(SseTx) -> Pin<Box<dyn Future<Output = Result<SseTx, ()>> + Send>> + Send + Sync>;

/// Empty 200/OK SSE response — used by the cancel handler so the
/// client gets a clean close after flipping the cancel flag.
pub fn empty_sse_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body("".into())
        .unwrap()
}

/// Error response for a chat-handler validation failure. Returns a
/// 200/text-event-stream body carrying a single error-toast
/// `datastar-patch-elements` event so the UI gets the same
/// red-bubble feedback as every other failed action. Used to live
/// as a 400 + plain text, but datastar 1.0 ignores non-SSE bodies
/// on `@post` responses — the user saw no toast and no console
/// message, just silence.
///
/// Deliberately does **not** touch `$chatStreaming`. Most handlers that
/// reach for this (tail, canvas, attachment removal, …) can fire while a
/// turn is streaming, and clearing the signal there disarmed the Stop
/// control mid-turn: the composer flipped back to "ready" while tool
/// calls kept landing, so the turn looked finished when it wasn't. Only
/// the handlers that own the submit→turn handoff may move the signal —
/// see [`sse_submit_error_response`] and [`sse_turn_busy_response`].
pub fn sse_error_response(message: &str) -> Response {
    use crate::chrome::{Flash, FlashKind, sse_response, sse_toast};
    sse_response(&[sse_toast(&Flash {
        kind: FlashKind::Error,
        message: message.to_string(),
    })])
}

/// Error response for a submit that never became a turn — the composer's
/// `POST …/messages` and the retry/edit regeneration posts.
///
/// Adds the `chatStreaming: false` reset [`sse_error_response`] leaves
/// alone: those three directives optimistically set `$chatStreaming =
/// true` before the `@post` fires, and only the streaming loop's
/// `Finalized` event clears it. Without the reset a rejected submit
/// leaves the composer showing Stop forever — and the next click posts to
/// `/cancel` instead of sending.
///
/// Use this **only** where no worker was left running. If a turn is in
/// fact live, use [`sse_turn_busy_response`].
pub fn sse_submit_error_response(message: &str) -> Response {
    use crate::chrome::{Flash, FlashKind, sse_response, sse_toast};
    sse_response(&[
        sse_signals(r#"{"chatStreaming":false}"#),
        sse_toast(&Flash {
            kind: FlashKind::Error,
            message: message.to_string(),
        }),
    ])
}

/// Rejection for a submit that arrived while this user's turn is still
/// streaming (`RegisterOutcome::Busy`).
///
/// Sets `chatStreaming: **true**`. The previous behaviour funnelled this
/// through the blanket reset, so asking "are you still working?" during a
/// long turn *disarmed* the Stop button — the one moment it is most
/// needed — and left the composer claiming the turn was done. A turn
/// really is in flight here, so the honest signal is the armed one; it
/// also re-arms a client whose signal had drifted false for any other
/// reason.
pub fn sse_turn_busy_response(message: &str) -> Response {
    use crate::chrome::{Flash, FlashKind, sse_response, sse_toast};
    sse_response(&[
        sse_signals(r#"{"chatStreaming":true}"#),
        sse_toast(&Flash {
            kind: FlashKind::Error,
            message: message.to_string(),
        }),
    ])
}

/// Flip the cancel flag on the active worker for this (user_id,
/// session_id) pair. Returns true if a worker was found + flagged,
/// false if nothing was running. Pure registry op — auth + redirect
/// shape lives in the per-binary handler.
pub fn cancel_turn(workers: &SessionWorkers, user_id: &str, session_id: &str) -> bool {
    let Some(worker) = workers.get(user_id) else {
        return false;
    };
    if worker.session_id != session_id {
        return false;
    }
    worker
        .cancel
        .store(true, std::sync::atomic::Ordering::SeqCst);
    true
}

/// Sender-side state of one SSE subscriber's turn stream.
struct TurnFeed {
    pool: Pool,
    session_id: String,
    assistant_turn_id: String,
    stream: TurnStream,
    dirty: bool,
    last_emit: Option<tokio::time::Instant>,
}

impl TurnFeed {
    fn new(
        pool: Pool,
        session_id: String,
        assistant_turn_id: String,
        actions: Option<String>,
        lang: Lang,
    ) -> Self {
        let stream = TurnStream::new(&assistant_turn_id, actions.as_deref(), lang);
        Self {
            pool,
            session_id,
            assistant_turn_id,
            stream,
            dirty: false,
            last_emit: None,
        }
    }

    /// Mark pending turn state; flush immediately when the coalesce
    /// window has elapsed, otherwise the deadline branch of the loop
    /// picks it up.
    fn tick(&mut self) {
        self.dirty = true;
    }

    fn due(&self) -> bool {
        self.dirty
            && self
                .last_emit
                .is_none_or(|last| last.elapsed() >= PATCH_COALESCE)
    }

    /// Read the turn, emit whatever changed, and record the emission.
    /// `Ok(false)` signals the turn vanished — the caller closes the
    /// stream.
    async fn flush(&mut self, tx: &mut SseTx) -> Result<bool, ()> {
        use rama::futures::sink::SinkExt;
        let turns =
            match db::get_turn_with_tools(&self.pool, &self.session_id, &self.assistant_turn_id)
                .await
            {
                Ok(t) => t,
                Err(err) => {
                    tracing::warn!(error = %err, "chat stream: reading turn failed");
                    return Ok(true);
                }
            };
        let Some(tw) = turns else {
            return Ok(false);
        };
        for patch in self.stream.diff(&tw) {
            let event = sse_patch(Some(&patch.selector), Some(patch.mode), &patch.html);
            if tx.send(Ok(event)).await.is_err() {
                return Err(());
            }
        }
        self.dirty = false;
        self.last_emit = Some(tokio::time::Instant::now());
        Ok(true)
    }
}

/// Open a per-request SSE response wired to the worker's broadcast.
/// The background task:
///   1. Optionally emits `initial_patch` (the messages-POST path
///      uses this to splice the empty bubble skeleton on first
///      response; tail subscribers pass None).
///   2. Paints the turn's current state once immediately so a
///      mid-stream subscriber catches up without waiting for the
///      next delta.
///   3. Loops on the broadcast: `Tick` → mark pending and flush at
///      most every [`PATCH_COALESCE`]; `SidebarChanged` → flush +
///      call the per-binary `on_sidebar_changed`; `Finalized` → one
///      authoritative full render + a `chatStreaming=false` signal
///      patch + close.
///   4. `Lagged` (slow subscriber dropped some Ticks) → catch up by
///      re-reading the DB; its state subsumes anything missed.
#[allow(clippy::too_many_arguments)]
pub fn spawn_session_stream_response(
    pool: Pool,
    session_id: String,
    assistant_turn_id: String,
    mut broadcast_rx: broadcast::Receiver<TurnUpdate>,
    initial_patch: Option<rama::bytes::Bytes>,
    on_sidebar_changed: SidebarEmitter,
    actions: Option<String>,
    lang: Lang,
) -> Response {
    let (mut tx, rx) =
        rama::futures::channel::mpsc::unbounded::<Result<rama::bytes::Bytes, std::io::Error>>();

    tokio::spawn(async move {
        use rama::futures::sink::SinkExt;

        if let Some(p) = initial_patch
            && tx.send(Ok(p)).await.is_err()
        {
            return;
        }

        let mut feed = TurnFeed::new(pool, session_id, assistant_turn_id, actions, lang);
        match feed.flush(&mut tx).await {
            Ok(true) => {}
            _ => return,
        }

        loop {
            let deadline = match feed.last_emit {
                Some(last) if feed.dirty => last + PATCH_COALESCE,
                _ => tokio::time::Instant::now() + PATCH_COALESCE,
            };
            tokio::select! {
                update = broadcast_rx.recv() => {
                    match update {
                        Ok(TurnUpdate::Tick) => {
                            if feed.due() {
                                if feed.flush(&mut tx).await.is_err() {
                                    return;
                                }
                            } else {
                                feed.tick();
                            }
                        }
                        Ok(TurnUpdate::SidebarChanged) => {
                            if matches!(feed.flush(&mut tx).await, Ok(false) | Err(())) {
                                return;
                            }
                            match (on_sidebar_changed)(tx).await {
                                Ok(t) => tx = t,
                                Err(_) => return,
                            }
                        }
                        // Forward pre-framed bytes straight through — transient UI
                        // (e.g. a tool's location prompt) that the DB-driven
                        // re-render must not own.
                        Ok(TurnUpdate::Inject(bytes)) => {
                            if tx.send(Ok(bytes.as_ref().clone())).await.is_err() {
                                return;
                            }
                        }
                        Ok(TurnUpdate::InfoMessage(msg)) => {
                            let html = format!(
                                r#"<div class="alert alert-info my-2 text-sm" role="alert">{msg}</div>"#
                            );
                            let sse = crate::chrome::sse_patch(None, None, &html);
                            if tx.send(Ok(sse)).await.is_err() {
                                return;
                            }
                        }
                        Ok(TurnUpdate::Finalized) => {
                            // The worker settles the row before broadcasting,
                            // so this flush renders the authoritative final
                            // bubble (thinking trace included, retry control,
                            // settled labels). Any pending in-progress diff is
                            // subsumed by it.
                            feed.dirty = false;
                            if matches!(feed.flush(&mut tx).await, Ok(false) | Err(())) {
                                return;
                            }
                            let _ = tx.send(Ok(sse_signals(r#"{"chatStreaming":false}"#))).await;
                            return;
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            feed.dirty = true;
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    if feed.due()
                        && matches!(feed.flush(&mut tx).await, Ok(false) | Err(()))
                    {
                        return;
                    }
                }
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(rx))
        .unwrap()
}

/// The on-demand thinking sub-stream opened by expanding a live
/// trace's `<details>` (`data-on:toggle` → `@get …/thinking`). Ships
/// only the `#turn-<id>-thinking-body` interior: an immediate
/// snapshot, then a coalesced patch per reasoning change until the
/// turn finalizes (or the turn is already settled, in which case one
/// final patch closes the stream). Collapsed traces cost nothing —
/// the main stream never carries the live body.
pub fn spawn_thinking_stream_response(
    pool: Pool,
    session_id: String,
    turn_id: String,
    mut broadcast_rx: broadcast::Receiver<TurnUpdate>,
    lang: Lang,
) -> Response {
    let (mut tx, rx) =
        rama::futures::channel::mpsc::unbounded::<Result<rama::bytes::Bytes, std::io::Error>>();

    tokio::spawn(async move {
        let selector = format!("#turn-{turn_id}-thinking-body");
        let mut last_html = String::new();
        let mut dirty = false;

        // Immediate snapshot — the opener wants the trace now, not at
        // the next upstream chunk. Also covers a race where the turn
        // finalizes before we subscribed (the broadcast then reads
        // Closed and the loop returns after this one patch).
        if !emit_thinking(
            &pool,
            &session_id,
            &turn_id,
            &selector,
            &mut last_html,
            &mut tx,
            lang,
        )
        .await
        {
            return;
        }
        let mut last_emit = Some(tokio::time::Instant::now());

        loop {
            let deadline = match last_emit {
                Some(last) if dirty => last + PATCH_COALESCE,
                _ => tokio::time::Instant::now() + PATCH_COALESCE,
            };
            tokio::select! {
                update = broadcast_rx.recv() => {
                    match update {
                        Ok(TurnUpdate::Tick) | Ok(TurnUpdate::SidebarChanged) => {
                            dirty = true;
                            if last_emit.is_none_or(|l| l.elapsed() >= PATCH_COALESCE) {
                                if !emit_thinking(&pool, &session_id, &turn_id, &selector, &mut last_html, &mut tx, lang).await {
                                    return;
                                }
                                last_emit = Some(tokio::time::Instant::now());
                                dirty = false;
                            }
                        }
                        Ok(TurnUpdate::Finalized) => {
                            let _ = emit_thinking(&pool, &session_id, &turn_id, &selector, &mut last_html, &mut tx, lang).await;
                            return;
                        }
                        // Transient UI the thinking body doesn't own.
                        Ok(TurnUpdate::Inject(_)) | Ok(TurnUpdate::InfoMessage(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => return,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            dirty = true;
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    if dirty {
                        if !emit_thinking(&pool, &session_id, &turn_id, &selector, &mut last_html, &mut tx, lang).await {
                            return;
                        }
                        last_emit = Some(tokio::time::Instant::now());
                        dirty = false;
                    }
                }
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(rx))
        .unwrap()
}

/// Read the turn and, when its rendered reasoning changed, patch the
/// thinking body onto the wire. Returns false when the stream must
/// close (turn vanished or subscriber gone).
async fn emit_thinking(
    pool: &Pool,
    session_id: &str,
    turn_id: &str,
    selector: &str,
    last_html: &mut String,
    tx: &mut SseTx,
    lang: Lang,
) -> bool {
    use rama::futures::sink::SinkExt;
    let turn = match db::get_turn(pool, session_id, turn_id).await {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(error = %err, "thinking stream: reading turn failed");
            return true;
        }
    };
    let Some(turn) = turn else {
        return false;
    };
    let html = render_thinking_body(turn.reasoning.as_deref().unwrap_or_default(), lang);
    if html == *last_html {
        return true;
    }
    let event = sse_patch(Some(selector), Some("inner"), &html);
    *last_html = html;
    tx.send(Ok(event)).await.is_ok()
}

/// Convenience constructor for the common "no sidebar repaint
/// needed" case — passes through ticks/finalized but ignores
/// `SidebarChanged`. Useful when the consumer isn't displaying a
/// session list sidebar.
pub fn no_op_sidebar_emitter() -> SidebarEmitter {
    Box::new(|tx| Box::pin(async move { Ok(tx) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rama::http::body::util::BodyExt;

    async fn body_of(resp: Response) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn a_plain_error_leaves_the_streaming_signal_alone() {
        // Most callers are handlers that can fire *while* a turn streams (tail,
        // canvas, attachment removal). Clearing the signal there flipped the
        // composer back to "ready" mid-turn, so a still-running turn looked
        // finished while its tool calls kept climbing.
        let body = body_of(sse_error_response("nope")).await;
        assert!(
            !body.contains("chatStreaming"),
            "a plain chat error must not touch the streaming signal:\n{body}"
        );
        assert!(
            body.contains("event: datastar-patch-elements"),
            "the user still needs the toast:\n{body}"
        );
    }

    #[tokio::test]
    async fn a_failed_submit_disarms_the_stop_control() {
        // The submit directives optimistically arm the signal before the @post,
        // so a submit that never became a turn has to hand it back.
        let body = body_of(sse_submit_error_response("empty")).await;
        assert!(
            body.contains(r#"{"chatStreaming":false}"#),
            "a rejected submit must release the Stop state:\n{body}"
        );
    }

    #[tokio::test]
    async fn a_busy_rejection_arms_the_stop_control() {
        let body = body_of(sse_turn_busy_response("already streaming")).await;
        assert!(
            body.contains(r#"{"chatStreaming":true}"#),
            "a turn is live, so Stop must stay armed:\n{body}"
        );
        assert!(
            !body.contains(r#"{"chatStreaming":false}"#),
            "must not also disarm it:\n{body}"
        );
    }
}
