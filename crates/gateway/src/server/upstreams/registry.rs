// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Runtime routing.
//!
//! Each backend tracks the set of models it currently advertises (populated
//! by the health probe in `health.rs`, which parses the OpenAI-shape
//! `/models` response on every successful probe). A request comes in with a
//! `model` string + a `PoolKind`; we walk pools matching the kind, pick the
//! first one that has at least one healthy backend advertising the model,
//! and acquire an inflight slot on a matching backend via the pool's
//! picker strategy.
//!
//! No static `model_routes` table — the gateway derives routes primarily
//! from what each upstream reports. A backend whose `/models` probe returns
//! nothing (no such endpoint, `401`, unparseable body) falls back to its
//! configured model ids (backend `models`, else the pool's) so it stays
//! routable; the live probe wins whenever it reports anything. If two pools
//! of the same kind both advertise the same model name, the first one in
//! config-order wins (`HashMap` iteration is unordered, so for deterministic
//! priority callers should keep one pool per kind in practice).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use std::sync::RwLock;
use thiserror::Error;

use super::config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig};

/// A single upstream backend with the runtime state we need to schedule it.
pub struct Backend {
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub weight: u32,
    pub max_inflight: u32,
    pub health_path: String,
    inflight: AtomicU32,
    healthy: AtomicBool,
    /// The set of model IDs this backend currently advertises, as reported
    /// by its most recent successful `/models` probe. Empty until the first
    /// probe completes (`health::spawn` does an initial blocking round so
    /// the first request finds something). Updated by the probe loop
    /// whenever the upstream's loadout changes.
    models: RwLock<HashSet<String>>,
    /// Static fallback model IDs from config (backend `models`, else the
    /// pool's `models`). Used only while `models` (the live probe set) is
    /// empty — see [`Backend::with_effective_models`] for the precedence.
    /// Lets a backend without a working `/models` endpoint (e.g. Voxtral
    /// realtime) still be routable and advertised.
    config_models: HashSet<String>,
}

impl Backend {
    /// `pool_models` is the pool-level fallback, applied when this backend
    /// declares no `models` of its own (backend config wins over pool).
    fn new(cfg: &BackendConfig, pool_models: &[String]) -> Self {
        let fallback = if cfg.models.is_empty() {
            pool_models
        } else {
            &cfg.models
        };
        let config_models: HashSet<String> =
            fallback.iter().filter(|s| !s.is_empty()).cloned().collect();
        Self {
            name: cfg.name.clone(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key(),
            weight: cfg.weight.max(1),
            max_inflight: cfg.max_inflight.max(1),
            health_path: cfg.health_path.clone(),
            inflight: AtomicU32::new(0),
            healthy: AtomicBool::new(true),
            models: RwLock::new(HashSet::new()),
            config_models,
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn set_healthy(&self, h: bool) {
        self.healthy.store(h, Ordering::Relaxed);
    }

    pub fn inflight(&self) -> u32 {
        self.inflight.load(Ordering::Relaxed)
    }

    /// Runs `f` against this backend's *effective* model set — the live probe
    /// set while it reports anything, otherwise the configured fallback
    /// (`config_models`). The single place the probe-then-config precedence
    /// lives; the read lock is held only for the duration of `f`.
    fn with_effective_models<R>(&self, f: impl FnOnce(&HashSet<String>) -> R) -> R {
        if let Ok(probe) = self.models.read()
            && !probe.is_empty()
        {
            return f(&probe);
        }
        f(&self.config_models)
    }

    /// Returns true if this backend currently serves `model` — i.e. its
    /// most recent probe reported it, or (while the probe set is empty) it's
    /// a configured fallback id. Cheap read-lock, called on the request hot
    /// path. Health is *not* considered here; callers gate on `is_healthy`.
    pub fn serves_model(&self, model: &str) -> bool {
        self.with_effective_models(|set| set.contains(model))
    }

    /// Replace the advertised-model set wholesale. Probe-only path —
    /// called from `health.rs` after a successful `/models` parse so the
    /// next routing lookup reflects the upstream's current loadout.
    pub fn set_models(&self, models: HashSet<String>) {
        if let Ok(mut guard) = self.models.write() {
            *guard = models;
        }
    }

    /// Effective advertised-model set: the live probe set if it reported
    /// anything, otherwise the configured fallback. Allocates; intended for
    /// listing/UI paths (`/v1/models`, the transcription dropdown), not the
    /// request hot path (which uses `serves_model`).
    pub fn models_snapshot(&self) -> HashSet<String> {
        self.with_effective_models(|set| set.clone())
    }

    /// Raw probe-reported set only (no config fallback). For `health.rs`'s
    /// change-detection so the "advertised models updated" diff reflects
    /// what the upstream actually reported, not the static fallback.
    pub fn probe_models(&self) -> HashSet<String> {
        self.models.read().map(|g| g.clone()).unwrap_or_default()
    }
}

/// A pool of backends sharing a strategy and a kind.
pub struct Pool {
    pub name: String,
    pub kind: PoolKind,
    pub strategy: PickerStrategy,
    pub backends: Vec<Arc<Backend>>,
    /// Cursor for round-robin.
    rr_cursor: AtomicUsize,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AcquireError {
    #[error("no healthy backend in pool `{pool}`")]
    NoHealthyBackend { pool: String },
    #[error("all backends in pool `{pool}` are at max inflight")]
    Saturated { pool: String },
}

impl Pool {
    fn new(name: String, cfg: &UpstreamPoolConfig) -> Self {
        let backends = cfg
            .backend
            .iter()
            .map(|b| Arc::new(Backend::new(b, &cfg.models)))
            .collect();
        Self {
            name,
            kind: cfg.kind,
            strategy: cfg.strategy,
            backends,
            rr_cursor: AtomicUsize::new(0),
        }
    }

    /// True if at least one healthy backend in the pool advertises `model`.
    /// Used by `UpstreamRegistry::acquire_for` to pick the right pool.
    pub fn serves_model(&self, model: &str) -> bool {
        self.backends
            .iter()
            .any(|b| b.is_healthy() && b.serves_model(model))
    }

    /// True if *any* backend in the pool serves `model`, regardless of
    /// health. Lets `acquire_for` tell "this model exists here but every
    /// replica is down" (→ 503) from "no backend serves it at all" (→ 404).
    pub fn knows_model(&self, model: &str) -> bool {
        self.backends.iter().any(|b| b.serves_model(model))
    }

    /// Picks a healthy backend that advertises `model`, atomically claims an
    /// inflight slot, and returns an `Acquired` guard. Drop releases the
    /// slot. The pool's `strategy` orders the candidate list; saturation
    /// falls through to the next candidate.
    pub fn acquire_for_model(&self, model: &str) -> Result<Acquired, AcquireError> {
        let candidates: Vec<&Arc<Backend>> = self
            .backends
            .iter()
            .filter(|b| b.is_healthy() && b.serves_model(model))
            .collect();
        if candidates.is_empty() {
            return Err(AcquireError::NoHealthyBackend {
                pool: self.name.clone(),
            });
        }

        let ordered = match self.strategy {
            PickerStrategy::RoundRobin => self.pick_round_robin(&candidates),
            PickerStrategy::LeastInflight => self.pick_least_inflight(&candidates),
        };

        for backend in ordered {
            if try_acquire_slot(backend) {
                return Ok(Acquired {
                    backend: Arc::clone(backend),
                });
            }
        }
        Err(AcquireError::Saturated {
            pool: self.name.clone(),
        })
    }

    fn pick_round_robin<'a>(&self, healthy: &[&'a Arc<Backend>]) -> Vec<&'a Arc<Backend>> {
        let start = self.rr_cursor.fetch_add(1, Ordering::Relaxed) % healthy.len();
        let mut out = Vec::with_capacity(healthy.len());
        for i in 0..healthy.len() {
            out.push(healthy[(start + i) % healthy.len()]);
        }
        out
    }

    fn pick_least_inflight<'a>(&self, healthy: &[&'a Arc<Backend>]) -> Vec<&'a Arc<Backend>> {
        // Sort ascending by inflight so we try the least-loaded first, falling
        // through to busier ones if it's saturated.
        let mut sorted: Vec<&'a Arc<Backend>> = healthy.to_vec();
        sorted.sort_by_key(|b| b.inflight());
        sorted
    }
}

fn try_acquire_slot(backend: &Backend) -> bool {
    let max = backend.max_inflight;
    let mut current = backend.inflight.load(Ordering::Relaxed);
    loop {
        if current >= max {
            return false;
        }
        match backend.inflight.compare_exchange(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

/// RAII guard: while held, the backend has one slot reserved for this caller.
/// Dropping releases it. Cheap to clone — we move it through the proxy
/// pipeline so the slot is held for the full streaming response.
pub struct Acquired {
    backend: Arc<Backend>,
}

impl std::fmt::Debug for Acquired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Acquired({})", self.backend.name)
    }
}

impl Acquired {
    pub fn backend(&self) -> &Backend {
        &self.backend
    }
}

impl Drop for Acquired {
    fn drop(&mut self) {
        self.backend.inflight.fetch_sub(1, Ordering::Release);
    }
}

/// Top-level pool registry. Routes are computed on demand from each
/// backend's advertised-model set; no compiled route table.
pub struct UpstreamRegistry {
    pools: HashMap<String, Arc<Pool>>,
}

impl std::fmt::Debug for UpstreamRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamRegistry")
            .field("pools", &self.pools.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BuildError {
    #[error("duplicate pool name `{0}`")]
    DuplicatePool(String),
}

impl UpstreamRegistry {
    pub fn new(
        pool_configs: &HashMap<String, UpstreamPoolConfig>,
    ) -> Result<Arc<Self>, BuildError> {
        let mut pools: HashMap<String, Arc<Pool>> = HashMap::new();
        for (name, cfg) in pool_configs {
            if pools.contains_key(name) {
                return Err(BuildError::DuplicatePool(name.clone()));
            }
            pools.insert(name.clone(), Arc::new(Pool::new(name.clone(), cfg)));
        }
        Ok(Arc::new(Self { pools }))
    }

    pub fn pools(&self) -> impl Iterator<Item = &Arc<Pool>> {
        self.pools.values()
    }

    /// Sorted, de-duplicated union of the effective model sets of every
    /// backend in the pools matching `pred`. Shared by `models_for_kind` and
    /// `all_models`.
    fn collect_models(&self, pred: impl Fn(&Pool) -> bool) -> Vec<String> {
        let mut all: HashSet<String> = HashSet::new();
        for pool in self.pools.values().filter(|p| pred(p)) {
            for backend in &pool.backends {
                all.extend(backend.models_snapshot());
            }
        }
        let mut out: Vec<String> = all.into_iter().collect();
        out.sort();
        out
    }

    /// Union of every advertised model name across all pools of the given
    /// kind. Used by the chat UI to populate the voice-model dropdown and
    /// by `/api/v0/transcription_models`.
    pub fn models_for_kind(&self, kind: PoolKind) -> Vec<String> {
        self.collect_models(|p| p.kind == kind)
    }

    /// Every advertised model across *all* pools and kinds, de-duplicated by
    /// id (replicas serving the same id collapse to one) and sorted. Backs
    /// the OpenAI-parity `GET /v1/models`, which lists every usable model
    /// regardless of capability — clients pick by id.
    pub fn all_models(&self) -> Vec<String> {
        self.collect_models(|_| true)
    }

    /// True if any pool of `kind` knows `model` (probe- or config-derived),
    /// regardless of backend health. Used to decide 404 (`model_not_found`)
    /// vs 503 before routing — see `acquire_for`.
    pub fn knows_model(&self, model: &str, kind: PoolKind) -> bool {
        self.pools
            .values()
            .any(|p| p.kind == kind && p.knows_model(model))
    }

    /// True if any pool of *any* kind knows `model`. Backs `GET
    /// /v1/models/{id}`, which (like the list) is capability-agnostic.
    pub fn knows_any(&self, model: &str) -> bool {
        self.pools.values().any(|p| p.knows_model(model))
    }

    /// Find a pool of the given kind whose backends advertise `model` and
    /// acquire a slot on one of those backends. If two pools of the same
    /// kind both advertise the model, the first one we iterate wins —
    /// `HashMap` iteration is unordered, so callers shouldn't depend on
    /// which one (real-world deployments keep one pool per kind).
    ///
    /// Error semantics distinguish two cases the OpenAI contract treats
    /// differently:
    ///   - no pool of this kind knows `model` at all → [`RouteError::Unknown
    ///     Model`] (the caller maps this to `404 model_not_found`);
    ///   - the model *is* known but no healthy backend can serve it right
    ///     now → [`AcquireError::NoHealthyBackend`] / `Saturated` (`503`).
    pub fn acquire_for(&self, model: &str, kind: PoolKind) -> Result<Acquired, RouteError> {
        // First, a pool with a healthy backend that serves the model.
        if let Some(pool) = self
            .pools
            .values()
            .find(|p| p.kind == kind && p.serves_model(model))
        {
            return pool.acquire_for_model(model).map_err(RouteError::Acquire);
        }
        // No healthy serving backend. If the model is nonetheless known to a
        // pool of this kind, it's a transient outage (all replicas down) —
        // surface 503, not 404.
        if let Some(pool) = self
            .pools
            .values()
            .find(|p| p.kind == kind && p.knows_model(model))
        {
            return Err(RouteError::Acquire(AcquireError::NoHealthyBackend {
                pool: pool.name.clone(),
            }));
        }
        Err(RouteError::UnknownModel(model.to_string()))
    }
}

#[derive(Debug, Error)]
pub enum RouteError {
    #[error(
        "no upstream advertises model `{0}` — check that the model is loaded on a backend of the right kind"
    )]
    UnknownModel(String),
    #[error(transparent)]
    Acquire(AcquireError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::upstreams::config::{
        BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig,
    };

    fn backend(name: &str, max_inflight: u32) -> BackendConfig {
        BackendConfig {
            name: name.into(),
            base_url: format!("http://{name}:8000/v1"),
            api_key_env: None,
            weight: 1,
            max_inflight,
            health_path: "/models".into(),
            models: Vec::new(),
        }
    }

    /// Backend with a static fallback model list (no probe needed to route).
    fn backend_with_models(name: &str, models: &[&str]) -> BackendConfig {
        BackendConfig {
            models: models.iter().map(|s| (*s).to_string()).collect(),
            ..backend(name, 16)
        }
    }

    fn pool_config(
        kind: PoolKind,
        strategy: PickerStrategy,
        backends: Vec<BackendConfig>,
    ) -> UpstreamPoolConfig {
        UpstreamPoolConfig {
            kind,
            strategy,
            models: Vec::new(),
            backend: backends,
        }
    }

    /// Pool with a pool-level fallback model list.
    fn pool_config_with_models(
        kind: PoolKind,
        models: &[&str],
        backends: Vec<BackendConfig>,
    ) -> UpstreamPoolConfig {
        UpstreamPoolConfig {
            kind,
            strategy: PickerStrategy::RoundRobin,
            models: models.iter().map(|s| (*s).to_string()).collect(),
            backend: backends,
        }
    }

    fn build(pools: Vec<(&str, UpstreamPoolConfig)>) -> Arc<UpstreamRegistry> {
        let map: HashMap<String, UpstreamPoolConfig> =
            pools.into_iter().map(|(k, v)| (k.into(), v)).collect();
        UpstreamRegistry::new(&map).unwrap()
    }

    /// Test helper — synthesise what a `/models` probe would have written
    /// for a single backend. Real code calls `Backend::set_models` from
    /// the health probe; tests use this to bypass the network entirely.
    fn seed_models(reg: &UpstreamRegistry, pool: &str, backend_idx: usize, models: &[&str]) {
        let pool = reg.pools.get(pool).expect("pool exists");
        let set: HashSet<String> = models.iter().map(|s| (*s).to_string()).collect();
        pool.backends[backend_idx].set_models(set);
    }

    #[test]
    fn acquire_for_routes_by_advertised_model() {
        let reg = build(vec![
            (
                "chat",
                pool_config(
                    PoolKind::Chat,
                    PickerStrategy::RoundRobin,
                    vec![backend("a", 16)],
                ),
            ),
            (
                "voice",
                pool_config(
                    PoolKind::Transcription,
                    PickerStrategy::RoundRobin,
                    vec![backend("b", 16)],
                ),
            ),
        ]);
        seed_models(&reg, "chat", 0, &["llama-3.1-70b", "llama-3.1-8b"]);
        seed_models(&reg, "voice", 0, &["whisper-1"]);

        let g = reg.acquire_for("llama-3.1-70b", PoolKind::Chat).unwrap();
        assert_eq!(g.backend().name, "a");
        let g = reg
            .acquire_for("whisper-1", PoolKind::Transcription)
            .unwrap();
        assert_eq!(g.backend().name, "b");
    }

    #[test]
    fn acquire_for_unknown_model_returns_route_error() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend("a", 16)],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["llama-3.1-70b"]);
        let err = reg.acquire_for("gpt-4o", PoolKind::Chat).unwrap_err();
        assert!(matches!(err, RouteError::UnknownModel(_)), "{err:?}");
    }

    #[test]
    fn acquire_for_wrong_kind_is_unknown_model() {
        // Voice pool advertises whisper-1; asking for it under Chat
        // doesn't surface a "wrong kind" error — it just doesn't match a
        // chat-kind pool, so the caller sees UnknownModel. Same UX as if
        // the model wasn't loaded anywhere.
        let reg = build(vec![(
            "voice",
            pool_config(
                PoolKind::Transcription,
                PickerStrategy::RoundRobin,
                vec![backend("a", 16)],
            ),
        )]);
        seed_models(&reg, "voice", 0, &["whisper-1"]);
        let err = reg.acquire_for("whisper-1", PoolKind::Chat).unwrap_err();
        assert!(matches!(err, RouteError::UnknownModel(_)), "{err:?}");
    }

    #[test]
    fn picks_backend_that_serves_the_model_when_pool_is_heterogeneous() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend("a", 16), backend("b", 16)],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["llama-3.1-70b"]);
        seed_models(&reg, "chat", 1, &["llama-3.1-8b"]);

        // 70b lives on backend `a` only — picker shouldn't land on `b`.
        for _ in 0..4 {
            let g = reg.acquire_for("llama-3.1-70b", PoolKind::Chat).unwrap();
            assert_eq!(g.backend().name, "a");
        }
        // …and vice versa.
        for _ in 0..4 {
            let g = reg.acquire_for("llama-3.1-8b", PoolKind::Chat).unwrap();
            assert_eq!(g.backend().name, "b");
        }
    }

    #[test]
    fn models_for_kind_unions_across_pool_backends() {
        let reg = build(vec![
            (
                "chat",
                pool_config(
                    PoolKind::Chat,
                    PickerStrategy::RoundRobin,
                    vec![backend("a", 16), backend("b", 16)],
                ),
            ),
            (
                "voice",
                pool_config(
                    PoolKind::Transcription,
                    PickerStrategy::RoundRobin,
                    vec![backend("c", 16)],
                ),
            ),
        ]);
        seed_models(&reg, "chat", 0, &["llama-3.1-70b"]);
        seed_models(&reg, "chat", 1, &["llama-3.1-8b"]);
        seed_models(&reg, "voice", 0, &["whisper-1"]);

        let mut chat = reg.models_for_kind(PoolKind::Chat);
        chat.sort();
        assert_eq!(chat, vec!["llama-3.1-70b", "llama-3.1-8b"]);
        assert_eq!(
            reg.models_for_kind(PoolKind::Transcription),
            vec!["whisper-1"]
        );
        assert!(reg.models_for_kind(PoolKind::Embedding).is_empty());
    }

    #[test]
    fn round_robin_cycles_among_matching_backends() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend("a", 16), backend("b", 16), backend("c", 16)],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["m"]);
        seed_models(&reg, "chat", 1, &["m"]);
        seed_models(&reg, "chat", 2, &["m"]);
        let mut picks = Vec::new();
        for _ in 0..6 {
            let g = reg.acquire_for("m", PoolKind::Chat).unwrap();
            picks.push(g.backend().name.clone());
        }
        for n in ["a", "b", "c"] {
            assert!(picks.contains(&n.to_string()), "no pick of {n}: {picks:?}");
        }
    }

    #[test]
    fn skips_unhealthy_backends_in_route_lookup() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend("a", 16), backend("b", 16)],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["m"]);
        seed_models(&reg, "chat", 1, &["m"]);
        // Mark `a` unhealthy — every acquire should land on `b`.
        reg.pools.get("chat").unwrap().backends[0].set_healthy(false);
        for _ in 0..5 {
            let g = reg.acquire_for("m", PoolKind::Chat).unwrap();
            assert_eq!(g.backend().name, "b");
        }
    }

    #[test]
    fn least_inflight_prefers_idle_backend() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::LeastInflight,
                vec![backend("a", 16), backend("b", 16)],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["m"]);
        seed_models(&reg, "chat", 1, &["m"]);
        let pool = reg.pools.get("chat").unwrap();
        // Hold one slot via Pool API directly — exercising the inflight counter.
        let _a1 = pool.acquire_for_model("m").unwrap();
        // Force a's inflight up so the picker prefers b.
        pool.backends[0].inflight.store(5, Ordering::Relaxed);
        let g = reg.acquire_for("m", PoolKind::Chat).unwrap();
        assert_eq!(g.backend().name, "b");
    }

    #[test]
    fn saturated_when_all_matching_backends_at_max() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::LeastInflight,
                vec![backend("a", 1), backend("b", 1)],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["m"]);
        seed_models(&reg, "chat", 1, &["m"]);
        let _g1 = reg.acquire_for("m", PoolKind::Chat).unwrap();
        let _g2 = reg.acquire_for("m", PoolKind::Chat).unwrap();
        let err = reg.acquire_for("m", PoolKind::Chat).unwrap_err();
        assert!(
            matches!(
                err,
                RouteError::Acquire(AcquireError::Saturated { ref pool }) if pool == "chat"
            ),
            "{err:?}"
        );
    }

    #[test]
    fn empty_model_set_means_no_route() {
        // First-request-before-first-probe scenario. `health::spawn` blocks
        // on the initial probe in production so this only happens if the
        // upstream is unreachable at boot — in which case UnknownModel is
        // the right surface error (the user wouldn't even know what model
        // to ask for yet).
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend("a", 16)],
            ),
        )]);
        let err = reg.acquire_for("anything", PoolKind::Chat).unwrap_err();
        assert!(matches!(err, RouteError::UnknownModel(_)), "{err:?}");
    }

    #[test]
    fn config_models_route_without_a_probe() {
        // A transcription backend with no working `/models` endpoint: the
        // probe set stays empty, but the pool-level `models` fallback makes
        // it routable and listable anyway.
        let reg = build(vec![(
            "voice",
            pool_config_with_models(
                PoolKind::Transcription,
                &["voxtral-realtime"],
                vec![backend("a", 16)],
            ),
        )]);
        // No seed_models — the probe never reported anything.
        let g = reg
            .acquire_for("voxtral-realtime", PoolKind::Transcription)
            .unwrap();
        assert_eq!(g.backend().name, "a");
        assert_eq!(reg.all_models(), vec!["voxtral-realtime"]);
        assert!(reg.knows_model("voxtral-realtime", PoolKind::Transcription));
    }

    #[test]
    fn backend_config_models_win_over_pool_config_models() {
        let reg = build(vec![(
            "voice",
            pool_config_with_models(
                PoolKind::Transcription,
                &["pool-model"],
                vec![backend_with_models("a", &["backend-model"])],
            ),
        )]);
        // Backend declared its own models, so the pool fallback is ignored
        // for that backend.
        assert!(
            reg.acquire_for("backend-model", PoolKind::Transcription)
                .is_ok()
        );
        let err = reg
            .acquire_for("pool-model", PoolKind::Transcription)
            .unwrap_err();
        assert!(matches!(err, RouteError::UnknownModel(_)), "{err:?}");
        assert_eq!(reg.all_models(), vec!["backend-model"]);
    }

    #[test]
    fn live_probe_overrides_config_models() {
        // While the probe reports anything, config fallback is not consulted
        // — the endpoint is authoritative for its own loadout.
        let reg = build(vec![(
            "voice",
            pool_config_with_models(
                PoolKind::Transcription,
                &["config-only"],
                vec![backend("a", 16)],
            ),
        )]);
        seed_models(&reg, "voice", 0, &["probe-reported"]);
        assert!(
            reg.acquire_for("probe-reported", PoolKind::Transcription)
                .is_ok()
        );
        let err = reg
            .acquire_for("config-only", PoolKind::Transcription)
            .unwrap_err();
        assert!(matches!(err, RouteError::UnknownModel(_)), "{err:?}");
        assert_eq!(reg.all_models(), vec!["probe-reported"]);
    }

    #[test]
    fn all_models_dedups_across_replicas_and_unions_across_kinds() {
        let reg = build(vec![
            (
                "chat",
                pool_config(
                    PoolKind::Chat,
                    PickerStrategy::RoundRobin,
                    vec![backend("a", 16), backend("b", 16)],
                ),
            ),
            (
                "voice",
                pool_config_with_models(
                    PoolKind::Transcription,
                    &["whisper-1"],
                    vec![backend("c", 16)],
                ),
            ),
        ]);
        // Both chat replicas serve the same id — must collapse to one entry.
        seed_models(&reg, "chat", 0, &["qwen"]);
        seed_models(&reg, "chat", 1, &["qwen"]);
        // Transcription model comes purely from config (no probe).
        assert_eq!(reg.all_models(), vec!["qwen", "whisper-1"]);
    }

    #[test]
    fn known_model_with_all_replicas_unhealthy_is_503_not_404() {
        // Distinguishes "model exists but every replica is down" (transient,
        // 503) from "no backend serves this id" (client error, 404).
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend("a", 16)],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["m"]);
        reg.pools.get("chat").unwrap().backends[0].set_healthy(false);

        // Still "known" (health-agnostic)…
        assert!(reg.knows_model("m", PoolKind::Chat));
        // …so acquire surfaces NoHealthyBackend, not UnknownModel.
        let err = reg.acquire_for("m", PoolKind::Chat).unwrap_err();
        assert!(
            matches!(
                err,
                RouteError::Acquire(AcquireError::NoHealthyBackend { ref pool }) if pool == "chat"
            ),
            "{err:?}"
        );

        // A genuinely unknown id is still UnknownModel.
        let err = reg.acquire_for("nope", PoolKind::Chat).unwrap_err();
        assert!(matches!(err, RouteError::UnknownModel(_)), "{err:?}");
    }

    #[test]
    fn knows_any_spans_all_kinds() {
        let reg = build(vec![(
            "voice",
            pool_config_with_models(
                PoolKind::Transcription,
                &["whisper-1"],
                vec![backend("a", 16)],
            ),
        )]);
        assert!(reg.knows_any("whisper-1"));
        assert!(!reg.knows_any("unknown"));
    }
}
