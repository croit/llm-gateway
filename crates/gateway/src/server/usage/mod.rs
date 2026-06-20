// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Usage-metrics ingest: a batched background writer that decouples
//! measurement from the request path.
//!
//! Hot paths (the `/v1` proxy, the chat driver, the scheduler) build a
//! [`UsageRecord`] and hand it to [`UsageHandle::emit`] — a non-blocking
//! `try_send` onto a bounded channel. A single background task drains the
//! channel and flushes in batches (whichever comes first of [`FLUSH_EVERY`]
//! or [`MAX_BATCH`] records) inside one transaction, so thousands of
//! requests collapse into a handful of writes per second and never contend
//! with the chat hot path's per-delta writes.
//!
//! Recording must never block or fail a request: if the channel is full we
//! drop the record and bump a counter (logged periodically) rather than
//! apply backpressure. This is metrics, not billing — a dropped sample
//! under extreme load is acceptable; a stalled response is not.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use tokio::sync::mpsc;

use super::db::usage::UsageRecord;
use super::db::{self, Pool};

/// Bounded channel capacity. At hundreds/min this is never close to full;
/// the bound exists purely to cap memory if the writer ever stalls.
const CHANNEL_CAPACITY: usize = 10_000;

/// Flush a partial batch at least this often, so low-traffic events still
/// land promptly instead of waiting for [`MAX_BATCH`].
const FLUSH_EVERY: Duration = Duration::from_millis(500);

/// Max records per flush transaction.
const MAX_BATCH: usize = 1_000;

/// How often the maintenance task prunes expired raw rows.
const PRUNE_EVERY: Duration = Duration::from_secs(3_600);

/// Cheap, cloneable handle the hot paths hold (lives on `RamaState`).
/// `emit` is non-blocking and infallible by contract.
#[derive(Clone)]
pub struct UsageHandle {
    tx: Option<mpsc::Sender<UsageRecord>>,
    dropped: Arc<AtomicU64>,
}

impl UsageHandle {
    /// A disabled handle: `emit` is a no-op. Used when `[usage] enabled =
    /// false`, and as the default in tests that don't exercise metrics.
    pub fn disabled() -> Self {
        Self {
            tx: None,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Record one measured call. Never blocks; on a full/closed channel the
    /// record is dropped and a counter incremented.
    pub fn emit(&self, record: UsageRecord) {
        let Some(tx) = self.tx.as_ref() else {
            return; // disabled
        };
        if tx.try_send(record).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether metrics are on. Hot paths check this to skip building a
    /// record (and any lookups feeding it) entirely when `[usage].enabled`
    /// is `false` — a true kill switch, not just a dropped write.
    pub fn is_enabled(&self) -> bool {
        self.tx.is_some()
    }

    /// Total records dropped because the channel was full/closed. For tests
    /// and the periodic warning.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Build the handle and spawn the writer + maintenance tasks. Returns a
/// `UsageHandle` to store on the shared state. Call once at startup when
/// `[usage] enabled` (else use [`UsageHandle::disabled`]).
pub fn spawn(pool: Pool, retention_days: i64) -> UsageHandle {
    let (tx, rx) = mpsc::channel::<UsageRecord>(CHANNEL_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));

    spawn_writer(pool.clone(), rx, dropped.clone());
    spawn_maintenance(pool, retention_days);

    UsageHandle {
        tx: Some(tx),
        dropped,
    }
}

/// Drain the channel and flush in batches. Runs until the channel closes
/// (process exit). A failed flush is logged and the batch dropped — the
/// loop keeps going rather than wedging on a transient DB error.
fn spawn_writer(pool: Pool, mut rx: mpsc::Receiver<UsageRecord>, dropped: Arc<AtomicU64>) {
    tokio::spawn(async move {
        let mut batch: Vec<UsageRecord> = Vec::with_capacity(MAX_BATCH);
        let mut last_dropped_logged: u64 = 0;
        loop {
            // Block for the first record, then greedily drain whatever else
            // is queued (up to MAX_BATCH) or until FLUSH_EVERY elapses.
            let Some(first) = rx.recv().await else {
                // Channel closed: flush anything pending and stop.
                if !batch.is_empty() {
                    flush(&pool, &batch).await;
                }
                return;
            };
            batch.push(first);

            let deadline = tokio::time::sleep(FLUSH_EVERY);
            tokio::pin!(deadline);
            while batch.len() < MAX_BATCH {
                tokio::select! {
                    maybe = rx.recv() => match maybe {
                        Some(rec) => batch.push(rec),
                        None => break, // closed; flush below then loop exits next recv
                    },
                    _ = &mut deadline => break,
                }
            }

            flush(&pool, &batch).await;
            batch.clear();

            // Surface dropped samples occasionally (not per-flush spam).
            let total = dropped.load(Ordering::Relaxed);
            if total > last_dropped_logged {
                tracing::warn!(
                    dropped = total,
                    "usage: channel full — some metrics samples were dropped"
                );
                last_dropped_logged = total;
            }
        }
    });
}

async fn flush(pool: &Pool, batch: &[UsageRecord]) {
    if let Err(err) = db::usage::insert_batch(pool, batch).await {
        tracing::warn!(error = %err, rows = batch.len(), "usage: batch flush failed");
    }
}

/// Periodically delete raw rows older than the retention window. Rollups
/// are kept forever.
fn spawn_maintenance(pool: Pool, retention_days: i64) {
    // Clamp to a sane floor: a zero/negative window would prune everything
    // every hour. `saturating_mul` keeps an absurd config from overflowing.
    let retention_hours = retention_days.max(1).saturating_mul(24);
    tokio::spawn(async move {
        loop {
            let cutoff = Timestamp::now()
                .checked_sub(SignedDuration::from_hours(retention_hours))
                .unwrap_or_else(|_| Timestamp::now());
            match db::usage::prune(&pool, cutoff).await {
                Ok(n) if n > 0 => tracing::info!(pruned = n, "usage: pruned expired raw rows"),
                Ok(_) => {}
                Err(err) => tracing::warn!(error = %err, "usage: prune pass failed"),
            }
            tokio::time::sleep(PRUNE_EVERY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::usage::{UsageKind, UsageSource};

    fn sample() -> UsageRecord {
        UsageRecord {
            created_at: Timestamp::now(),
            user_id: "alice".into(),
            user_email: Some("alice@x".into()),
            token_id: None,
            token_name: None,
            source: UsageSource::Chat,
            kind: UsageKind::Chat,
            backend: "gpu-01".into(),
            model: "qwen".into(),
            status: 200,
            duration_ms: 5,
            prompt_tokens: Some(1),
            completion_tokens: Some(1),
            total_tokens: Some(2),
        }
    }

    #[tokio::test]
    async fn emitted_records_land_as_rows_after_flush() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let handle = spawn(pool.clone(), 90);
        for _ in 0..3 {
            handle.emit(sample());
        }
        // Give the writer a couple of flush windows to drain.
        tokio::time::sleep(Duration::from_millis(700)).await;
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(handle.dropped(), 0);
    }

    #[test]
    fn disabled_handle_is_a_noop() {
        let handle = UsageHandle::disabled();
        assert!(!handle.is_enabled());
        handle.emit(sample()); // must not panic
        assert_eq!(handle.dropped(), 0);
    }

    #[tokio::test]
    async fn spawned_handle_reports_enabled() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        assert!(spawn(pool, 90).is_enabled());
    }
}
