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
use crate::server::usage::UsageHandle;

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
    /// Fire-and-forget usage-metrics sink. The proxy, chat driver, and
    /// scheduler hand it a `UsageRecord` per upstream call; a background
    /// task batches the writes. `disabled()` when `[usage] enabled = false`,
    /// where `emit` is a no-op. See `server::usage`.
    pub usage: UsageHandle,
}

impl RamaState {
    pub fn new(inner: AppState, sessions: SessionStore, usage: UsageHandle) -> Self {
        Self {
            inner,
            sessions,
            chats: Arc::new(SessionWorkers::default()),
            location_feedback: Arc::new(crate::server::tools::feedback::FeedbackHub::default()),
            usage,
        }
    }

    /// Swap in a different usage sink. Mainly for tests, which build state
    /// with a disabled handle and opt into a live metered one.
    pub fn with_usage(mut self, usage: UsageHandle) -> Self {
        self.usage = usage;
        self
    }
}

impl Deref for RamaState {
    type Target = AppState;
    fn deref(&self) -> &AppState {
        &self.inner
    }
}
