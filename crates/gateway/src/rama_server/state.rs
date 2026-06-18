// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Shared state for the rama-based server.
//!
//! Wraps the existing `AppState` (which has the DB, upstream registry,
//! RBAC resolver, OIDC client, etc.) and adds rama-specific extras: a
//! hand-rolled `SessionStore` plus a `SessionWorkers` registry that
//! tracks each user's in-flight chat worker for the live-stream tail
//! and cancel paths. `Deref`s to `AppState` so call sites like
//! `state.upstreams` keep working without churn.

use std::ops::Deref;
use std::sync::Arc;

use session_core::SessionWorkers;

use crate::rama_server::session::SessionStore;
use crate::server::AppState;

#[derive(Clone)]
pub struct RamaState {
    inner: AppState,
    pub sessions: SessionStore,
    pub chats: Arc<SessionWorkers>,
    /// Mid-turn browser-prompt rendezvous, keyed by assistant turn id.
    /// `get_user_location` parks on it while waiting for the user to
    /// share a precise position; `POST /api/v0/me/location/feedback/{id}`
    /// resolves it. See `server::tools::feedback`.
    pub location_feedback: Arc<crate::server::tools::feedback::FeedbackHub>,
}

impl RamaState {
    pub fn new(inner: AppState, sessions: SessionStore) -> Self {
        Self {
            inner,
            sessions,
            chats: Arc::new(SessionWorkers::default()),
            location_feedback: Arc::new(crate::server::tools::feedback::FeedbackHub::default()),
        }
    }
}

impl Deref for RamaState {
    type Target = AppState;
    fn deref(&self) -> &AppState {
        &self.inner
    }
}
