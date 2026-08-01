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

use rama::http::{Body, Response, StatusCode, header};
use tokio::sync::broadcast;

use crate::chrome::{sse_patch, sse_signals};
use crate::db::{self, Pool};
use crate::i18n::Lang;
use crate::render;
use crate::workers::{SessionWorkers, TurnUpdate};

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

/// Pull the assistant turn out of the DB, render it, and forward as
/// a `mode outer` patch keyed to `#turn-<uuid>`. Used both for the
/// in-flight tail and for the "fresh subscriber catches up" path
/// inside `spawn_session_stream_response`.
pub async fn emit_current_state(
    pool: &Pool,
    session_id: &str,
    assistant_turn_id: &str,
    tx: &mut SseTx,
    actions: Option<&str>,
    lang: Lang,
) -> Result<(), ()> {
    use rama::futures::sink::SinkExt;

    let turns = match db::list_turns(pool, session_id).await {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(error = %err, "chat stream: list_turns failed");
            return Ok(());
        }
    };
    let Some(turn_with_tools) = turns.into_iter().find(|t| t.turn.id == assistant_turn_id) else {
        // Turn vanished (session deleted from another tab). Clean
        // close from the streamer's POV.
        return Err(());
    };
    let selector = format!("#turn-{assistant_turn_id}");
    let html = render::render_assistant_turn(&turn_with_tools, actions, lang).to_string();
    let patch = sse_patch(Some(&selector), Some("outer"), &html);
    tx.send(Ok(patch)).await.map_err(|_| ())
}

/// Open a per-request SSE response wired to the worker's broadcast.
/// The background task:
///   1. Optionally emits `initial_patch` (the messages-POST path
///      uses this to splice the empty bubble skeleton on first
///      response; tail subscribers pass None).
///   2. Emits the current DB state once immediately so a mid-stream
///      subscriber catches up without waiting for the next delta.
///   3. Loops on the broadcast: `Tick` → re-emit; `SidebarChanged`
///      → call the per-binary `on_sidebar_changed`; `Finalized` →
///      one last re-emit + a `chatStreaming=false` signal patch +
///      close.
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
        let actions = actions.as_deref();

        if let Some(p) = initial_patch
            && tx.send(Ok(p)).await.is_err()
        {
            return;
        }
        // Render the empty skeleton (or the just-finalized turn for
        // a fresh tail subscriber) before waiting on the broadcast.
        let _ = emit_current_state(
            &pool,
            &session_id,
            &assistant_turn_id,
            &mut tx,
            actions,
            lang,
        )
        .await;

        loop {
            match broadcast_rx.recv().await {
                Ok(TurnUpdate::Tick) => {
                    if emit_current_state(
                        &pool,
                        &session_id,
                        &assistant_turn_id,
                        &mut tx,
                        actions,
                        lang,
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                Ok(TurnUpdate::SidebarChanged) => {
                    if emit_current_state(
                        &pool,
                        &session_id,
                        &assistant_turn_id,
                        &mut tx,
                        actions,
                        lang,
                    )
                    .await
                    .is_err()
                    {
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
                    let _ = emit_current_state(
                        &pool,
                        &session_id,
                        &assistant_turn_id,
                        &mut tx,
                        actions,
                        lang,
                    )
                    .await;
                    let _ = tx.send(Ok(sse_signals(r#"{"chatStreaming":false}"#))).await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let _ = emit_current_state(
                        &pool,
                        &session_id,
                        &assistant_turn_id,
                        &mut tx,
                        actions,
                        lang,
                    )
                    .await;
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
