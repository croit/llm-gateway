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
use std::sync::atomic::{AtomicU32, Ordering};

use session_core::SessionWorkers;

use crate::server::AppState;
use gateway_core::rama_server::session::SessionStore;
use gateway_core::server::usage::UsageHandle;

#[derive(Clone)]
pub struct RamaState {
    inner: AppState,
    pub sessions: SessionStore,
    pub chats: Arc<SessionWorkers>,
    /// Mid-turn browser-prompt rendezvous, keyed by assistant turn id.
    /// `get_user_location` parks on it while waiting for the user to
    /// share a precise position; `POST /api/v0/me/location/feedback/{id}`
    /// resolves it. See `server::tools::feedback`.
    pub location_feedback: Arc<
        crate::server::tools::feedback::FeedbackHub<crate::server::tools::feedback::BrowserFix>,
    >,
    /// The same rendezvous for `ask_user`: the tool parks on it after
    /// injecting a question, and `POST /api/v0/me/ask/feedback/{id}` resolves
    /// it. A separate hub (not a shared one keyed only by turn id) so the two
    /// endpoints can't un-park each other's tool — a turn can legitimately
    /// have both pending.
    pub ask_feedback:
        Arc<crate::server::tools::feedback::FeedbackHub<crate::server::tools::feedback::AskReply>>,
    /// Fire-and-forget usage-metrics sink. The proxy, chat driver, and
    /// scheduler hand it a `UsageRecord` per upstream call; a background
    /// task batches the writes. `disabled()` when `[usage] enabled = false`,
    /// where `emit` is a no-op. See `server::usage`.
    pub usage: UsageHandle,
    /// Automatic document OCR: the derivative cache, the limits, the shared
    /// concurrency gate, and the usage accounting. Always present — the
    /// service reports itself unavailable when `[chat.ocr] enabled = false` or
    /// no healthy `ocr` pool serves a model, and the chat path then behaves
    /// exactly as it did before OCR existed. Lives here (not on `AppState`)
    /// because it needs the usage sink, and it must be *one* instance:
    /// per-request services would each get their own semaphore and bound
    /// nothing.
    pub ocr: gateway_features::server::ocr::OcrService,
    /// Rate-limit / quota gate, consulted by the `/v1` proxy, the chat send
    /// path, and the scheduler before a call runs. Reads the same DB; a no-op
    /// when `[limits] enabled = false` or the caller has no rules. See
    /// `server::limits`.
    pub enforcer: Arc<gateway_core::server::limits::Enforcer>,
    /// In-memory count of unapplied upstream-topology edits — pool/backend
    /// saves and deletes bump it, `POST /admin/upstreams/reload` ("Apply
    /// changes") resets it. It measures the drift between the DB topology (what
    /// the admin has edited) and the runtime registry (what the gateway is
    /// actually serving), which the `/admin/upstreams` page surfaces as a
    /// sticky "N unapplied changes" bar. Not persisted: after a restart the
    /// registry is rebuilt from the DB, so a fresh 0 is correct.
    topology_dirty: Arc<AtomicU32>,
}

impl RamaState {
    pub fn new(inner: AppState, sessions: SessionStore, usage: UsageHandle) -> Self {
        let enforcer = Arc::new(gateway_core::server::limits::Enforcer::new(
            inner.db.clone(),
            inner.config().limits.enabled,
        ));
        let ocr = build_ocr(&inner, &usage);
        Self {
            inner,
            sessions,
            chats: Arc::new(SessionWorkers::default()),
            location_feedback: Arc::new(crate::server::tools::feedback::FeedbackHub::default()),
            ask_feedback: Arc::new(crate::server::tools::feedback::FeedbackHub::default()),
            usage,
            ocr,
            enforcer,
            topology_dirty: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Record one unapplied topology edit and return the new count. Called by
    /// every pool/backend save + delete handler so the apply bar can nudge the
    /// admin to reload the registry.
    pub fn topology_dirty_bump(&self) -> u32 {
        self.topology_dirty.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Clear the unapplied-edit count — the registry now matches the DB. Called
    /// after a successful `POST /admin/upstreams/reload`.
    pub fn topology_dirty_reset(&self) {
        self.topology_dirty.store(0, Ordering::Relaxed);
    }

    /// The current unapplied-edit count, for the initial render of the apply
    /// bar (datastar keeps it live thereafter).
    pub fn topology_dirty_count(&self) -> u32 {
        self.topology_dirty.load(Ordering::Relaxed)
    }

    /// Swap in a different usage sink. Mainly for tests, which build state
    /// with a disabled handle and opt into a live metered one.
    pub fn with_usage(mut self, usage: UsageHandle) -> Self {
        // Rebuild the OCR service against the new sink: it captured the old
        // one, and a test that opts into metering expects OCR rows too.
        self.ocr = build_ocr(&self.inner, &usage);
        self.usage = usage;
        self
    }

    /// Share an already-built OCR service instead of the one [`Self::new`]
    /// constructs.
    ///
    /// The concurrency gate inside `OcrService` only bounds anything if every
    /// caller holds the *same* instance. The RAG indexer needs one before
    /// `RamaState` exists, so without this the gateway would run two
    /// independent gates and quietly allow twice the intended number of
    /// concurrent OCR calls against one GPU.
    pub fn with_ocr(mut self, ocr: gateway_features::server::ocr::OcrService) -> Self {
        self.ocr = ocr;
        self
    }

    /// Install the Web Push sender on the wrapped [`AppState`]. Production
    /// installs it on the `AppState` before `RamaState::new`; this mirror lets
    /// callers (and tests) opt into push after the fact.
    pub fn with_push(mut self, push: Arc<gateway_features::server::push::PushSender>) -> Self {
        self.inner = self.inner.with_push(push);
        self
    }
}

/// Build the shared OCR service from an [`AppState`]'s config + handles.
/// Constructing it is cheap and never fails: an unconfigured service simply
/// reports itself unavailable.
fn build_ocr(inner: &AppState, usage: &UsageHandle) -> gateway_features::server::ocr::OcrService {
    gateway_features::server::ocr::OcrService::new(
        inner.config().chat.ocr.clone(),
        inner.upstreams.clone(),
        inner.http.clone(),
        usage.clone(),
        inner.db.clone(),
    )
}

impl Deref for RamaState {
    type Target = AppState;
    fn deref(&self) -> &AppState {
        &self.inner
    }
}
