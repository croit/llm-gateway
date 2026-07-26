// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The shared "run a prompt headlessly" helper.
//!
//! Both cron-fired scheduled actions ([`crate::server::scheduled::worker`])
//! and inbound-webhook fires (`rama_server::pages::webhooks`) need the same
//! thing: open a chat session, append the prompt + an in-progress assistant
//! turn, then drive it to completion through the same [`OpenAiDriver`] the
//! interactive `/chat` path uses — so the result lands as an ordinary
//! conversation the owner can open afterwards.
//!
//! The work is split in two so callers can act between the steps:
//!   - [`open_session`] mints (or reuses) the session and appends the prompt +
//!     an in-progress assistant turn, returning `(session_id,
//!     assistant_turn_id)`. A caller that must respond *before* the model
//!     finishes (an async webhook returning `202` with the session id) needs
//!     these ids up front.
//!   - [`drive`] builds the driver and runs the turn to completion.
//!
//! After [`drive`] returns, read the finished assistant turn
//! (`session_core::db::get_turn`) to classify the outcome or return its text.
//!
//! [`OpenAiDriver`]: crate::openai_driver::OpenAiDriver

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use session_core::db as chat;
use uuid::Uuid;

use crate::rama_server::state::RamaState;
use gateway_core::server::db::usage::UsageSource;
use gateway_core::server::db::{DbError, Pool};

/// Inputs to [`open_session`].
pub struct OpenParams<'a> {
    pub user_id: &'a str,
    /// Title for a freshly-minted session (ignored when `existing_session` is
    /// `Some`).
    pub title: &'a str,
    pub prompt: &'a str,
    pub model: &'a str,
    /// When `Some`, append into this existing session instead of minting a
    /// fresh one — so prior runs become conversation history. `None` mints a
    /// fresh session titled `title`.
    pub existing_session: Option<String>,
}

/// Open the run's session and append the prompt + an in-progress assistant
/// turn. Returns `(session_id, assistant_turn_id)`.
///
/// Every call mints fresh turn ids, so repeated runs from the same source
/// never PRIMARY KEY-clash — whether they share a session (reuse) or not.
pub async fn open_session(db: &Pool, p: OpenParams<'_>) -> Result<(String, String), DbError> {
    let session_id = match p.existing_session {
        Some(id) => id,
        None => {
            let session = chat::create_session(db, p.user_id).await?;
            chat::set_session_title(db, &session.id, p.title).await?;
            session.id
        }
    };

    let user_turn_id = Uuid::new_v4().to_string();
    chat::create_user_turn(db, &session_id, &user_turn_id, p.prompt).await?;

    let assistant_turn_id = Uuid::new_v4().to_string();
    chat::create_assistant_turn_in_progress(db, &session_id, &assistant_turn_id, p.model).await?;
    Ok((session_id, assistant_turn_id))
}

/// Inputs to [`drive`]. Tools are gated by `roles`: pass the owner's roles to
/// offer their normal tools, or an empty vec to offer none.
pub struct DriveParams {
    pub user_id: String,
    pub roles: Vec<String>,
    pub session_id: String,
    pub assistant_turn_id: String,
    pub model: String,
    pub source: UsageSource,
    /// Cap on how many prior turns the driver replays (`None` = no cap, the
    /// fresh-chat default). Callers that reuse a session set this to bound the
    /// replayed history.
    pub history_limit: Option<usize>,
}

/// Drive an already-opened turn to completion through the `OpenAiDriver`.
pub async fn drive(state: &Arc<RamaState>, p: DriveParams) {
    let tool_ctx = crate::openai_driver::build_tool_context(
        state,
        crate::openai_driver::TurnFacts {
            user_id: p.user_id.clone(),
            roles: p.roles,
            session_id: p.session_id.clone(),
            assistant_turn_id: p.assistant_turn_id.clone(),
            // Headless: no request, so no client IP, and nobody watching the
            // stream to answer an interactive prompt.
            client_ip: None,
            chat_feedback: None,
            model: Some(p.model.clone()),
        },
    );
    let driver = Box::new(crate::openai_driver::OpenAiDriver {
        state: state.clone(),
        tool_ctx,
        source: p.source,
        history_limit: p.history_limit,
        voice_mode: false,
    });

    // No registry slot and a throwaway broadcast channel: a headless run has no
    // live viewer to tail or cancel it. The DB is the source of truth, so
    // dropping every frame is fine.
    let (broadcast, _rx) = tokio::sync::broadcast::channel(16);
    let ctx = session_core::driver::SessionContext {
        user_id: Some(p.user_id),
        session_id: p.session_id,
        assistant_turn_id: p.assistant_turn_id,
        model: p.model,
        cancel: Arc::new(AtomicBool::new(false)),
        broadcast,
    };
    session_core::worker::run_session_turn(state.db.clone(), driver, ctx).await;
}
