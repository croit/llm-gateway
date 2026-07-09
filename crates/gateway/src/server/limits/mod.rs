// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Rate-limit / quota enforcement.
//!
//! The [`Enforcer`] is consulted once per user-initiated call — the `/v1`
//! proxy, the chat UI send, and the scheduler all gate on it. It resolves the
//! caller's in-force limits (`db::limits::effective_limits`, the
//! global → role → user hierarchy) and compares each against the caller's
//! recent usage in that limit's sliding window (`db::usage::usage_in_window`,
//! which counts only metered rows).
//!
//! Enforcement is **post-hoc / debt-based**: a request is allowed while the
//! caller is *under* the limit; its own usage is settled afterwards, so the
//! request that crosses the line is served and the *next* one is refused. This
//! keeps the check a single cheap read and works with streaming (where the
//! token count isn't known until the end). A blocked call is a hard refusal
//! (HTTP 429 on the API; a visible error in the chat UI; a skipped, recorded
//! run for the scheduler).
//!
//! The usage read is the committed `usage_events` table, which the batched
//! writer flushes within ~500 ms — so a burst can overshoot by at most that
//! window's worth of traffic before it starts counting. That is well within
//! the bounded-overshoot the debt model already tolerates. The `Enforcer` is a
//! concrete struct today (in-process, one DB per deployment); it's the single
//! choke point to swap for a shared/atomic store if the gateway ever runs
//! multi-instance.

use std::collections::HashMap;

use jiff::Timestamp;

use super::db::Pool;
use super::db::limits::{self, Dimension, SubjectType, Window};
use super::db::usage::{self, WindowUsage};

/// Per-request limit gate. Cheap to clone (holds a pool handle + a flag).
#[derive(Clone)]
pub struct Enforcer {
    db: Pool,
    enabled: bool,
}

/// One in-force limit paired with the caller's current usage — the unit the
/// user's self-view renders as a progress bar, and what [`Enforcer::check`]
/// scans for a breach.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitStatus {
    pub model: Option<String>,
    pub dimension: Dimension,
    pub window: Window,
    pub limit: f64,
    pub used: f64,
    /// Which hierarchy level supplied the winning limit (for UI labelling).
    pub source: SubjectType,
    /// When the sliding window next advances (the next top of the hour).
    pub refreshes_at: Timestamp,
}

impl LimitStatus {
    /// Usage as a fraction of the limit, clamped to `[0, 1]` for the bar. A
    /// zero/negative limit reads as full (it can never be satisfied).
    pub fn fraction(&self) -> f64 {
        if self.limit <= 0.0 {
            return 1.0;
        }
        (self.used / self.limit).clamp(0.0, 1.0)
    }

    /// Whole-percent used, for the bar label.
    pub fn percent(&self) -> u32 {
        (self.fraction() * 100.0).round() as u32
    }

    /// True once the caller has hit or passed the limit (debt model: the next
    /// request is refused).
    pub fn exceeded(&self) -> bool {
        self.used >= self.limit
    }
}

/// A refused call: the first limit found already at/over its ceiling.
#[derive(Debug, Clone)]
pub struct LimitExceeded {
    pub model: Option<String>,
    pub dimension: Dimension,
    pub window: Window,
    pub limit: f64,
    pub used: f64,
    /// Seconds until the window next advances — served as `Retry-After`.
    pub retry_after_secs: i64,
}

impl Enforcer {
    pub fn new(db: Pool, enabled: bool) -> Self {
        Self { db, enabled }
    }

    /// Resolve the caller's in-force limits and pair each with current usage.
    /// Empty when enforcement is disabled or the caller has no applicable
    /// rules (the unlimited default). Fails open on a DB error — a metrics/
    /// limits read must never wedge live traffic.
    pub async fn statuses(&self, user_id: &str, role_ids: &[String]) -> Vec<LimitStatus> {
        if !self.enabled {
            return Vec::new();
        }
        let rules = match limits::applicable(&self.db, user_id, role_ids).await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err, "limits: applicable() failed; allowing");
                return Vec::new();
            }
        };
        if rules.is_empty() {
            return Vec::new();
        }
        let effective = limits::effective_limits(&rules);
        let now = Timestamp::now();
        // One usage read per distinct (model-scope, window); the three
        // dimensions share it.
        let mut cache: HashMap<(Option<String>, Window), WindowUsage> = HashMap::new();
        let mut out = Vec::with_capacity(effective.len());
        for lim in effective {
            let key = (lim.model.clone(), lim.window);
            let usage = match cache.get(&key) {
                Some(u) => *u,
                None => {
                    let since = lim.window.since(now);
                    let u = usage::usage_in_window(&self.db, user_id, since, lim.model.as_deref())
                        .await
                        .unwrap_or_default();
                    cache.insert(key, u);
                    u
                }
            };
            let used = match lim.dimension {
                Dimension::Requests => usage.requests as f64,
                Dimension::Tokens => usage.tokens as f64,
                Dimension::Cost => usage.cost,
            };
            out.push(LimitStatus {
                model: lim.model,
                dimension: lim.dimension,
                window: lim.window,
                limit: lim.value,
                used,
                source: lim.source,
                refreshes_at: lim.window.next_refresh(now),
            });
        }
        out
    }

    /// Gate a call. `Ok(())` to proceed; `Err` with the first breached limit
    /// (post-hoc debt: a limit already at/over its ceiling blocks the *next*
    /// call). Unlimited callers and disabled enforcement pass instantly.
    pub async fn check(&self, user_id: &str, role_ids: &[String]) -> Result<(), LimitExceeded> {
        let now = Timestamp::now();
        for s in self.statuses(user_id, role_ids).await {
            if s.exceeded() {
                let retry = (s.refreshes_at.as_second() - now.as_second()).max(1);
                return Err(LimitExceeded {
                    model: s.model,
                    dimension: s.dimension,
                    window: s.window,
                    limit: s.limit,
                    used: s.used,
                    retry_after_secs: retry,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::usage::{UsageKind, UsageRecord, UsageSource};

    async fn pool() -> Pool {
        crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap()
    }

    fn event(
        user: &str,
        model: &str,
        total: i64,
        enforce_limits: bool,
        at: Timestamp,
    ) -> UsageRecord {
        UsageRecord {
            created_at: at,
            user_id: user.into(),
            user_email: None,
            token_id: None,
            token_name: None,
            source: UsageSource::V1Api,
            kind: UsageKind::Chat,
            backend: "b".into(),
            model: model.into(),
            status: 200,
            duration_ms: 1,
            prompt_tokens: Some(total),
            completion_tokens: Some(0),
            total_tokens: Some(total),
            enforce_limits,
        }
    }

    #[tokio::test]
    async fn no_rules_means_unlimited() {
        let pool = pool().await;
        let enf = Enforcer::new(pool, true);
        assert!(enf.check("alice", &[]).await.is_ok());
        assert!(enf.statuses("alice", &[]).await.is_empty());
    }

    #[tokio::test]
    async fn disabled_enforcer_never_blocks() {
        let pool = pool().await;
        limits::upsert(
            &pool,
            SubjectType::Global,
            "",
            None,
            Dimension::Requests,
            Window::Hour,
            0.0,
        )
        .await
        .unwrap();
        let enf = Enforcer::new(pool, false);
        assert!(enf.check("alice", &[]).await.is_ok());
    }

    #[tokio::test]
    async fn blocks_once_usage_reaches_the_limit() {
        let pool = pool().await;
        // 2 requests / hour, global.
        limits::upsert(
            &pool,
            SubjectType::Global,
            "",
            None,
            Dimension::Requests,
            Window::Hour,
            2.0,
        )
        .await
        .unwrap();
        let enf = Enforcer::new(pool.clone(), true);
        let now = Timestamp::now();

        // No usage yet → allowed.
        assert!(enf.check("alice", &[]).await.is_ok());

        // Two metered requests recorded → at the ceiling → next is refused.
        usage::insert_batch(
            &pool,
            &[
                event("alice", "gpt", 1, true, now),
                event("alice", "gpt", 1, true, now),
            ],
        )
        .await
        .unwrap();
        let err = enf.check("alice", &[]).await.unwrap_err();
        assert_eq!(err.dimension, Dimension::Requests);
        assert_eq!(err.limit, 2.0);
        assert!(err.used >= 2.0);
        assert!(err.retry_after_secs >= 1);
    }

    #[tokio::test]
    async fn usage_outside_the_window_ages_out_and_resets() {
        let pool = pool().await;
        // 2 requests / hour, global.
        limits::upsert(
            &pool,
            SubjectType::Global,
            "",
            None,
            Dimension::Requests,
            Window::Hour,
            2.0,
        )
        .await
        .unwrap();
        let enf = Enforcer::new(pool.clone(), true);
        let now = Timestamp::now();
        let three_hours_ago = now
            .checked_sub(jiff::SignedDuration::from_hours(3))
            .unwrap();

        // Three requests, but all from 3h ago — outside the 1-hour sliding
        // window, so they've aged out and don't count. The window has, in
        // effect, reset.
        usage::insert_batch(
            &pool,
            &[
                event("alice", "gpt", 1, true, three_hours_ago),
                event("alice", "gpt", 1, true, three_hours_ago),
                event("alice", "gpt", 1, true, three_hours_ago),
            ],
        )
        .await
        .unwrap();
        assert!(
            enf.check("alice", &[]).await.is_ok(),
            "usage older than the window must not count (the window resets)"
        );
        let status = enf.statuses("alice", &[]).await;
        assert_eq!(status.len(), 1);
        assert_eq!(
            status[0].used, 0.0,
            "aged-out usage reads as 0 in the window"
        );

        // Fresh usage inside the window does count, and re-blocks.
        usage::insert_batch(
            &pool,
            &[
                event("alice", "gpt", 1, true, now),
                event("alice", "gpt", 1, true, now),
            ],
        )
        .await
        .unwrap();
        assert!(enf.check("alice", &[]).await.is_err());
    }

    #[tokio::test]
    async fn exempt_usage_does_not_count() {
        let pool = pool().await;
        limits::upsert(
            &pool,
            SubjectType::User,
            "alice",
            None,
            Dimension::Tokens,
            Window::Day,
            100.0,
        )
        .await
        .unwrap();
        let enf = Enforcer::new(pool.clone(), true);
        let now = Timestamp::now();
        // 500 tokens but on an EXEMPT (metered=false) pool → ignored.
        usage::insert_batch(&pool, &[event("alice", "local", 500, false, now)])
            .await
            .unwrap();
        assert!(enf.check("alice", &[]).await.is_ok());
        let st = enf.statuses("alice", &[]).await;
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].used, 0.0);
    }

    #[tokio::test]
    async fn per_model_scope_counts_only_that_model() {
        let pool = pool().await;
        limits::upsert(
            &pool,
            SubjectType::User,
            "alice",
            Some("pricey"),
            Dimension::Tokens,
            Window::Day,
            100.0,
        )
        .await
        .unwrap();
        let enf = Enforcer::new(pool.clone(), true);
        let now = Timestamp::now();
        usage::insert_batch(
            &pool,
            &[
                event("alice", "cheap", 999, true, now),
                event("alice", "pricey", 100, true, now),
            ],
        )
        .await
        .unwrap();
        // Only "pricey" usage (100) counts against the pricey-scoped limit → at ceiling.
        assert!(enf.check("alice", &[]).await.is_err());
    }
}
