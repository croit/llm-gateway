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
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use arc_swap::ArcSwap;
use thiserror::Error;

use super::config::{
    BackendConfig, Compliance, FallbackConfig, PickerStrategy, PoolKind, UpstreamPoolConfig,
};

/// A configured alias and its current state, for the read-only admin view.
#[derive(Debug, Clone)]
pub struct AliasStatus {
    /// The client-facing name.
    pub name: String,
    /// Explicit target real id (map form), or `None` for a bare list alias
    /// that binds to the backend's sole model.
    pub target: Option<String>,
    /// True when a bare alias is currently disabled because the backend serves
    /// more than one model (ambiguous) — see [`Backend::reevaluate_aliases`].
    pub disabled: bool,
}

/// A single upstream backend with the runtime state we need to schedule it.
pub struct Backend {
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub weight: u32,
    pub max_inflight: u32,
    pub health_path: String,
    /// Whether the probe may overwrite [`models`](Self::models) from a
    /// `/models` response. `false` pins the model set to `config_models`
    /// (see [`BackendConfig::probe_models`]).
    probe_models: bool,
    /// Whether this backend can edit images (image-to-image), not just
    /// generate. Only meaningful on image pools; see
    /// [`BackendConfig::supports_edit`].
    supports_edit: bool,
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
    /// Client-facing aliases this backend answers to, from config: alias name →
    /// optional explicit target real id. `Some(id)` (map form) pins a specific
    /// model; `None` (bare list form) resolves to the backend's sole model at
    /// request time. Static — aliases never come from the probe.
    aliases: HashMap<String, Option<String>>,
    /// Bare aliases currently disabled because the backend serves ≠1 model, so
    /// "the sole model" is ambiguous. Recomputed by [`Backend::reevaluate_aliases`]
    /// whenever the effective model set changes (probe update or construction);
    /// a disabled alias stops resolving until the ambiguity clears. Map-form
    /// aliases are never disabled (they name their target explicitly).
    disabled_aliases: RwLock<HashSet<String>>,
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
        let aliases = cfg.alias.as_ref().map(|a| a.into_map()).unwrap_or_default();
        let backend = Self {
            name: cfg.name.clone(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key(),
            weight: cfg.weight.max(1),
            max_inflight: cfg.max_inflight.max(1),
            health_path: cfg.health_path.clone(),
            probe_models: cfg.probe_models,
            supports_edit: cfg.supports_edit,
            inflight: AtomicU32::new(0),
            healthy: AtomicBool::new(true),
            models: RwLock::new(HashSet::new()),
            config_models,
            aliases,
            disabled_aliases: RwLock::new(HashSet::new()),
        };
        // Evaluate against the config-model set now, so a bare alias declared
        // alongside multiple static models is disabled (and logged) from the
        // start; the first probe re-evaluates against the live set.
        backend.reevaluate_aliases();
        backend
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

    /// Runs `f` against this backend's *effective* model set — the set the
    /// backend actually serves and advertises. The single place the
    /// probe/config precedence lives; the read lock is held for the duration
    /// of `f`. Precedence:
    ///   - **live probe + a configured `models` list** → the *intersection*:
    ///     the list is an allowlist, so a probed model it doesn't name is
    ///     discovered-but-withheld (not served, not advertised);
    ///   - **live probe + empty list** → the whole probe set (offer everything
    ///     the backend reports);
    ///   - **no live probe** → the configured `models` verbatim (the static
    ///     fallback for backends that don't self-report via `/models`).
    fn with_effective_models<R>(&self, f: impl FnOnce(&HashSet<String>) -> R) -> R {
        if let Ok(probe) = self.models.read()
            && !probe.is_empty()
        {
            if self.config_models.is_empty() {
                return f(&probe);
            }
            let allowed: HashSet<String> =
                probe.intersection(&self.config_models).cloned().collect();
            return f(&allowed);
        }
        f(&self.config_models)
    }

    /// The models the backend reports via `/models` but its allowlist withholds
    /// — i.e. `live probe \ effective`. Empty unless a configured `models` list
    /// is actively filtering a live probe. Drives the struck-through
    /// "discovered but not served" chips in the admin health view; never
    /// consulted on the routing hot path.
    pub fn withheld_models(&self) -> HashSet<String> {
        let Ok(probe) = self.models.read() else {
            return HashSet::new();
        };
        if probe.is_empty() || self.config_models.is_empty() {
            return HashSet::new();
        }
        probe.difference(&self.config_models).cloned().collect()
    }

    /// Real-model membership only (no aliases): the backend's effective set
    /// (live probe, else config fallback) contains `model`. Cheap read-lock.
    fn serves_real(&self, model: &str) -> bool {
        self.with_effective_models(|set| set.contains(model))
    }

    /// The backend's sole effective model, if it serves exactly one. Backs
    /// bare (list-form) alias resolution — an alias with no explicit target
    /// binds to this. `None` when the backend serves zero or several models.
    fn sole_model(&self) -> Option<String> {
        self.with_effective_models(|set| {
            if set.len() == 1 {
                set.iter().next().cloned()
            } else {
                None
            }
        })
    }

    /// True if a bare alias is currently disabled (the backend serves ≠1
    /// model, so "the sole model" is ambiguous — see `reevaluate_aliases`).
    fn alias_disabled(&self, name: &str) -> bool {
        self.disabled_aliases
            .read()
            .map(|g| g.contains(name))
            .unwrap_or(false)
    }

    /// Resolve a requested name to the **real model id** this backend would
    /// forward to the upstream, or `None` if it doesn't serve it. A real id
    /// always wins over an alias of the same spelling (identity). A map-form
    /// alias resolves to its target only while that target is actually served;
    /// a bare alias resolves to the backend's sole model while it isn't
    /// disabled. Health is not considered here; callers gate on `is_healthy`.
    pub fn resolve(&self, requested: &str) -> Option<String> {
        if self.serves_real(requested) {
            return Some(requested.to_string());
        }
        match self.aliases.get(requested) {
            Some(Some(target)) => self.serves_real(target).then(|| target.clone()),
            Some(None) => {
                if self.alias_disabled(requested) {
                    None
                } else {
                    self.sole_model()
                }
            }
            None => None,
        }
    }

    /// Returns true if this backend currently serves `model` — as a real
    /// advertised id, or as a resolvable alias. Health is *not* considered
    /// here; callers gate on `is_healthy`. Cheap on the common real-id path
    /// (no allocation); only an alias hit does the extra lookup.
    pub fn serves_model(&self, model: &str) -> bool {
        if self.serves_real(model) {
            return true;
        }
        match self.aliases.get(model) {
            Some(Some(target)) => self.serves_real(target),
            Some(None) => !self.alias_disabled(model) && self.sole_model().is_some(),
            None => false,
        }
    }

    /// Whether the probe is allowed to discover this backend's model set from
    /// `/models`. `false` pins the set to `config_models` — see
    /// [`BackendConfig::probe_models`].
    pub fn probe_models_enabled(&self) -> bool {
        self.probe_models
    }

    /// Whether this backend advertises image-editing support. Only meaningful
    /// on image pools.
    pub fn supports_edit(&self) -> bool {
        self.supports_edit
    }

    /// Replace the advertised-model set wholesale. Probe-only path —
    /// called from `health.rs` after a successful `/models` parse so the
    /// next routing lookup reflects the upstream's current loadout. Also
    /// re-evaluates bare-alias ambiguity against the new set.
    pub fn set_models(&self, models: HashSet<String>) {
        if let Ok(mut guard) = self.models.write() {
            *guard = models;
        }
        self.reevaluate_aliases();
    }

    /// Effective advertised-model set: the live probe set if it reported
    /// anything, otherwise the configured fallback. Allocates; intended for
    /// listing/UI paths (`/v1/models`, the transcription dropdown), not the
    /// request hot path (which uses `serves_model`).
    pub fn models_snapshot(&self) -> HashSet<String> {
        self.with_effective_models(|set| set.clone())
    }

    /// The *live* probe set only (never the config fallback), empty until the
    /// first successful probe. Used by [`UpstreamRegistry::reload`] to carry a
    /// backend's discovered models across a topology swap so routing doesn't gap.
    pub fn live_models(&self) -> HashSet<String> {
        self.models.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Names a listing surface should advertise: the effective real set plus
    /// every alias that currently resolves (so clients can pick either the
    /// real id or the alias). An alias that can't route right now — disabled,
    /// or a map target that isn't loaded — is omitted so the list never
    /// advertises a name that would 404/503.
    pub fn listed_models(&self) -> HashSet<String> {
        let mut set = self.models_snapshot();
        for name in self.aliases.keys() {
            if self.resolve(name).is_some() {
                set.insert(name.clone());
            }
        }
        set
    }

    /// Recompute which bare (list-form) aliases are ambiguous — the backend
    /// serves more than one model, so "the sole model" is undefined — and
    /// disable them. Called at construction and after every probe update. Logs
    /// only on the transition (disable / re-enable), so a steady state stays
    /// silent. Map-form aliases are never disabled: they name their target.
    fn reevaluate_aliases(&self) {
        // A bare alias needs exactly one model to bind to. Only >1 is the
        // genuinely-ambiguous case worth an ERROR; with 0 models the backend
        // serves nothing, so the alias just doesn't resolve (not "ambiguous").
        let effective_len = self.with_effective_models(|set| set.len());
        let mut now_disabled: HashSet<String> = HashSet::new();
        if effective_len > 1 {
            for (name, target) in &self.aliases {
                if target.is_none() {
                    now_disabled.insert(name.clone());
                }
            }
        }
        let Ok(mut guard) = self.disabled_aliases.write() else {
            return;
        };
        for name in now_disabled.difference(&guard) {
            tracing::error!(
                backend = %self.name,
                alias = %name,
                models = effective_len,
                "bare alias `{name}` is ambiguous — this backend now serves multiple models, \
                 so it can't pick one; disabling it. Give it an explicit target with the map \
                 form, e.g. `alias = {{ \"{name}\" = \"<real-model-id>\" }}`."
            );
        }
        for name in guard.difference(&now_disabled) {
            tracing::info!(
                backend = %self.name,
                alias = %name,
                "bare alias `{name}` is no longer ambiguous — re-enabled"
            );
        }
        *guard = now_disabled;
    }

    /// Raw probe-reported set only (no config fallback). For `health.rs`'s
    /// change-detection so the "advertised models updated" diff reflects
    /// what the upstream actually reported, not the static fallback.
    pub fn probe_models(&self) -> HashSet<String> {
        self.models.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Configured aliases and their current state, sorted by name. For the
    /// read-only `/admin/backends` view.
    pub fn alias_status(&self) -> Vec<AliasStatus> {
        let disabled = self
            .disabled_aliases
            .read()
            .map(|g| g.clone())
            .unwrap_or_default();
        let mut out: Vec<AliasStatus> = self
            .aliases
            .iter()
            .map(|(name, target)| AliasStatus {
                name: name.clone(),
                target: target.clone(),
                disabled: disabled.contains(name),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

/// A pool of backends sharing a strategy and a kind.
pub struct Pool {
    pub name: String,
    pub kind: PoolKind,
    pub strategy: PickerStrategy,
    pub backends: Vec<Arc<Backend>>,
    /// Data-handling attributes for every model this pool serves (default
    /// all-clear). Drives advisory chat-UI warnings; never affects routing.
    pub compliance: Compliance,
    /// Whether calls served by this pool count toward rate limits / quotas
    /// (default `true`; self-hosted pools set `false`). See
    /// [`UpstreamPoolConfig::enforce_limits`] and [`UpstreamRegistry::enforce_limits_for_model`].
    pub enforce_limits: bool,
    /// Language → voice-id map (speech pools only). See
    /// [`UpstreamPoolConfig::voices`] / [`UpstreamPoolConfig::voice_for_language`].
    pub voices: std::collections::HashMap<String, String>,
    /// Voices this pool offers users to choose from, in the operator's order
    /// (speech pools only). See [`UpstreamPoolConfig::offer_voices`] — this is
    /// the menu, `voices` is the resolution.
    pub offer_voices: Vec<String>,
    /// Pool-level configured model ids (the `models` TOML list). Retained so
    /// the speech default-model pick can prefer the operator's declared TTS
    /// model over the `/models` probe — a cloud provider like OpenAI reports
    /// its whole catalogue on `/models`, which would otherwise swamp the pick.
    pub configured_models: Vec<String>,
    /// Backup model when a model this pool *knows* has no healthy backend
    /// (every replica down). `UpstreamRegistry::route` re-resolves the request
    /// to this instead of returning `503`. See [`UpstreamPoolConfig::fallback_offline`].
    fallback_offline: Option<String>,
    /// Gateway-group names allowed to see + route to this pool. Empty =
    /// unrestricted (every user). See [`PoolAccess`] and
    /// [`crate::server::rbac::Resolver::resource_allowed`].
    pub allowed_groups: Vec<String>,
    /// Cursor for round-robin.
    rr_cursor: AtomicUsize,
}

/// A resolved caller's pool-access decision, built once per request from the
/// RBAC resolver and threaded into the group-aware listing/routing methods.
/// Encapsulates the opt-in + admin-bypass rule so the registry needs no
/// dependency on `rbac`.
#[derive(Debug, Clone, Default)]
pub struct PoolAccess {
    /// The caller's resolved gateway-group ids (`Resolver::role_ids_for`).
    pub role_ids: Vec<String>,
    /// True to bypass every restriction (`Resolver::is_admin`).
    pub is_admin: bool,
}

impl PoolAccess {
    /// Full access — used by internal callers that must see the whole topology
    /// (health probes, admin topology views, non-user-facing resolution).
    pub fn all() -> Self {
        Self {
            role_ids: Vec::new(),
            is_admin: true,
        }
    }

    /// Whether the caller may see/route to `pool`: unrestricted pools are open
    /// to all; admins bypass; otherwise the caller must hold a listed group.
    pub fn allows(&self, pool: &Pool) -> bool {
        if self.is_admin || pool.allowed_groups.is_empty() {
            return true;
        }
        pool.allowed_groups
            .iter()
            .any(|g| self.role_ids.iter().any(|r| r == g))
    }
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
            compliance: cfg.compliance,
            enforce_limits: cfg.enforce_limits,
            voices: cfg.voices.clone(),
            offer_voices: cfg.offer_voices.clone(),
            configured_models: cfg.models.clone(),
            fallback_offline: cfg.fallback_offline.clone(),
            allowed_groups: cfg.allowed_groups.clone(),
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

    /// The pool's configured offline backup model, if any. Read-only admin view.
    pub fn fallback_offline(&self) -> Option<&str> {
        self.fallback_offline.as_deref()
    }

    /// The real id the first healthy backend resolves `model` to, if any.
    /// Backs the registry's non-acquiring `resolve_model`.
    fn resolve_healthy(&self, model: &str) -> Option<String> {
        self.backends
            .iter()
            .filter(|b| b.is_healthy())
            .find_map(|b| b.resolve(model))
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
                // The real model id to forward: `model` itself for a real id,
                // or the alias's target on *this* backend. Candidates were
                // filtered by `serves_model`, so `resolve` is normally `Some`;
                // fall back to the requested string on a probe-update race.
                let resolved_model = backend.resolve(model).unwrap_or_else(|| model.to_string());
                return Ok(Acquired {
                    backend: Arc::clone(backend),
                    resolved_model,
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
    /// The real model id the request resolved to on this backend — the id to
    /// write into the forwarded body's `model` field. Equal to the requested
    /// model for a direct hit; the alias's target when routed via an alias.
    resolved_model: String,
}

impl std::fmt::Debug for Acquired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Acquired({}, model={})",
            self.backend.name, self.resolved_model
        )
    }
}

impl Acquired {
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// The real model id to forward upstream (see the field docs). Callers
    /// rewrite the request body's `model` to this and key usage/defaults on it.
    pub fn resolved_model(&self) -> &str {
        &self.resolved_model
    }
}

impl Drop for Acquired {
    fn drop(&mut self) {
        self.backend.inflight.fetch_sub(1, Ordering::Release);
    }
}

/// Top-level pool registry. Routes are computed on demand from each
/// backend's advertised-model set; no compiled route table.
///
/// The pool/fallback data lives behind an [`ArcSwap`] so the admin UI can
/// hot-swap the entire topology via [`Self::reload`] without restarting.
/// Every method call is a single lock-free atomic load.
pub struct UpstreamRegistry {
    inner: ArcSwap<RegistryData>,
    /// Bumped on every [`Self::reload`]. Each health-probe loop is tagged with
    /// the generation it was spawned for and retires itself once this moves past
    /// it, so a reload doesn't leak the previous generation's infinite probe
    /// loops (which hold detached `Backend` Arcs and would keep hammering the old
    /// URLs forever). See `health::spawn` / `health::run_probe`.
    generation: AtomicU64,
}

struct RegistryData {
    pools: HashMap<String, Arc<Pool>>,
    /// Unknown-model fallback per kind (`[fallback]`). Applied by `route` when
    /// a requested name is neither a real id nor an alias.
    fallback: FallbackConfig,
}

impl std::fmt::Debug for UpstreamRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamRegistry")
            .field("pools", &self.data().pools.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BuildError {
    #[error("duplicate pool name `{0}`")]
    DuplicatePool(String),
    #[error(
        "alias `{alias}` on backend `{backend}` in pool `{pool}` collides with a real model id declared in config — an alias must not shadow a model name"
    )]
    AliasCollidesWithModel {
        alias: String,
        pool: String,
        backend: String,
    },
    #[error(
        "alias `{alias}` on backend `{backend}` in pool `{pool}` targets `{target}`, which isn't in that backend's configured `models` {declared:?} — fix the target or add it to `models`"
    )]
    AliasTargetUnknown {
        alias: String,
        target: String,
        pool: String,
        backend: String,
        declared: Vec<String>,
    },
}

/// Construct the internal data from config structs. Shared by the
/// constructors and [`UpstreamRegistry::reload`].
fn build_data(
    pool_configs: &HashMap<String, UpstreamPoolConfig>,
    fallback: FallbackConfig,
) -> Result<RegistryData, BuildError> {
    validate_aliases(pool_configs)?;
    let mut pools: HashMap<String, Arc<Pool>> = HashMap::new();
    for (name, cfg) in pool_configs {
        if pools.contains_key(name) {
            return Err(BuildError::DuplicatePool(name.clone()));
        }
        pools.insert(name.clone(), Arc::new(Pool::new(name.clone(), cfg)));
    }
    Ok(RegistryData { pools, fallback })
}

impl UpstreamRegistry {
    /// Build with no unknown-model fallback. Used by tests and any caller that
    /// doesn't route `[fallback]` (RAG embeddings, etc.).
    pub fn new(
        pool_configs: &HashMap<String, UpstreamPoolConfig>,
    ) -> Result<Arc<Self>, BuildError> {
        Self::build(pool_configs, FallbackConfig::default())
    }

    /// Build with the `[fallback]` map wired in, so `route` can substitute an
    /// unknown requested model with a configured per-kind default.
    pub fn with_fallback(
        pool_configs: &HashMap<String, UpstreamPoolConfig>,
        fallback: FallbackConfig,
    ) -> Result<Arc<Self>, BuildError> {
        Self::build(pool_configs, fallback)
    }

    /// Build from a DB topology snapshot (loaded by
    /// [`crate::server::db::upstreams_config::load_snapshot`]). Converts the
    /// snapshot into the same config structs that TOML parsing produces, then
    /// delegates to [`build`]. Used on startup and on every "Apply changes"
    /// reload from the admin UI.
    pub fn from_snapshot(
        snap: &crate::server::db::upstreams_config::UpstreamConfigSnapshot,
        crypto: &crate::server::crypto::Crypto,
    ) -> Result<Arc<Self>, BuildError> {
        let (pool_configs, fallback) =
            crate::server::upstreams::db_bridge::snapshot_to_configs(snap, crypto);
        Self::build(&pool_configs, fallback)
    }

    fn build(
        pool_configs: &HashMap<String, UpstreamPoolConfig>,
        fallback: FallbackConfig,
    ) -> Result<Arc<Self>, BuildError> {
        let data = build_data(pool_configs, fallback)?;
        Ok(Arc::new(Self {
            inner: ArcSwap::new(Arc::new(data)),
            generation: AtomicU64::new(0),
        }))
    }

    /// The current topology generation — bumped on every [`Self::reload`]. A
    /// health-probe loop reads this to know when it has been superseded.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Hot-swap the entire topology from a DB snapshot. Validates aliases
    /// before swapping; on success the old pools are replaced atomically and the
    /// generation is bumped so stale probe loops retire. Fresh probes must be
    /// re-spawned after reload (see `health::spawn`).
    ///
    /// To avoid a routing gap during the swap, the *live* probed model set of
    /// each unchanged backend (same name + base_url) is carried over onto its
    /// freshly-built replacement. Without this, the new backends start with an
    /// empty live set and every request 404s until the first re-probe completes.
    pub fn reload(
        &self,
        snap: &crate::server::db::upstreams_config::UpstreamConfigSnapshot,
        crypto: &crate::server::crypto::Crypto,
    ) -> Result<(), BuildError> {
        let (pool_configs, fallback) =
            crate::server::upstreams::db_bridge::snapshot_to_configs(snap, crypto);
        let data = build_data(&pool_configs, fallback)?;

        // Snapshot the outgoing live model sets, keyed by identity (name +
        // base_url). Only carry over when both match — an edited base_url points
        // at a possibly-different upstream, so its set must be re-probed.
        // `old` (an `Arc<RegistryData>`) is held for this whole block, so the
        // map can borrow the outgoing backends' names/urls as keys — no clones
        // on either insert or lookup.
        let old = self.data();
        let mut prior: HashMap<(&str, &str), HashSet<String>> = HashMap::new();
        for pool in old.pools.values() {
            for b in &pool.backends {
                let live = b.live_models();
                if !live.is_empty() {
                    prior.insert((b.name.as_str(), b.base_url.as_str()), live);
                }
            }
        }
        for pool in data.pools.values() {
            for b in &pool.backends {
                if let Some(live) = prior.get(&(b.name.as_str(), b.base_url.as_str())) {
                    b.set_models(live.clone());
                }
            }
        }

        self.inner.store(Arc::new(data));
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Load the current data snapshot. Lock-free read + one atomic Arc clone.
    fn data(&self) -> Arc<RegistryData> {
        self.inner.load_full()
    }

    pub fn pools(&self) -> Vec<Arc<Pool>> {
        self.data().pools.values().cloned().collect()
    }

    /// Sorted, de-duplicated union of the effective model sets of every
    /// backend in the pools matching `pred`, **including resolvable aliases**
    /// (so `/v1/models` and the pickers advertise alias names too). Shared by
    /// `models_for_kind` and `all_models`.
    fn collect_models(&self, pred: impl Fn(&Pool) -> bool) -> Vec<String> {
        let d = self.data();
        let mut all: HashSet<String> = HashSet::new();
        for pool in d.pools.values().filter(|p| pred(p)) {
            for backend in &pool.backends {
                all.extend(backend.listed_models());
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

    /// Every listed model of `kind`, each paired with the real id it resolves to
    /// when the name is an **alias** (`None` = a real model that owns its own
    /// settings). Real ids win over an alias of the same spelling. Sorted by
    /// name. Backs the admin pricing/defaults page, which renders aliases
    /// read-only: an alias carries no price or defaults of its own — requests are
    /// configured and metered as the model it resolves to.
    pub fn models_with_alias_target(&self, kind: PoolKind) -> Vec<(String, Option<String>)> {
        let d = self.data();
        let mut real: HashSet<String> = HashSet::new();
        for pool in d.pools.values().filter(|p| p.kind == kind) {
            for backend in &pool.backends {
                real.extend(backend.models_snapshot());
            }
        }
        let mut out: HashMap<String, Option<String>> = HashMap::new();
        for name in &real {
            out.insert(name.clone(), None);
        }
        for pool in d.pools.values().filter(|p| p.kind == kind) {
            for backend in &pool.backends {
                for name in backend.listed_models() {
                    if out.contains_key(&name) {
                        continue; // already a real id, or an alias we've mapped
                    }
                    if let Some(target) = backend.resolve(&name) {
                        out.insert(name, Some(target));
                    }
                }
            }
        }
        let mut rows: Vec<(String, Option<String>)> = out.into_iter().collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    /// True when at least one `speech` pool is configured — the switch that
    /// makes voice mode (and `POST /api/v0/speech`) available in the UI.
    pub fn has_speech(&self) -> bool {
        self.data()
            .pools
            .values()
            .any(|p| p.kind == PoolKind::Speech)
    }

    /// Resolve the voice-mode TTS target for a spoken `language` (lowercase
    /// ISO-639-1): the default speech model (first advertised across speech
    /// pools) plus the voice from the speech pool's language→voice map (exact
    /// match, then the `""` default entry, then `None` = backend default voice).
    /// `None` when no speech pool advertises a model. Used by the session
    /// `POST /api/v0/speech`; the raw `/v1/audio/speech` proxy takes an explicit
    /// model/voice from the caller instead.
    pub fn speech_target(&self, language: &str) -> Option<(String, Option<String>)> {
        let d = self.data();
        let pool = d.pools.values().find(|p| p.kind == PoolKind::Speech)?;
        // Prefer the operator's declared model (`models = ["tts-1"]`): a cloud
        // provider's `/models` probe lists its whole catalogue, so the probe
        // union can't identify *the* TTS model. Fall back to the probe union
        // for self-hosted servers that correctly advertise only their voice.
        let model = pool
            .configured_models
            .first()
            .cloned()
            .or_else(|| self.models_for_kind(PoolKind::Speech).into_iter().next())?;
        let voice = pool
            .voices
            .get(language)
            .or_else(|| pool.voices.get(""))
            .cloned();
        Some((model, voice))
    }

    /// Every distinct voice advertised by the speech pools `access` permits,
    /// sorted. Backs the per-user voice picker and, on the speech path, the
    /// check that a stored preference is still on offer.
    ///
    /// The menu is exactly what the operator declared in the pool's
    /// language→voice map, so a user can neither pick a voice the deployment
    /// doesn't offer nor keep one after the operator removes it — the stored
    /// preference falls back to [`Self::speech_target`]'s voice instead of
    /// reaching the upstream as an unknown id.
    pub fn speech_voices_for(&self, access: &PoolAccess) -> Vec<String> {
        let d = self.data();
        let pools = || {
            d.pools
                .values()
                .filter(|p| p.kind == PoolKind::Speech && access.allows(p))
        };
        // The operator's explicit menu comes first and in their order — the
        // house voice belongs at the top, not wherever the alphabet puts it.
        let mut voices: Vec<String> = Vec::new();
        for p in pools() {
            for v in &p.offer_voices {
                if !voices.contains(v) {
                    voices.push(v.clone());
                }
            }
        }
        // Then whatever the language map resolves to, so a deployment that
        // never fills the menu still offers its configured voices rather than
        // nothing at all. Sorted, since a HashMap has no order to honour.
        let mut resolved: Vec<String> = pools()
            .flat_map(|p| p.voices.values().cloned())
            .filter(|v| !voices.contains(v))
            .collect();
        resolved.sort();
        resolved.dedup();
        voices.extend(resolved);
        voices
    }

    /// Every advertised model across *all* pools and kinds, de-duplicated by
    /// id (replicas serving the same id collapse to one) and sorted. Backs
    /// the OpenAI-parity `GET /v1/models`, which lists every usable model
    /// regardless of capability — clients pick by id.
    pub fn all_models(&self) -> Vec<String> {
        self.collect_models(|p| p.kind != PoolKind::Ocr)
    }

    /// Like [`Self::all_models`], but only over pools `access` permits — the
    /// per-user `GET /v1/models`. A model withheld here is also unroutable for
    /// the same caller (see [`Self::route_for`]), so the list can't be bypassed.
    pub fn all_models_for(&self, access: &PoolAccess) -> Vec<String> {
        self.collect_models(|p| p.kind != PoolKind::Ocr && access.allows(p))
    }

    /// Like [`Self::models_for_kind`], but only over pools `access` permits —
    /// the per-user chat / transcription / speech model dropdowns.
    pub fn models_for_kind_for(&self, kind: PoolKind, access: &PoolAccess) -> Vec<String> {
        self.collect_models(|p| p.kind == kind && access.allows(p))
    }

    /// True if a pool of *any* kind that `access` permits knows `model`. Backs
    /// the per-user `GET /v1/models/{id}`.
    pub fn knows_any_for(&self, model: &str, access: &PoolAccess) -> bool {
        self.data()
            .pools
            .values()
            .any(|p| p.kind != PoolKind::Ocr && access.allows(p) && p.knows_model(model))
    }

    /// Sorted list of `(model_id, merged_compliance)` for every model served
    /// by a pool of `kind`. When the same id is served by multiple pools the
    /// flags are merged **most-restrictively** (a flag is clear only if it's
    /// clear on *every* serving pool) — so a model that's GDPR-safe on one
    /// upstream but not another is treated as not-safe. Backs the chat-UI
    /// model dropdown labels and the per-conversation warning banner.
    pub fn models_with_compliance_for_kind(&self, kind: PoolKind) -> Vec<(String, Compliance)> {
        self.models_with_compliance_for_kind_for(kind, &PoolAccess::all())
    }

    /// Like [`Self::models_with_compliance_for_kind`], but only over pools
    /// `access` permits — the per-user chat model dropdown.
    pub fn models_with_compliance_for_kind_for(
        &self,
        kind: PoolKind,
        access: &PoolAccess,
    ) -> Vec<(String, Compliance)> {
        let d = self.data();
        let mut merged: HashMap<String, Compliance> = HashMap::new();
        for pool in d
            .pools
            .values()
            .filter(|p| p.kind == kind && access.allows(p))
        {
            for backend in &pool.backends {
                // Alias names inherit the pool's compliance flags, same as the
                // real ids — clients pick either, so both must carry the warning.
                for id in backend.listed_models() {
                    let entry = merged.entry(id).or_default();
                    // AND the flags: clear only where every serving pool is clear.
                    entry.gdpr &= pool.compliance.gdpr;
                    entry.nda &= pool.compliance.nda;
                }
            }
        }
        let mut out: Vec<(String, Compliance)> = merged.into_iter().collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// True if any pool of `kind` knows `model` (probe- or config-derived),
    /// regardless of backend health. Used to decide 404 (`model_not_found`)
    /// vs 503 before routing — see `acquire_for`.
    pub fn knows_model(&self, model: &str, kind: PoolKind) -> bool {
        self.data()
            .pools
            .values()
            .any(|p| p.kind == kind && p.knows_model(model))
    }

    /// True if any pool of *any* kind knows `model`. Backs `GET
    /// /v1/models/{id}`, which (like the list) is capability-agnostic.
    pub fn knows_any(&self, model: &str) -> bool {
        self.data().pools.values().any(|p| p.knows_model(model))
    }

    /// Whether calls to `model` (of `kind`) count toward rate limits / quotas.
    /// Limits apply if *any* pool of that kind that knows it has
    /// `enforce_limits = true` (so a model available on a paid cloud pool is
    /// always counted, even if also mirrored on a free self-hosted one).
    /// Exempt only when every serving pool sets `enforce_limits = false` — i.e.
    /// a purely self-hosted model. An unknown model defaults to enforced (fail
    /// toward counting). See `server::limits`.
    pub fn enforce_limits_for_model(&self, model: &str, kind: PoolKind) -> bool {
        let d = self.data();
        let mut known = false;
        let mut any_enforced = false;
        for pool in d
            .pools
            .values()
            .filter(|p| p.kind == kind && p.knows_model(model))
        {
            known = true;
            any_enforced |= pool.enforce_limits;
        }
        !known || any_enforced
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
        self.acquire_for_access(model, kind, &PoolAccess::all())
    }

    /// Like [`Self::acquire_for`], but only considers pools `access` permits. A
    /// model served solely by pools the caller can't access is reported as
    /// [`RouteError::UnknownModel`] (→ `404`), identical to a model that
    /// doesn't exist — so a restricted model can't be probed for existence, and
    /// filtering the `/v1/models` list can't be bypassed by calling the id.
    pub fn acquire_for_access(
        &self,
        model: &str,
        kind: PoolKind,
        access: &PoolAccess,
    ) -> Result<Acquired, RouteError> {
        let d = self.data();
        // First, a pool with a healthy backend that serves the model.
        if let Some(pool) = d
            .pools
            .values()
            .find(|p| p.kind == kind && access.allows(p) && p.serves_model(model))
        {
            return pool.acquire_for_model(model).map_err(RouteError::Acquire);
        }
        // No healthy serving backend. If the model is nonetheless known to a
        // pool of this kind the caller may access, it's a transient outage (all
        // replicas down) — surface 503, not 404.
        if let Some(pool) = d
            .pools
            .values()
            .find(|p| p.kind == kind && access.allows(p) && p.knows_model(model))
        {
            return Err(RouteError::Acquire(AcquireError::NoHealthyBackend {
                pool: pool.name.clone(),
            }));
        }
        Err(RouteError::UnknownModel(model.to_string()))
    }

    /// Resolve + acquire with the two fallbacks layered on top of
    /// [`acquire_for`] (§ Fallback models in `docs/upstreams.md`):
    ///   - **unknown model** (`UnknownModel`) → retry with `[fallback].<kind>`;
    ///   - **known but all replicas down** (`NoHealthyBackend`) → retry with
    ///     that pool's `fallback_offline`;
    ///   - **saturated** (all healthy backends at `max_inflight`) → *no*
    ///     fallback, return `503` (don't silently downgrade under load).
    ///
    /// Fallback is a **single hop**: the retry calls `acquire_for` (not
    /// `route`), so a fallback target can't itself trigger another fallback —
    /// if it's also unavailable, the *original* error is returned. This is the
    /// method the dispatch paths call; the returned [`Acquired::resolved_model`]
    /// is the real id to forward (after alias/fallback resolution).
    pub fn route(&self, model: &str, kind: PoolKind) -> Result<Acquired, RouteError> {
        self.route_access(model, kind, &PoolAccess::all())
    }

    /// Like [`Self::route`], but only over pools `access` permits — the fallback
    /// targets are gated the same way, so a fallback can never route a caller to
    /// a pool they're not allowed to use.
    pub fn route_access(
        &self,
        model: &str,
        kind: PoolKind,
        access: &PoolAccess,
    ) -> Result<Acquired, RouteError> {
        match self.acquire_for_access(model, kind, access) {
            Ok(acquired) => Ok(acquired),
            Err(RouteError::UnknownModel(orig)) => {
                let fb = self.data().fallback.for_kind(kind).map(str::to_owned);
                match fb {
                    Some(fallback) => self
                        .acquire_for_access(&fallback, kind, access)
                        .map_err(|_| RouteError::UnknownModel(orig)),
                    None => Err(RouteError::UnknownModel(orig)),
                }
            }
            Err(RouteError::Acquire(AcquireError::NoHealthyBackend { pool })) => {
                match self.pool_fallback_offline(&pool) {
                    Some(fallback) => self
                        .acquire_for_access(&fallback, kind, access)
                        .map_err(|_| RouteError::Acquire(AcquireError::NoHealthyBackend { pool })),
                    None => Err(RouteError::Acquire(AcquireError::NoHealthyBackend { pool })),
                }
            }
            // Saturated (or any other) → surface as-is; no fallback under load.
            Err(other) => Err(other),
        }
    }

    /// The `fallback_offline` model configured on the named pool, if any.
    fn pool_fallback_offline(&self, pool_name: &str) -> Option<String> {
        self.data()
            .pools
            .get(pool_name)
            .and_then(|p| p.fallback_offline.clone())
    }

    /// The configured unknown-model fallback for `kind` (`[fallback]`), if any.
    /// Read-only admin view.
    pub fn fallback_model(&self, kind: PoolKind) -> Option<String> {
        self.data().fallback.for_kind(kind).map(str::to_owned)
    }

    /// Resolve a requested name to the real model id a healthy backend of
    /// `kind` would serve it as, **without acquiring a slot**. Alias-aware;
    /// `None` when no healthy backend of that kind currently serves it. For
    /// callers that must rewrite a request body's `model` before a slot is
    /// taken (the chat-UI driver serialises before acquiring). Does not apply
    /// fallback — that's `route`'s job at acquire time.
    pub fn resolve_model(&self, model: &str, kind: PoolKind) -> Option<String> {
        self.resolve_model_for(model, kind, &PoolAccess::all())
    }

    /// Like [`Self::resolve_model`], but only over pools `access` permits. Used
    /// by the chat-UI driver so it resolves a body's `model` only against pools
    /// the signed-in user may use.
    pub fn resolve_model_for(
        &self,
        model: &str,
        kind: PoolKind,
        access: &PoolAccess,
    ) -> Option<String> {
        self.data()
            .pools
            .values()
            .filter(|p| p.kind == kind && access.allows(p))
            .find_map(|p| p.resolve_healthy(model))
    }
}

/// Boot-time alias validation (§ Alias validation in `docs/upstreams.md`).
/// Only conflicts knowable from *config* are checked here — the runtime,
/// probe-discovered kind (a bare alias on a multi-model backend) is handled by
/// [`Backend::reevaluate_aliases`]. Two failures refuse startup:
///   - an alias name that shadows a real model id declared in config;
///   - a map-form target that isn't in that backend's configured `models`
///     (only checkable when the backend declares `models`; otherwise deferred
///     — the alias just won't resolve until the probe reports the target).
fn validate_aliases(pool_configs: &HashMap<String, UpstreamPoolConfig>) -> Result<(), BuildError> {
    // Every real model id config actually names. Probe-discovered ids aren't
    // known at build time, so a collision with one of those can't be caught
    // here — but a real id wins over an alias at resolve time regardless.
    let mut config_ids: HashSet<&str> = HashSet::new();
    for cfg in pool_configs.values() {
        config_ids.extend(cfg.models.iter().map(String::as_str));
        for b in &cfg.backend {
            config_ids.extend(b.models.iter().map(String::as_str));
        }
    }
    for (pool_name, cfg) in pool_configs {
        for b in &cfg.backend {
            let Some(spec) = &b.alias else { continue };
            // This backend's effective config models (backend wins over pool).
            let declared: &[String] = if b.models.is_empty() {
                &cfg.models
            } else {
                &b.models
            };
            for (alias, target) in spec.into_map() {
                if config_ids.contains(alias.as_str()) {
                    return Err(BuildError::AliasCollidesWithModel {
                        alias,
                        pool: pool_name.clone(),
                        backend: b.name.clone(),
                    });
                }
                if let Some(target) = target
                    && !declared.is_empty()
                    && !declared.contains(&target)
                {
                    return Err(BuildError::AliasTargetUnknown {
                        alias,
                        target,
                        pool: pool_name.clone(),
                        backend: b.name.clone(),
                        declared: declared.to_vec(),
                    });
                }
            }
        }
    }
    Ok(())
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
        AliasSpec, BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig,
    };

    fn backend(name: &str, max_inflight: u32) -> BackendConfig {
        BackendConfig {
            name: name.into(),
            base_url: format!("http://{name}:8000/v1"),
            api_key_env: None,
            api_key: None,
            weight: 1,
            max_inflight,
            health_path: "/models".into(),
            models: Vec::new(),
            alias: None,
            probe_models: true,
            supports_edit: false,
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
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            compliance: Default::default(),
            enforce_limits: true,
            kind,
            strategy,
            models: Vec::new(),
            fallback_offline: None,
            backend: backends,
        }
    }

    /// Pool carrying explicit compliance flags.
    fn pool_config_with_compliance(
        kind: PoolKind,
        compliance: Compliance,
        backends: Vec<BackendConfig>,
    ) -> UpstreamPoolConfig {
        UpstreamPoolConfig {
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            compliance,
            kind,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            fallback_offline: None,
            enforce_limits: true,
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
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            compliance: Default::default(),
            enforce_limits: true,
            kind,
            strategy: PickerStrategy::RoundRobin,
            models: models.iter().map(|s| (*s).to_string()).collect(),
            fallback_offline: None,
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
        let d = reg.data();
        let pool = d.pools.get(pool).expect("pool exists");
        let set: HashSet<String> = models.iter().map(|s| (*s).to_string()).collect();
        pool.backends[backend_idx].set_models(set);
    }

    #[test]
    fn speech_target_prefers_configured_model_over_probe_flood() {
        // Regression: a cloud provider's /models probe reports its whole
        // catalogue. speech_target must return the operator's declared TTS
        // model, not the alphabetically-first of the flood.
        let cfg: UpstreamPoolConfig = toml::from_str(
            r#"
            kind = "speech"
            models = ["tts-1"]
            [voices]
            "" = "alloy"
            de = "nova"
            [[backend]]
            name = "openai"
            base_url = "https://api.openai.com/v1"
        "#,
        )
        .unwrap();
        let reg = build(vec![("openai_tts", cfg)]);
        // Simulate the OpenAI /models flood.
        seed_models(
            &reg,
            "openai_tts",
            0,
            &["babbage-002", "gpt-4o", "tts-1", "whisper-1"],
        );

        assert!(reg.has_speech());
        // Configured model wins over the flood; voice resolves by language.
        assert_eq!(
            reg.speech_target("de"),
            Some(("tts-1".to_string(), Some("nova".to_string())))
        );
        // Unknown language → the "" default voice.
        assert_eq!(
            reg.speech_target("fr"),
            Some(("tts-1".to_string(), Some("alloy".to_string())))
        );
    }

    #[test]
    fn speech_voices_are_the_operators_declared_set_deduped() {
        // Two speech pools, one voice shared between them and one restricted to
        // a group. The picker must offer each distinct voice once, and only
        // from pools the caller may route to.
        let open: UpstreamPoolConfig = toml::from_str(
            r#"
            kind = "speech"
            models = ["tts-1"]
            [voices]
            "" = "alloy"
            de = "onyx"
            [[backend]]
            name = "openai"
            base_url = "https://api.openai.com/v1"
        "#,
        )
        .unwrap();
        let restricted: UpstreamPoolConfig = toml::from_str(
            r#"
            kind = "speech"
            models = ["tts-1"]
            allowed_groups = ["studio"]
            [voices]
            "" = "onyx"
            en = "sage"
            [[backend]]
            name = "local"
            base_url = "http://tts.example.com"
        "#,
        )
        .unwrap();
        let reg = build(vec![("cloud", open), ("studio", restricted)]);

        // A member of no group sees only the unrestricted pool's voices.
        let plain = PoolAccess::default();
        assert_eq!(reg.speech_voices_for(&plain), vec!["alloy", "onyx"]);
        // With access to both, `onyx` still appears exactly once.
        assert_eq!(
            reg.speech_voices_for(&PoolAccess::all()),
            vec!["alloy", "onyx", "sage"]
        );
    }

    #[test]
    fn an_explicit_voice_menu_leads_and_the_language_map_follows() {
        // The reason `offer_voices` exists: three voices for ONE language,
        // which the (pool, lang) keyed map cannot hold. The menu keeps the
        // operator's order — the house voice belongs first, not wherever the
        // alphabet puts it — and the map's own voices fill in behind it.
        let cfg: UpstreamPoolConfig = toml::from_str(
            r#"
            kind = "speech"
            models = ["tts-1"]
            offer_voices = ["marin", "cedar", "alloy"]
            [voices]
            "" = "alloy"
            de = "onyx"
            [[backend]]
            name = "openai"
            base_url = "https://api.openai.com/v1"
        "#,
        )
        .unwrap();
        let reg = build(vec![("cloud", cfg)]);
        assert_eq!(
            reg.speech_voices_for(&PoolAccess::default()),
            vec!["marin", "cedar", "alloy", "onyx"]
        );
        // Resolution is untouched by the menu: `de` still gets its own voice.
        assert_eq!(
            reg.speech_target("de"),
            Some(("tts-1".to_string(), Some("onyx".to_string())))
        );
    }

    #[test]
    fn speech_target_none_without_speech_pool() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend("a", 16)],
            ),
        )]);
        assert!(!reg.has_speech());
        assert_eq!(reg.speech_target("en"), None);
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
    fn pool_allowed_groups_gate_listing_and_routing() {
        // Two chat pools: "open" (unrestricted) and "vip" (restricted to the
        // `dev` gateway group). A user without the group sees only the open
        // pool's model and can't route to the vip one; a member and an admin
        // both see everything.
        let mut vip = pool_config(
            PoolKind::Chat,
            PickerStrategy::RoundRobin,
            vec![backend("vip-b", 16)],
        );
        vip.allowed_groups = vec!["dev".into()];
        let reg = build(vec![
            (
                "open",
                pool_config(
                    PoolKind::Chat,
                    PickerStrategy::RoundRobin,
                    vec![backend("open-b", 16)],
                ),
            ),
            ("vip", vip),
        ]);
        seed_models(&reg, "open", 0, &["open-model"]);
        seed_models(&reg, "vip", 0, &["vip-model"]);

        let outsider = PoolAccess {
            role_ids: vec!["other".into()],
            is_admin: false,
        };
        let member = PoolAccess {
            role_ids: vec!["dev".into()],
            is_admin: false,
        };
        let admin = PoolAccess::all();

        // Listing: the restricted model is withheld from the outsider only.
        let outsider_models = reg.all_models_for(&outsider);
        assert!(outsider_models.contains(&"open-model".to_string()));
        assert!(!outsider_models.contains(&"vip-model".to_string()));
        assert!(
            reg.all_models_for(&member)
                .contains(&"vip-model".to_string())
        );
        assert!(
            reg.all_models_for(&admin)
                .contains(&"vip-model".to_string())
        );

        // knows_any_for mirrors the listing (backs GET /v1/models/{id}).
        assert!(!reg.knows_any_for("vip-model", &outsider));
        assert!(reg.knows_any_for("vip-model", &member));

        // Routing: an outsider can't reach the restricted model — it's reported
        // as UnknownModel (404), identical to a nonexistent one, so the listing
        // filter can't be bypassed by calling the id directly. The open model
        // still routes for them.
        assert!(matches!(
            reg.acquire_for_access("vip-model", PoolKind::Chat, &outsider),
            Err(RouteError::UnknownModel(_))
        ));
        assert!(
            reg.acquire_for_access("open-model", PoolKind::Chat, &outsider)
                .is_ok()
        );
        // A member (and an admin) can route to the restricted model.
        assert!(
            reg.acquire_for_access("vip-model", PoolKind::Chat, &member)
                .is_ok()
        );
        assert!(
            reg.acquire_for_access("vip-model", PoolKind::Chat, &admin)
                .is_ok()
        );
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
    fn models_with_alias_target_marks_aliases_and_real_ids() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend_alias("a", targets(&[("smart", "glm-4.6")]))],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["glm-4.6", "glm-4.5-air"]);

        let map: std::collections::HashMap<String, Option<String>> = reg
            .models_with_alias_target(PoolKind::Chat)
            .into_iter()
            .collect();
        // Real ids own their settings — no alias target.
        assert_eq!(map["glm-4.6"], None);
        assert_eq!(map["glm-4.5-air"], None);
        // The alias resolves to its real target and carries no row of its own.
        assert_eq!(map["smart"], Some("glm-4.6".to_string()));
        // Exactly the two reals plus the one alias, nothing else.
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn compliance_flags_attach_per_model_and_default_clear() {
        let reg = build(vec![
            (
                "zai",
                pool_config_with_compliance(
                    PoolKind::Chat,
                    Compliance {
                        gdpr: false,
                        nda: false,
                    },
                    vec![backend("zai", 16)],
                ),
            ),
            (
                "qwen",
                pool_config(
                    PoolKind::Chat,
                    PickerStrategy::RoundRobin,
                    vec![backend("qwen", 16)],
                ),
            ),
        ]);
        seed_models(&reg, "zai", 0, &["glm-4.6"]);
        seed_models(&reg, "qwen", 0, &["qwen-3"]);

        let map: std::collections::HashMap<String, Compliance> = reg
            .models_with_compliance_for_kind(PoolKind::Chat)
            .into_iter()
            .collect();
        // Flagged pool propagates to its model…
        assert_eq!(
            map["glm-4.6"],
            Compliance {
                gdpr: false,
                nda: false
            }
        );
        // …and a pool with no compliance block stays all-clear.
        assert!(map["qwen-3"].is_all_clear());
    }

    #[test]
    fn compliance_merges_most_restrictively_across_pools() {
        // Same model id served by two pools: one GDPR-safe, one not. The
        // merge must take the restrictive view (not safe).
        let reg = build(vec![
            (
                "safe",
                pool_config(
                    PoolKind::Chat,
                    PickerStrategy::RoundRobin,
                    vec![backend("safe", 16)],
                ),
            ),
            (
                "unsafe",
                pool_config_with_compliance(
                    PoolKind::Chat,
                    Compliance {
                        gdpr: false,
                        nda: true,
                    },
                    vec![backend("unsafe", 16)],
                ),
            ),
        ]);
        seed_models(&reg, "safe", 0, &["shared"]);
        seed_models(&reg, "unsafe", 0, &["shared"]);

        let map: std::collections::HashMap<String, Compliance> = reg
            .models_with_compliance_for_kind(PoolKind::Chat)
            .into_iter()
            .collect();
        // gdpr false on one pool wins; nda clear on both stays clear.
        assert_eq!(
            map["shared"],
            Compliance {
                gdpr: false,
                nda: true
            }
        );
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
        reg.data().pools.get("chat").unwrap().backends[0].set_healthy(false);
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
        let d = reg.data();
        let pool = d.pools.get("chat").unwrap();
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
    fn image_pool_routes_by_config_models() {
        // An image backend whose `/models` isn't discovered (probe off, or no
        // such endpoint) is still routable via its static model ids — the same
        // mechanism transcription relies on, now for PoolKind::Image.
        let reg = build(vec![(
            "images",
            pool_config_with_models(PoolKind::Image, &["glm-image"], vec![backend("a", 16)]),
        )]);
        let g = reg.acquire_for("glm-image", PoolKind::Image).unwrap();
        assert_eq!(g.backend().name, "a");
        assert!(reg.knows_model("glm-image", PoolKind::Image));
        // Wrong kind must not match an image model.
        assert!(reg.acquire_for("glm-image", PoolKind::Chat).is_err());
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
    fn config_models_allowlist_restricts_live_probe() {
        // A configured `models` list is an allowlist over the live probe: only
        // the ids it names are served/advertised; a probed id it omits is
        // discovered-but-withheld (404 on request, absent from `/v1/models`).
        let reg = build(vec![(
            "voice",
            pool_config_with_models(
                PoolKind::Transcription,
                &["keep-a", "keep-b"],
                vec![backend("a", 16)],
            ),
        )]);
        seed_models(&reg, "voice", 0, &["keep-a", "keep-b", "drop-c"]);
        assert!(
            reg.acquire_for("keep-a", PoolKind::Transcription).is_ok(),
            "allowlisted id must route"
        );
        assert!(reg.acquire_for("keep-b", PoolKind::Transcription).is_ok());
        let err = reg
            .acquire_for("drop-c", PoolKind::Transcription)
            .unwrap_err();
        assert!(
            matches!(err, RouteError::UnknownModel(_)),
            "withheld id must 404 even though the backend reports it: {err:?}"
        );
        // Advertised set is the allowlist ∩ probe, not the whole probe.
        assert_eq!(reg.all_models(), vec!["keep-a", "keep-b"]);
        // The withheld id surfaces for the struck-through UI chip.
        let d = reg.data();
        let b = &d.pools.get("voice").unwrap().backends[0];
        assert_eq!(b.withheld_models(), HashSet::from(["drop-c".to_string()]));
    }

    #[test]
    fn empty_model_list_serves_whole_probe_and_withholds_nothing() {
        // With no configured list, the probe set is served verbatim — the
        // allowlist is opt-in, so unconfigured pools are unaffected.
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend("a", 16)],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["m1", "m2"]);
        assert!(reg.acquire_for("m1", PoolKind::Chat).is_ok());
        assert!(reg.acquire_for("m2", PoolKind::Chat).is_ok());
        assert_eq!(reg.all_models(), vec!["m1", "m2"]);
        let d = reg.data();
        let b = &d.pools.get("chat").unwrap().backends[0];
        assert!(
            b.withheld_models().is_empty(),
            "no allowlist → nothing withheld"
        );
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
        reg.data().pools.get("chat").unwrap().backends[0].set_healthy(false);

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

    // ------- aliases + fallback -------

    fn backend_alias(name: &str, spec: AliasSpec) -> BackendConfig {
        BackendConfig {
            alias: Some(spec),
            ..backend(name, 16)
        }
    }

    fn names(v: &[&str]) -> AliasSpec {
        AliasSpec::Names(v.iter().map(|s| (*s).to_string()).collect())
    }

    fn targets(pairs: &[(&str, &str)]) -> AliasSpec {
        AliasSpec::Targets(
            pairs
                .iter()
                .map(|(k, t)| ((*k).to_string(), (*t).to_string()))
                .collect(),
        )
    }

    fn pool_offline(
        kind: PoolKind,
        offline: &str,
        backends: Vec<BackendConfig>,
    ) -> UpstreamPoolConfig {
        UpstreamPoolConfig {
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            compliance: Default::default(),
            enforce_limits: true,
            kind,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            fallback_offline: Some(offline.to_string()),
            backend: backends,
        }
    }

    fn build_with_fallback(
        pools: Vec<(&str, UpstreamPoolConfig)>,
        fallback: FallbackConfig,
    ) -> Arc<UpstreamRegistry> {
        let map: HashMap<String, UpstreamPoolConfig> =
            pools.into_iter().map(|(k, v)| (k.into(), v)).collect();
        UpstreamRegistry::with_fallback(&map, fallback).unwrap()
    }

    fn try_build(
        pools: Vec<(&str, UpstreamPoolConfig)>,
    ) -> Result<Arc<UpstreamRegistry>, BuildError> {
        let map: HashMap<String, UpstreamPoolConfig> =
            pools.into_iter().map(|(k, v)| (k.into(), v)).collect();
        UpstreamRegistry::new(&map)
    }

    fn set_health(reg: &UpstreamRegistry, pool: &str, idx: usize, healthy: bool) {
        reg.data().pools.get(pool).unwrap().backends[idx].set_healthy(healthy);
    }

    #[test]
    fn bare_alias_resolves_to_backends_sole_model() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend_alias("a", names(&["qwen", "fast"]))],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["Qwen/Qwen3-235B"]);

        // Both alias names route, and resolve to the backend's real id.
        let g = reg.route("qwen", PoolKind::Chat).unwrap();
        assert_eq!(g.backend().name, "a");
        assert_eq!(g.resolved_model(), "Qwen/Qwen3-235B");
        assert_eq!(
            reg.route("fast", PoolKind::Chat).unwrap().resolved_model(),
            "Qwen/Qwen3-235B"
        );
        // The real id still routes and resolves to itself.
        assert_eq!(
            reg.route("Qwen/Qwen3-235B", PoolKind::Chat)
                .unwrap()
                .resolved_model(),
            "Qwen/Qwen3-235B"
        );
        // Both the alias and the real id are listed.
        let listed = reg.all_models();
        assert!(listed.contains(&"qwen".to_string()));
        assert!(listed.contains(&"fast".to_string()));
        assert!(listed.contains(&"Qwen/Qwen3-235B".to_string()));
    }

    #[test]
    fn shared_alias_forms_a_group_resolving_to_each_backends_real_id() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![
                    backend_alias("a", names(&["qwen"])),
                    backend_alias("b", names(&["qwen"])),
                ],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["Qwen/Qwen2.5-72B"]);
        seed_models(&reg, "chat", 1, &["Qwen/Qwen3-30B-A3B"]);

        // Round-robin across the group; each hop rewrites to that backend's id.
        let g1 = reg.route("qwen", PoolKind::Chat).unwrap();
        let g2 = reg.route("qwen", PoolKind::Chat).unwrap();
        let mut resolved = [
            g1.resolved_model().to_string(),
            g2.resolved_model().to_string(),
        ];
        resolved.sort();
        assert_eq!(resolved, ["Qwen/Qwen2.5-72B", "Qwen/Qwen3-30B-A3B"]);
        // Pinning a real id hits exactly that backend.
        assert_eq!(
            reg.route("Qwen/Qwen3-30B-A3B", PoolKind::Chat)
                .unwrap()
                .backend()
                .name,
            "b"
        );
    }

    #[test]
    fn bare_alias_disabled_when_backend_serves_multiple_models() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend_alias("a", names(&["qwen"]))],
            ),
        )]);
        // Two models → "the sole model" is ambiguous → alias disabled + logged.
        seed_models(&reg, "chat", 0, &["m-1", "m-2"]);
        assert!(matches!(
            reg.route("qwen", PoolKind::Chat).unwrap_err(),
            RouteError::UnknownModel(_)
        ));
        assert!(!reg.all_models().contains(&"qwen".to_string()));
        // The backend's real ids still route fine.
        assert!(reg.route("m-1", PoolKind::Chat).is_ok());
        // Drop back to one model → alias re-enables.
        seed_models(&reg, "chat", 0, &["m-1"]);
        assert_eq!(
            reg.route("qwen", PoolKind::Chat).unwrap().resolved_model(),
            "m-1"
        );
    }

    #[test]
    fn map_alias_targets_specific_model_even_on_multi_model_backend() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend_alias(
                    "zai",
                    targets(&[("smart", "glm-4.6"), ("cheap", "glm-4.5-air")]),
                )],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["glm-4.6", "glm-4.5-air"]);
        assert_eq!(
            reg.route("smart", PoolKind::Chat).unwrap().resolved_model(),
            "glm-4.6"
        );
        assert_eq!(
            reg.route("cheap", PoolKind::Chat).unwrap().resolved_model(),
            "glm-4.5-air"
        );
    }

    #[test]
    fn map_alias_does_not_resolve_while_target_unserved() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend_alias("zai", targets(&[("smart", "glm-4.6")]))],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["something-else"]);
        assert!(matches!(
            reg.route("smart", PoolKind::Chat).unwrap_err(),
            RouteError::UnknownModel(_)
        ));
    }

    #[test]
    fn alias_colliding_with_config_model_refuses_build() {
        // Backend statically declares model "qwen" AND an alias "qwen".
        let mut b = backend_alias("a", names(&["qwen"]));
        b.models = vec!["qwen".into()];
        let err = try_build(vec![(
            "chat",
            pool_config(PoolKind::Chat, PickerStrategy::RoundRobin, vec![b]),
        )])
        .unwrap_err();
        assert!(
            matches!(err, BuildError::AliasCollidesWithModel { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn map_target_not_in_declared_models_refuses_build() {
        let mut b = backend_alias("a", targets(&[("smart", "not-served")]));
        b.models = vec!["glm-4.6".into()];
        let err = try_build(vec![(
            "chat",
            pool_config(PoolKind::Chat, PickerStrategy::RoundRobin, vec![b]),
        )])
        .unwrap_err();
        assert!(
            matches!(err, BuildError::AliasTargetUnknown { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn fallback_offline_fires_only_when_whole_group_is_down() {
        let reg = build(vec![
            (
                "local",
                pool_offline(
                    PoolKind::Chat,
                    "cloud-model",
                    vec![backend("a", 16), backend("b", 16)],
                ),
            ),
            (
                "cloud",
                pool_config(
                    PoolKind::Chat,
                    PickerStrategy::RoundRobin,
                    vec![backend("c", 16)],
                ),
            ),
        ]);
        seed_models(&reg, "local", 0, &["m"]);
        seed_models(&reg, "local", 1, &["m"]);
        seed_models(&reg, "cloud", 0, &["cloud-model"]);

        // One replica down → still served by the other, NOT the offline backup.
        set_health(&reg, "local", 0, false);
        let g = reg.route("m", PoolKind::Chat).unwrap();
        assert_eq!(g.resolved_model(), "m");
        assert_eq!(g.backend().name, "b");

        // Whole group down → spill to fallback_offline.
        set_health(&reg, "local", 1, false);
        let g = reg.route("m", PoolKind::Chat).unwrap();
        assert_eq!(g.resolved_model(), "cloud-model");
        assert_eq!(g.backend().name, "c");
    }

    #[test]
    fn fallback_offline_single_hop_returns_original_503_when_backup_also_down() {
        let reg = build(vec![(
            "local",
            pool_offline(PoolKind::Chat, "cloud-model", vec![backend("a", 16)]),
        )]);
        seed_models(&reg, "local", 0, &["m"]);
        set_health(&reg, "local", 0, false); // known but down; backup "cloud-model" served by nobody
        assert!(matches!(
            reg.route("m", PoolKind::Chat).unwrap_err(),
            RouteError::Acquire(AcquireError::NoHealthyBackend { .. })
        ));
    }

    #[test]
    fn unknown_model_falls_back_per_kind() {
        let reg = build_with_fallback(
            vec![(
                "chat",
                pool_config(
                    PoolKind::Chat,
                    PickerStrategy::RoundRobin,
                    vec![backend("a", 16)],
                ),
            )],
            FallbackConfig {
                chat: Some("house-model".into()),
                ..Default::default()
            },
        );
        seed_models(&reg, "chat", 0, &["house-model"]);
        // Never-heard-of model → substitute the house model.
        let g = reg.route("gpt-4-turbo", PoolKind::Chat).unwrap();
        assert_eq!(g.resolved_model(), "house-model");
        // Unset kind isn't rescued by the chat fallback.
        assert!(matches!(
            reg.route("gpt-4-turbo", PoolKind::Embedding).unwrap_err(),
            RouteError::UnknownModel(_)
        ));
    }

    #[test]
    fn unknown_fallback_is_single_hop_and_404_without_config() {
        // No fallback configured → plain 404.
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend("a", 16)],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["m"]);
        assert!(matches!(
            reg.route("nope", PoolKind::Chat).unwrap_err(),
            RouteError::UnknownModel(_)
        ));

        // Fallback points at a model nobody serves → original 404, no loop.
        let reg = build_with_fallback(
            vec![(
                "chat",
                pool_config(
                    PoolKind::Chat,
                    PickerStrategy::RoundRobin,
                    vec![backend("a", 16)],
                ),
            )],
            FallbackConfig {
                chat: Some("also-missing".into()),
                ..Default::default()
            },
        );
        seed_models(&reg, "chat", 0, &["m"]);
        assert!(matches!(
            reg.route("nope", PoolKind::Chat).unwrap_err(),
            RouteError::UnknownModel(_)
        ));
    }

    #[test]
    fn saturation_does_not_fall_back() {
        let reg = build(vec![(
            "local",
            pool_offline(PoolKind::Chat, "cloud-model", vec![backend("a", 1)]),
        )]);
        seed_models(&reg, "local", 0, &["m"]);
        // Take the only slot; the model is loaded+healthy but saturated.
        let _held = reg.route("m", PoolKind::Chat).unwrap();
        assert!(
            matches!(
                reg.route("m", PoolKind::Chat).unwrap_err(),
                RouteError::Acquire(AcquireError::Saturated { .. })
            ),
            "saturation must 503, never spill to fallback_offline"
        );
    }

    #[test]
    fn resolve_model_is_alias_aware_and_takes_no_slot() {
        let reg = build(vec![(
            "chat",
            pool_config(
                PoolKind::Chat,
                PickerStrategy::RoundRobin,
                vec![backend_alias("a", names(&["qwen"]))],
            ),
        )]);
        seed_models(&reg, "chat", 0, &["Qwen/Qwen3-235B"]);
        assert_eq!(
            reg.resolve_model("qwen", PoolKind::Chat).as_deref(),
            Some("Qwen/Qwen3-235B")
        );
        assert_eq!(reg.resolve_model("nope", PoolKind::Chat), None);
        // No inflight slot consumed — the backend is still at 0.
        assert_eq!(
            reg.data().pools.get("chat").unwrap().backends[0].inflight(),
            0
        );
    }

    /// A reload bumps the generation and carries an unchanged backend's live
    /// (probed) model set onto its freshly-built replacement, so routing never
    /// 404s during the re-probe window.
    #[test]
    fn reload_carries_over_live_models_and_bumps_generation() {
        use crate::server::db::upstreams_config::{BackendRow, PoolRow, UpstreamConfigSnapshot};
        use jiff::Timestamp;

        let mk_snap = || {
            let mut snap = UpstreamConfigSnapshot::default();
            snap.backends.insert(
                "b".into(),
                BackendRow {
                    name: "b".into(),
                    base_url: "http://b".into(),
                    api_key_env: None,
                    api_key_ct: None,
                    api_key_nonce: None,
                    weight: 1,
                    max_inflight: 16,
                    health_path: "/models".into(),
                    probe_models: true,
                    supports_edit: false,
                    models: vec![],
                    aliases: vec![],
                    created_at: Timestamp::now(),
                    updated_at: Timestamp::now(),
                },
            );
            snap.pools.push(PoolRow {
                name: "chat".into(),
                kind: "chat".into(),
                strategy: "least_inflight".into(),
                fallback_offline: None,
                compliance_gdpr: true,
                compliance_nda: true,
                enforce_limits: true,
                sort_order: 0,
                allowed_groups: Vec::new(),
                backends: vec!["b".into()],
                models: vec![],
                voices: vec![],
                offer_voices: Vec::new(),
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
            });
            snap
        };

        let reg = UpstreamRegistry::from_snapshot(
            &mk_snap(),
            &crate::server::crypto::Crypto::ephemeral(),
        )
        .unwrap();
        assert_eq!(reg.generation(), 0);

        // Simulate a successful probe populating the live model set.
        reg.data().pools.get("chat").unwrap().backends[0]
            .set_models(HashSet::from(["live-model".to_string()]));
        assert!(reg.knows_model("live-model", PoolKind::Chat));

        // Reload the same topology: the rebuilt backend starts with an empty
        // live set, but the carry-over must keep "live-model" routable.
        reg.reload(&mk_snap(), &crate::server::crypto::Crypto::ephemeral())
            .unwrap();
        assert_eq!(reg.generation(), 1);
        assert!(
            reg.knows_model("live-model", PoolKind::Chat),
            "reload must carry over the live model set for an unchanged backend"
        );
    }
}
