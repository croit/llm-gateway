// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Warm pool of single-use sandbox containers.
//!
//! Security model: every job runs in a container that is used **exactly
//! once** and then destroyed — no state leaks between calls or users. To
//! hide container cold-start latency we keep `pool_size` pristine, idle
//! containers pre-booted; a job pops one, runs, and the container is torn
//! down while a replacement boots in the background.
//!
//! Networked calls (pip / browser) never reuse a pooled container: they
//! get a fresh on-demand container attached to the egress-proxy network,
//! so the default-deny warm pool stays default-deny.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use shared::sandbox::{RunRequest, RunResponse};
use thiserror::Error;
use tokio::sync::Semaphore;

use crate::backend::{ContainerBackend, Network};
use crate::config::Config;

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("sandbox backend error: {0}")]
    Backend(#[from] crate::backend::BackendError),
    #[error("sandbox at capacity")]
    Busy,
    #[error("network egress requested but not configured on this runner")]
    NetworkUnavailable,
}

pub struct Pool {
    backend: Arc<dyn ContainerBackend>,
    cfg: Arc<Config>,
    /// Pre-booted, default-deny containers awaiting a job.
    ready: Mutex<VecDeque<String>>,
    /// Caps concurrent in-flight jobs.
    sem: Semaphore,
    /// Serializes refill so we never overshoot `pool_size`.
    refill_lock: tokio::sync::Mutex<()>,
}

impl Pool {
    pub fn new(backend: Arc<dyn ContainerBackend>, cfg: Arc<Config>) -> Arc<Self> {
        let permits = cfg.max_concurrent.max(1);
        Arc::new(Self {
            backend,
            cfg,
            ready: Mutex::new(VecDeque::new()),
            sem: Semaphore::new(permits),
            refill_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Number of pre-booted containers currently idle. Test-only today;
    /// promote to `pub` (and surface via `/healthz`) if we add readiness
    /// reporting.
    #[cfg(test)]
    pub fn ready_len(&self) -> usize {
        self.ready.lock().unwrap().len()
    }

    /// Boot containers until the ready queue reaches `pool_size`. Held
    /// behind `refill_lock` so concurrent callers can't overshoot.
    pub async fn refill(self: &Arc<Self>) {
        let _g = self.refill_lock.lock().await;
        loop {
            let have = self.ready.lock().unwrap().len();
            if have >= self.cfg.pool_size {
                break;
            }
            match self.backend.create(Network::None).await {
                Ok(id) => self.ready.lock().unwrap().push_back(id),
                Err(e) => {
                    tracing::warn!(error = %e, "warm-pool refill failed; will retry later");
                    break;
                }
            }
        }
    }

    /// Run one job. Acquires a concurrency permit, obtains a container
    /// (pooled for default-deny, on-demand for egress), executes, and
    /// tears the container down. A background refill tops the pool back
    /// up for pooled jobs.
    pub async fn run(self: &Arc<Self>, req: &RunRequest) -> Result<RunResponse, RunnerError> {
        let _permit = self.sem.try_acquire().map_err(|_| RunnerError::Busy)?;

        let want_egress = req.network;
        if want_egress && !self.cfg.egress_available() {
            return Err(RunnerError::NetworkUnavailable);
        }

        let (id, pooled) = if want_egress {
            (self.backend.create(Network::Egress).await?, false)
        } else {
            // Scope the std MutexGuard so it's dropped before any `.await`
            // below — holding it across a suspend point would make this
            // future non-Send (and rama handlers must be Send).
            let pooled = { self.ready.lock().unwrap().pop_front() };
            match pooled {
                Some(id) => (id, true),
                None => (self.backend.create(Network::None).await?, false),
            }
        };

        let timeout = self.cfg.effective_timeout(req.timeout_secs);
        let started = Instant::now();
        let result = self.backend.exec(&id, req, timeout).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        // Single-use: always destroy, regardless of outcome.
        let backend = self.backend.clone();
        let dead_id = id.clone();
        tokio::spawn(async move { backend.destroy(&dead_id).await });

        // Top the pool back up after consuming a pooled container.
        if pooled {
            let this = self.clone();
            tokio::spawn(async move { this.refill().await });
        }

        let mut resp = result?;
        // Trust the runner's own wall-clock over the agent's self-report.
        if !resp.timed_out {
            resp.duration_ms = elapsed_ms;
        }
        clamp_output(&mut resp, self.cfg.max_output_bytes);
        Ok(resp)
    }
}

/// Clip stdout/stderr to the configured cap so a runaway job can't return
/// gigabytes through the gateway (and blow the model's context). Keeps a
/// HEAD and a TAIL with an omission marker in the middle — for logs and
/// tracebacks the decisive part (the error, the exit) is at the end, so a
/// head-only truncation would hide exactly what matters. The agent
/// separately preserves the full stream as an attachment when it's large,
/// so nothing is actually lost. Marks `output_truncated` when it bites.
fn clamp_output(resp: &mut RunResponse, max: usize) {
    // Split the budget so one stream can't starve the other.
    let half = max.max(2) / 2;
    let a = clip_head_tail(&mut resp.stdout, half);
    let b = clip_head_tail(&mut resp.stderr, half);
    if a || b {
        resp.output_truncated = true;
    }
}

/// Keep ~60% head + ~40% tail of `s` within `budget` bytes, char-boundary
/// safe, with a marker naming how much was dropped. Returns whether it cut.
fn clip_head_tail(s: &mut String, budget: usize) -> bool {
    if s.len() <= budget {
        return false;
    }
    let total = s.len();
    let head_budget = (budget * 6 / 10).max(1);
    let tail_budget = budget.saturating_sub(head_budget);
    let mut h = head_budget.min(total);
    while h > 0 && !s.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = total.saturating_sub(tail_budget);
    while t < total && !s.is_char_boundary(t) {
        t += 1;
    }
    if t < h {
        t = h;
    }
    let omitted = t - h;
    let head = s[..h].to_string();
    let tail = s[t..].to_string();
    *s = format!(
        "{head}\n…[{omitted} bytes omitted; full output saved as a stdout/stderr attachment]…\n{tail}"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::{FakeBackend, req};

    fn cfg(pool_size: usize, max_concurrent: usize, egress: bool) -> Arc<Config> {
        Arc::new(Config {
            bind: "127.0.0.1:9000".into(),
            image: "img".into(),
            runtime: "runsc".into(),
            podman: "podman".into(),
            pool_size,
            max_concurrent,
            default_timeout_secs: 60,
            max_timeout_secs: 300,
            memory: "1024m".into(),
            cpus: "2".into(),
            pids_limit: 256,
            work_size: "512m".into(),
            tmp_size: "512m".into(),
            max_output_bytes: 1_048_576,
            egress_network: if egress {
                "egress".into()
            } else {
                String::new()
            },
            egress_proxy: if egress {
                "http://proxy:3128".into()
            } else {
                String::new()
            },
        })
    }

    async fn settle_ready(pool: &Arc<Pool>, want: usize) {
        for _ in 0..2000 {
            if pool.ready_len() >= want {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!(
            "pool never reached {want} ready (have {})",
            pool.ready_len()
        );
    }

    #[tokio::test]
    async fn refill_warms_to_pool_size() {
        let be = Arc::new(FakeBackend::new());
        let pool = Pool::new(be.clone(), cfg(3, 6, false));
        pool.refill().await;
        assert_eq!(pool.ready_len(), 3);
        assert_eq!(be.live_count(), 3);
    }

    #[tokio::test]
    async fn run_consumes_a_pooled_container_then_refills() {
        let be = Arc::new(FakeBackend::new());
        let pool = Pool::new(be.clone(), cfg(2, 4, false));
        pool.refill().await;
        assert_eq!(pool.ready_len(), 2);

        let resp = pool.run(&req()).await.unwrap();
        assert_eq!(resp.exit_code, 0);
        assert!(resp.stdout.contains("ran python"));

        // The consumed container is destroyed and the pool refilled.
        settle_ready(&pool, 2).await;
        assert_eq!(be.destroyed.lock().unwrap().len(), 1, "single-use teardown");
        // Created: 2 warm + 1 refill = 3; one destroyed → 2 live.
        assert_eq!(be.live_count(), 2);
    }

    #[tokio::test]
    async fn pooled_container_is_default_deny_not_egress() {
        let be = Arc::new(FakeBackend::new());
        let pool = Pool::new(be.clone(), cfg(1, 2, true));
        pool.refill().await;
        let created = be.created.lock().unwrap();
        assert!(created.iter().all(|(_, net)| *net == Network::None));
    }

    #[tokio::test]
    async fn egress_request_creates_on_demand_networked_container() {
        let be = Arc::new(FakeBackend::new());
        let pool = Pool::new(be.clone(), cfg(0, 2, true));
        let mut r = req();
        r.network = true;
        pool.run(&r).await.unwrap();
        let created = be.created.lock().unwrap();
        assert!(
            created.iter().any(|(_, net)| *net == Network::Egress),
            "a networked call must get an egress container: {created:?}"
        );
    }

    #[tokio::test]
    async fn egress_request_rejected_when_not_configured() {
        let be = Arc::new(FakeBackend::new());
        let pool = Pool::new(be.clone(), cfg(1, 2, false));
        let mut r = req();
        r.network = true;
        let err = pool.run(&r).await.unwrap_err();
        assert!(matches!(err, RunnerError::NetworkUnavailable));
    }

    #[test]
    fn clamp_output_keeps_head_and_tail() {
        // Distinct head/tail so we can prove BOTH ends survive.
        let body = format!("{}{}", "A".repeat(500), "Z".repeat(500));
        let mut resp = RunResponse {
            exit_code: 0,
            stdout: body,
            stderr: String::new(),
            artifacts: vec![],
            duration_ms: 0,
            timed_out: false,
            output_truncated: false,
        };
        clamp_output(&mut resp, 200);
        assert!(resp.output_truncated);
        assert!(resp.stdout.len() < 1000);
        assert!(resp.stdout.contains("omitted"));
        assert!(resp.stdout.starts_with('A'), "head kept");
        assert!(resp.stdout.ends_with('Z'), "tail kept");
    }
}
