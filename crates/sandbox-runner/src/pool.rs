// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Warm pool of sandbox containers.
//!
//! Security model: by default every job runs in a container that is used
//! **exactly once** and then destroyed — no state leaks between users. To
//! hide container cold-start latency we keep `pool_size` pristine, idle
//! containers pre-booted; a job pops one, runs, and the container is torn
//! down while a replacement boots in the background.
//!
//! Exception — **leases**: a `keep_alive` job (the gateway's `run_in_sandbox`
//! within one conversation turn) keeps its container alive in the `leased`
//! table so later calls in the same turn reuse it (persisting `/work`). It's
//! freed by an explicit `DELETE /container/{id}` at turn end, with a TTL
//! sweeper reaping any lease a crashed turn left behind. This never crosses a
//! user boundary: a turn is one authenticated caller.
//!
//! Networked calls (pip / browser) never reuse a pooled container: they
//! get a fresh on-demand container attached to the egress-proxy network,
//! so the default-deny warm pool stays default-deny.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
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

/// A pre-booted, idle container plus the workload-image id it was created
/// from. Stamping the image lets the pool spot containers left over from a
/// previous image after a rebuild / re-tag and recycle them.
struct Warm {
    id: String,
    image: String,
}

/// A container kept alive across calls within one conversation turn (a
/// "lease"). Unlike a [`Warm`] container it is *not* pristine — the turn's
/// earlier calls may have written `/work`, set env, etc. — which is exactly
/// the point. Freed explicitly by the gateway at turn end
/// ([`Pool::release_container`]); the [`Pool::sweep_expired`] backstop reaps
/// one whose gateway died mid-turn.
struct Leased {
    /// Instant of the most recent exec's *completion* (not start), so a
    /// long-running exec doesn't look idle while it runs.
    last_used: Instant,
    /// A job is currently exec'ing in this container — the sweeper must not
    /// reap it even if `last_used` is old (a single exec can legitimately
    /// run up to `max_timeout_secs`).
    busy: bool,
}

pub struct Pool {
    backend: Arc<dyn ContainerBackend>,
    cfg: Arc<Config>,
    /// Pre-booted, default-deny containers awaiting a job.
    ready: Mutex<VecDeque<Warm>>,
    /// Containers kept alive for reuse within a turn, keyed by container id.
    leased: Mutex<HashMap<String, Leased>>,
    /// The workload-image id the pool is currently warming to. Empty until
    /// the first [`Self::refresh_image`]; updated when the image changes.
    image: Mutex<String>,
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
            leased: Mutex::new(HashMap::new()),
            image: Mutex::new(String::new()),
            sem: Semaphore::new(permits),
            refill_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Spawn the background lease sweeper: every 30 s, reap any leased
    /// container that has been idle (no in-flight job) longer than
    /// `lease_ttl_secs`. This is the crash/leak backstop — in the normal
    /// case the gateway frees leases explicitly at turn end. Called once at
    /// boot (from `main`); a no-op-friendly loop that runs for the process
    /// lifetime.
    pub fn spawn_sweeper(self: &Arc<Self>) {
        let this = self.clone();
        // Tick often enough that a small configured TTL is actually honored,
        // but never busier than once a second; capped at 30s so a large TTL
        // doesn't spin. (Reap latency ≈ TTL + up to one tick.)
        let tick = Duration::from_secs(self.cfg.lease_ttl_secs.clamp(1, 30));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                this.sweep_expired().await;
            }
        });
    }

    /// Reap leased containers idle longer than the TTL. Skips any with an
    /// in-flight job (`busy`), so a legitimately long exec is never torn out
    /// from under itself.
    async fn sweep_expired(self: &Arc<Self>) {
        let ttl = Duration::from_secs(self.cfg.lease_ttl_secs);
        let now = Instant::now();
        let expired: Vec<String> = {
            let leased = self.leased.lock().unwrap();
            leased
                .iter()
                .filter(|(_, l)| !l.busy && now.saturating_duration_since(l.last_used) > ttl)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in expired {
            tracing::warn!(
                container = %id,
                "sandbox lease expired (gateway did not release it — turn crashed?); reaping"
            );
            self.reap_if_idle(&id).await;
        }
    }

    /// Reap a lease only if it's still idle, re-checking `busy` **under the
    /// same lock** as the removal. The gap between `sweep_expired` collecting
    /// an id and destroying it is a window in which a fresh `/run` can reuse
    /// that id and start an exec (marking it busy); this re-check ensures we
    /// don't destroy a container mid-exec. Nothing removed → nothing destroyed.
    async fn reap_if_idle(self: &Arc<Self>, id: &str) {
        let removed = {
            let mut leased = self.leased.lock().unwrap();
            match leased.get(id) {
                Some(l) if !l.busy => leased.remove(id).is_some(),
                _ => false,
            }
        };
        if removed {
            self.backend.destroy(id).await;
        }
    }

    /// Free a leased container: drop it from the lease table and destroy it.
    /// Idempotent — a DELETE for an id we no longer track (already swept, or
    /// never leased) still best-effort destroys and returns cleanly, so the
    /// gateway's turn-end release and the sweeper can't double-fault.
    pub async fn release_container(self: &Arc<Self>, id: &str) {
        let tracked = { self.leased.lock().unwrap().remove(id).is_some() };
        // Destroy regardless: the id came from us, and `destroy` is itself
        // best-effort (a no-op on an unknown/gone container).
        self.backend.destroy(id).await;
        if tracked {
            tracing::debug!(container = %id, "sandbox lease released");
        }
    }

    /// Number of live leases — test/observability only.
    #[cfg(test)]
    pub fn leased_len(&self) -> usize {
        self.leased.lock().unwrap().len()
    }

    /// Number of pre-booted containers currently idle. Test-only today;
    /// promote to `pub` (and surface via `/healthz`) if we add readiness
    /// reporting.
    #[cfg(test)]
    pub fn ready_len(&self) -> usize {
        self.ready.lock().unwrap().len()
    }

    /// The image ids stamped on the currently-idle warm containers.
    #[cfg(test)]
    pub fn ready_images(&self) -> Vec<String> {
        self.ready
            .lock()
            .unwrap()
            .iter()
            .map(|w| w.image.clone())
            .collect()
    }

    /// Boot containers until the ready queue reaches `pool_size`. Held
    /// behind `refill_lock` so concurrent callers can't overshoot. Each
    /// new container is stamped with the pool's current target image so a
    /// later image change can tell it apart from a fresh one.
    pub async fn refill(self: &Arc<Self>) {
        let _g = self.refill_lock.lock().await;
        let target = self.image.lock().unwrap().clone();
        loop {
            let have = self.ready.lock().unwrap().len();
            if have >= self.cfg.pool_size {
                break;
            }
            match self.backend.create(Network::None).await {
                Ok(id) => self.ready.lock().unwrap().push_back(Warm {
                    id,
                    image: target.clone(),
                }),
                Err(e) => {
                    tracing::warn!(error = %e, "warm-pool refill failed; will retry later");
                    break;
                }
            }
        }
    }

    /// Re-resolve the workload image's id and, if it changed since the pool
    /// was last warmed (a rebuild / re-tag), destroy the now-stale idle
    /// containers and re-warm on the new image. Seeds the target image on
    /// the first call (boot). Best-effort: a failed id lookup leaves the
    /// pool as-is rather than tearing it down on a transient error.
    pub async fn refresh_image(self: &Arc<Self>) {
        let now = match self.backend.image_id().await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "sandbox image-id check failed; keeping current pool");
                return;
            }
        };
        let prev = {
            let mut g = self.image.lock().unwrap();
            if *g == now {
                return; // unchanged — nothing to do
            }
            std::mem::replace(&mut *g, now.clone())
        };
        if prev.is_empty() {
            tracing::info!(image = %short(&now), "sandbox warm pool: seeding on image");
        } else {
            tracing::info!(
                prev = %short(&prev),
                new = %short(&now),
                "sandbox workload image changed — recycling warm pool"
            );
            // Drop every stale idle container; in-flight jobs are single-use
            // and torn down on completion regardless.
            let stale: Vec<Warm> = { self.ready.lock().unwrap().drain(..).collect() };
            for w in stale {
                let backend = self.backend.clone();
                tokio::spawn(async move { backend.destroy(&w.id).await });
            }
        }
        self.refill().await;
    }

    /// Obtain a *fresh* container for a job: a warm pooled one for
    /// default-deny, an on-demand one for egress. Returns `(id, pooled)`
    /// where `pooled` means it came off the warm queue (so the pool should
    /// refill). The std `MutexGuard`s are scoped so they never straddle an
    /// `.await` (that would make the future non-Send; rama handlers must be
    /// Send).
    async fn acquire_fresh(
        self: &Arc<Self>,
        want_egress: bool,
    ) -> Result<(String, bool), RunnerError> {
        if want_egress {
            return Ok((self.backend.create(Network::Egress).await?, false));
        }
        let warm = { self.ready.lock().unwrap().pop_front() };
        let target = { self.image.lock().unwrap().clone() };
        match warm {
            // Defensive against the brief race where the image changed but
            // the periodic refresh hasn't drained yet: a popped container
            // stamped with a superseded image is destroyed, not served, and
            // we boot a fresh one (which uses the new image).
            Some(w) if !target.is_empty() && w.image != target => {
                let backend = self.backend.clone();
                let stale = w.id.clone();
                tokio::spawn(async move { backend.destroy(&stale).await });
                Ok((self.backend.create(Network::None).await?, false))
            }
            Some(w) => Ok((w.id, true)),
            None => Ok((self.backend.create(Network::None).await?, false)),
        }
    }

    /// Run one job. Acquires a concurrency permit, obtains a container
    /// (reusing a live lease when `req.container_id` names one, else a fresh
    /// pooled/on-demand one), executes, and disposes of the container per
    /// `req.keep_alive`:
    ///
    /// - **reused lease** → refreshed and kept; response echoes its id.
    /// - **fresh + `keep_alive`** → registered as a lease *if* under the
    ///   `max_leases` cap, response echoes its id; when the cap is full the
    ///   job still ran, the container is destroyed single-use, and the
    ///   response carries `container_id: None` (graceful fallback).
    /// - **fresh, no `keep_alive`** → destroyed single-use (today's default).
    ///
    /// A background refill tops the pool back up whenever a warm container
    /// was consumed (whether it became a lease or was torn down).
    pub async fn run(self: &Arc<Self>, req: &RunRequest) -> Result<RunResponse, RunnerError> {
        let _permit = self.sem.try_acquire().map_err(|_| RunnerError::Busy)?;

        let want_egress = req.network;
        // Egress posture is only decided when creating a container. On reuse
        // the leased container's fixed network stands, so don't reject a
        // reuse call just because this runner has no egress wired.
        let creating = req.container_id.is_none();
        if want_egress && creating && !self.cfg.egress_available() {
            return Err(RunnerError::NetworkUnavailable);
        }

        // Resolve the container. `reused` marks a live lease we exec into
        // again (mark it busy so the sweeper leaves it alone); `pooled` marks
        // a warm container consumed from the queue (so we refill).
        let mut reused = false;
        let mut pooled = false;
        let id = match req.container_id.as_deref() {
            Some(want) if self.mark_lease_busy(want) => {
                reused = true;
                want.to_string()
            }
            // Either no reuse requested, or the named lease is gone (swept /
            // expired): obtain a fresh container. `want_egress` reflects the
            // caller's request for the fresh case.
            _ => {
                let (fresh, from_pool) = self.acquire_fresh(want_egress).await?;
                pooled = from_pool;
                fresh
            }
        };

        let timeout = self.cfg.effective_timeout(req.timeout_secs);
        let started = Instant::now();
        let result = self.backend.exec(&id, req, timeout).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        // Decide the container's fate and what id (if any) to echo back. A
        // wall-clock timeout comes back as `Ok(timed_out:true)` but must be
        // treated as a failure here: the runaway process is still alive inside
        // (the exec was abandoned, not killed), so the container has to be torn
        // down, never kept as a lease.
        let exec_ok = matches!(&result, Ok(r) if !r.timed_out);
        let lease_id = self.dispose(&id, reused, req.keep_alive, exec_ok);

        // Top the warm pool back up whenever we consumed a warm container —
        // it left the ready queue regardless of whether it became a lease.
        if pooled {
            let this = self.clone();
            tokio::spawn(async move { this.refill().await });
        }

        let mut resp = result?;
        // Trust the runner's own wall-clock over the agent's self-report.
        if !resp.timed_out {
            resp.duration_ms = elapsed_ms;
        }
        resp.container_id = lease_id;
        clamp_output(&mut resp, self.cfg.max_output_bytes);
        Ok(resp)
    }

    /// Mark an existing lease busy (so the sweeper won't reap it) if it's
    /// live. Returns whether the id named a tracked lease.
    fn mark_lease_busy(&self, id: &str) -> bool {
        let mut leased = self.leased.lock().unwrap();
        match leased.get_mut(id) {
            Some(l) => {
                l.busy = true;
                true
            }
            None => false,
        }
    }

    /// Post-exec disposition of a container. Returns the id to echo in the
    /// response iff the container is (still) leased, else `None`. Never
    /// awaits — teardown is spawned so a slow `podman rm` doesn't stall the
    /// response.
    fn dispose(
        self: &Arc<Self>,
        id: &str,
        reused: bool,
        keep_alive: bool,
        exec_ok: bool,
    ) -> Option<String> {
        if reused {
            if exec_ok {
                // Existing lease still good: clear busy + refresh idle timer.
                let mut leased = self.leased.lock().unwrap();
                if let Some(l) = leased.get_mut(id) {
                    l.busy = false;
                    l.last_used = Instant::now();
                }
                return Some(id.to_string());
            }
            // The reused container's job failed or timed out — treat the lease
            // as poisoned: drop it and destroy the container so the gateway's
            // next call recreates a clean one (a stale id it re-sends will miss
            // the table and fall through to a fresh container) instead of
            // re-exec'ing into a dead/saturated container all turn.
            self.leased.lock().unwrap().remove(id);
            self.spawn_destroy(id.to_string());
            return None;
        }
        // Only turn a *fresh* container into a lease when its job actually
        // ran: on a backend error the runner returns no body, so the gateway
        // would never learn this id to release it — don't strand a container
        // it can't address (the single-use teardown below handles it).
        if keep_alive && exec_ok {
            // New lease requested. Register only if under the cap; otherwise
            // fall back to single-use so idle leases can't over-commit RAM.
            let registered = {
                let mut leased = self.leased.lock().unwrap();
                if leased.len() < self.cfg.max_leases {
                    leased.insert(
                        id.to_string(),
                        Leased {
                            last_used: Instant::now(),
                            busy: false,
                        },
                    );
                    true
                } else {
                    false
                }
            };
            if registered {
                return Some(id.to_string());
            }
            tracing::warn!(
                max_leases = self.cfg.max_leases,
                "sandbox lease cap reached; running single-use (no persistence for this turn)"
            );
        }
        // Single-use teardown (default, or lease-cap fallback).
        self.spawn_destroy(id.to_string());
        None
    }

    /// Fire-and-forget container teardown.
    fn spawn_destroy(self: &Arc<Self>, id: String) {
        let backend = self.backend.clone();
        tokio::spawn(async move { backend.destroy(&id).await });
    }
}

/// First few chars of an image id, for readable logs (`sha256:abcd…`).
fn short(id: &str) -> &str {
    let end = id
        .char_indices()
        .nth(19)
        .map(|(i, _)| i)
        .unwrap_or(id.len());
    &id[..end]
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
            image_check_secs: 0,
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
            lease_ttl_secs: 600,
            max_leases: 6,
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

    /// Wait for at least `want` destroys to land (teardown is spawned, so a
    /// synchronous assert right after would race it).
    async fn settle_destroyed(be: &Arc<FakeBackend>, want: usize) {
        for _ in 0..2000 {
            if be.destroyed.lock().unwrap().len() >= want {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!(
            "never reached {want} destroyed (have {})",
            be.destroyed.lock().unwrap().len()
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
    async fn refresh_image_seeds_then_recycles_on_change() {
        let be = Arc::new(FakeBackend::new()); // image "img-v1"
        let pool = Pool::new(be.clone(), cfg(2, 4, false));
        // First refresh seeds the image and warms the pool.
        pool.refresh_image().await;
        settle_ready(&pool, 2).await;
        assert!(pool.ready_images().iter().all(|i| i == "img-v1"));
        let created_v1 = be.created.lock().unwrap().len();
        assert_eq!(created_v1, 2);

        // Image gets rebuilt/re-tagged → next refresh drains v1 and re-warms.
        be.set_image("img-v2");
        pool.refresh_image().await;
        settle_ready(&pool, 2).await;
        assert!(
            pool.ready_images().iter().all(|i| i == "img-v2"),
            "pool must be re-warmed on the new image: {:?}",
            pool.ready_images()
        );
        // The two v1 containers were destroyed and two v2 ones created.
        settle_destroyed(&be, 2).await;
        assert_eq!(be.destroyed.lock().unwrap().len(), 2);
        assert_eq!(be.created.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn refresh_image_noop_when_unchanged() {
        let be = Arc::new(FakeBackend::new());
        let pool = Pool::new(be.clone(), cfg(2, 4, false));
        pool.refresh_image().await;
        settle_ready(&pool, 2).await;
        let created = be.created.lock().unwrap().len();
        // Same image → no drain, no re-create.
        pool.refresh_image().await;
        assert_eq!(be.created.lock().unwrap().len(), created);
        assert_eq!(be.destroyed.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn stale_warm_container_is_discarded_on_checkout() {
        // Simulate the race: pool warmed on v1, image flips to v2, but the
        // periodic drain hasn't run yet. A job must NOT be served the stale
        // v1 container — it's destroyed and a fresh one booted.
        let be = Arc::new(FakeBackend::new());
        let pool = Pool::new(be.clone(), cfg(1, 4, false));
        pool.refresh_image().await;
        settle_ready(&pool, 1).await;
        // Flip the pool's notion of the target image WITHOUT draining (mimic
        // the window before refresh_image fires).
        be.set_image("img-v2");
        *pool.image.lock().unwrap() = "img-v2".to_string();
        let before = be.created.lock().unwrap().len();
        pool.run(&req()).await.unwrap();
        // The stale v1 container was destroyed; a fresh on-demand one ran.
        assert!(be.created.lock().unwrap().len() > before);
        settle_destroyed(&be, 1).await;
        assert!(!be.destroyed.lock().unwrap().is_empty());
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

    /// A `Config` with lease knobs made explicit, for the lease tests.
    fn cfg_leases(pool_size: usize, max_leases: usize, lease_ttl_secs: u64) -> Arc<Config> {
        let mut c = (*cfg(pool_size, 8, false)).clone();
        c.max_leases = max_leases;
        c.lease_ttl_secs = lease_ttl_secs;
        Arc::new(c)
    }

    #[tokio::test]
    async fn keep_alive_leases_and_reuses_the_same_container() {
        let be = Arc::new(FakeBackend::new());
        let pool = Pool::new(be.clone(), cfg_leases(1, 6, 600));
        pool.refill().await;

        // First keep-alive call: the warm container becomes a lease and its
        // id is echoed back. Nothing destroyed yet.
        let mut r = req();
        r.keep_alive = true;
        let resp1 = pool.run(&r).await.unwrap();
        let id = resp1.container_id.clone().expect("lease id echoed");
        assert_eq!(pool.leased_len(), 1);
        settle_ready(&pool, 1).await; // pool refilled after consuming the warm one
        assert!(
            be.destroyed.lock().unwrap().is_empty(),
            "a leased container must not be torn down"
        );

        // Second call naming that lease reuses the SAME container (a second
        // exec into it) — no new job container is created for the exec.
        let mut r2 = req();
        r2.keep_alive = true;
        r2.container_id = Some(id.clone());
        let resp2 = pool.run(&r2).await.unwrap();
        assert_eq!(resp2.container_id.as_deref(), Some(id.as_str()));
        assert!(resp2.stdout.contains(&id), "ran in the leased container");
        assert_eq!(pool.leased_len(), 1);
        assert!(be.destroyed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn keep_alive_false_is_still_single_use() {
        let be = Arc::new(FakeBackend::new());
        let pool = Pool::new(be.clone(), cfg_leases(1, 6, 600));
        pool.refill().await;
        let resp = pool.run(&req()).await.unwrap();
        assert_eq!(resp.container_id, None, "no lease id for a single-use job");
        assert_eq!(pool.leased_len(), 0);
        settle_destroyed(&be, 1).await;
    }

    #[tokio::test]
    async fn release_container_destroys_and_untracks_and_is_idempotent() {
        let be = Arc::new(FakeBackend::new());
        let pool = Pool::new(be.clone(), cfg_leases(0, 6, 600));
        let mut r = req();
        r.keep_alive = true;
        let id = pool.run(&r).await.unwrap().container_id.unwrap();
        assert_eq!(pool.leased_len(), 1);

        pool.release_container(&id).await;
        assert_eq!(pool.leased_len(), 0);
        assert!(be.destroyed.lock().unwrap().contains(&id));

        // A second release (e.g. sweeper raced the gateway) must not panic.
        pool.release_container(&id).await;
        assert_eq!(pool.leased_len(), 0);
    }

    #[tokio::test]
    async fn lease_cap_falls_back_to_single_use() {
        let be = Arc::new(FakeBackend::new());
        // Cap at 1 lease.
        let pool = Pool::new(be.clone(), cfg_leases(0, 1, 600));

        let mut r = req();
        r.keep_alive = true;
        let first = pool.run(&r).await.unwrap();
        assert!(first.container_id.is_some(), "first lease granted");
        assert_eq!(pool.leased_len(), 1);

        // Second keep-alive request: cap is full, so the job runs single-use
        // and returns NO lease id — the caller gracefully loses persistence
        // rather than the runner over-committing.
        let second = pool.run(&r).await.unwrap();
        assert_eq!(second.container_id, None, "over-cap request not leased");
        assert_eq!(pool.leased_len(), 1);
        settle_destroyed(&be, 1).await;
    }

    #[tokio::test]
    async fn sweeper_reaps_idle_lease_but_skips_busy() {
        let be = Arc::new(FakeBackend::new());
        // TTL 0 → any idle lease is immediately expired once a moment passes.
        let pool = Pool::new(be.clone(), cfg_leases(0, 6, 0));

        // A real idle lease.
        let mut r = req();
        r.keep_alive = true;
        let idle = pool.run(&r).await.unwrap().container_id.unwrap();

        // A synthetic busy lease the sweeper must leave alone.
        pool.leased.lock().unwrap().insert(
            "busy-1".to_string(),
            Leased {
                last_used: Instant::now(),
                busy: true,
            },
        );

        // Let a moment pass so the idle lease's age exceeds the 0s TTL.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        pool.sweep_expired().await;

        assert!(
            be.destroyed.lock().unwrap().contains(&idle),
            "idle lease reaped"
        );
        assert!(
            pool.leased.lock().unwrap().contains_key("busy-1"),
            "busy lease must survive the sweep"
        );
        assert!(!pool.leased.lock().unwrap().contains_key(&idle));
    }

    /// Backend whose `exec` always errors, to prove a failed job doesn't
    /// strand a lease. `create`/`destroy` are recorded so we can assert the
    /// single-use teardown happened.
    struct FailExecBackend {
        created: Mutex<usize>,
        destroyed: Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl ContainerBackend for FailExecBackend {
        async fn create(&self, _network: Network) -> Result<String, crate::backend::BackendError> {
            let mut n = self.created.lock().unwrap();
            *n += 1;
            Ok(format!("fail-{n}"))
        }
        async fn exec(
            &self,
            _id: &str,
            _req: &RunRequest,
            _timeout: std::time::Duration,
        ) -> Result<RunResponse, crate::backend::BackendError> {
            Err(crate::backend::BackendError::Protocol("boom".into()))
        }
        async fn destroy(&self, id: &str) {
            self.destroyed.lock().unwrap().push(id.to_string());
        }
    }

    #[tokio::test]
    async fn failed_exec_does_not_strand_a_lease() {
        let be = Arc::new(FailExecBackend {
            created: Mutex::new(0),
            destroyed: Mutex::new(Vec::new()),
        });
        let pool = Pool::new(be.clone(), cfg_leases(0, 6, 600));
        let mut r = req();
        r.keep_alive = true; // wants a lease…
        let err = pool.run(&r).await;
        assert!(err.is_err(), "exec error propagates");
        // …but a job that never ran must not become a lease the gateway can't
        // address; the fresh container is torn down single-use instead.
        assert_eq!(pool.leased_len(), 0, "no stranded lease on exec failure");
        settle_destroyed_generic(&be.destroyed, 1).await;
    }

    /// Like `settle_destroyed` but for a plain `Mutex<Vec<String>>`.
    async fn settle_destroyed_generic(destroyed: &Mutex<Vec<String>>, want: usize) {
        for _ in 0..2000 {
            if destroyed.lock().unwrap().len() >= want {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("never reached {want} destroyed");
    }

    #[tokio::test]
    async fn reused_exec_error_drops_the_poisoned_lease() {
        // A reused container whose exec fails must have its lease dropped and
        // the container destroyed, so the gateway's next call recreates a clean
        // one instead of re-exec'ing into a dead container all turn.
        let be = Arc::new(FailExecBackend {
            created: Mutex::new(0),
            destroyed: Mutex::new(Vec::new()),
        });
        let pool = Pool::new(be.clone(), cfg_leases(0, 6, 600));
        // Pre-register a live lease, then drive a reuse call at it.
        pool.leased.lock().unwrap().insert(
            "live-1".to_string(),
            Leased {
                last_used: Instant::now(),
                busy: false,
            },
        );
        let mut r = req();
        r.keep_alive = true;
        r.container_id = Some("live-1".to_string());
        assert!(pool.run(&r).await.is_err(), "exec error propagates");
        assert_eq!(pool.leased_len(), 0, "poisoned lease dropped");
        settle_destroyed_generic(&be.destroyed, 1).await;
        assert!(be.destroyed.lock().unwrap().contains(&"live-1".to_string()));
    }

    /// Backend whose exec always reports a wall-clock timeout (Ok, timed_out).
    struct TimeoutBackend {
        created: Mutex<usize>,
        destroyed: Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl ContainerBackend for TimeoutBackend {
        async fn create(&self, _network: Network) -> Result<String, crate::backend::BackendError> {
            let mut n = self.created.lock().unwrap();
            *n += 1;
            Ok(format!("to-{n}"))
        }
        async fn exec(
            &self,
            _id: &str,
            _req: &RunRequest,
            _timeout: std::time::Duration,
        ) -> Result<RunResponse, crate::backend::BackendError> {
            Ok(RunResponse {
                exit_code: -1,
                stdout: String::new(),
                stderr: "timed out".into(),
                artifacts: vec![],
                duration_ms: 1,
                timed_out: true,
                output_truncated: false,
                container_id: None,
            })
        }
        async fn destroy(&self, id: &str) {
            self.destroyed.lock().unwrap().push(id.to_string());
        }
    }

    #[tokio::test]
    async fn timed_out_keepalive_job_is_not_leased() {
        // A timeout returns Ok(timed_out:true), but the runaway process is
        // still alive in the container, so it must be torn down single-use, not
        // kept as a lease (which would poison the turn's next call).
        let be = Arc::new(TimeoutBackend {
            created: Mutex::new(0),
            destroyed: Mutex::new(Vec::new()),
        });
        let pool = Pool::new(be.clone(), cfg_leases(0, 6, 600));
        let mut r = req();
        r.keep_alive = true;
        let resp = pool.run(&r).await.unwrap();
        assert!(resp.timed_out);
        assert_eq!(resp.container_id, None, "timed-out job not leased");
        assert_eq!(pool.leased_len(), 0);
        settle_destroyed_generic(&be.destroyed, 1).await;
    }

    #[tokio::test]
    async fn stale_lease_id_falls_back_to_a_fresh_container() {
        // The gateway sends an id the runner no longer tracks (swept mid-turn):
        // the job must still run — in a fresh container — and get a new lease
        // id back, not error.
        let be = Arc::new(FakeBackend::new());
        let pool = Pool::new(be.clone(), cfg_leases(0, 6, 600));
        let mut r = req();
        r.keep_alive = true;
        r.container_id = Some("gone-forever".to_string());
        let resp = pool.run(&r).await.unwrap();
        let id = resp.container_id.expect("a fresh lease id");
        assert_ne!(id, "gone-forever");
        assert_eq!(pool.leased_len(), 1);
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
            container_id: None,
        };
        clamp_output(&mut resp, 200);
        assert!(resp.output_truncated);
        assert!(resp.stdout.len() < 1000);
        assert!(resp.stdout.contains("omitted"));
        assert!(resp.stdout.starts_with('A'), "head kept");
        assert!(resp.stdout.ends_with('Z'), "tail kept");
    }
}
