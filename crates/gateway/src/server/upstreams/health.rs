// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-backend health checker + model discovery.
//!
//! Each backend gets one background task that pings `<base_url><health_path>`
//! (default `/models`) every 5 s with a 2 s timeout. The probe does two
//! jobs from one round-trip:
//!
//!   1. **Liveness** — three consecutive failures flip the backend to
//!      unhealthy; one success flips it back. The picker in `registry.rs`
//!      skips unhealthy backends.
//!   2. **Model discovery** — on every success the response body is parsed
//!      as the OpenAI `/models` envelope (`{"data": [{"id": "..."}, ...]}`)
//!      and the backend's advertised-model set is replaced wholesale. The
//!      router in `acquire_for` reads that set to decide which pool handles
//!      a given model. No static route table.
//!
//! Bootstrap: `spawn` is `async` because it does one initial parallel probe
//! round and awaits it before returning. That way the gateway doesn't start
//! serving traffic with empty model sets — the first `POST /v1/chat/
//! completions` lands on a registry that already knows what's where.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::sleep;

use super::registry::{Backend, UpstreamRegistry};

const PROBE_INTERVAL: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const FAILURE_THRESHOLD: u32 = 3;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// OpenAI `/models` response envelope — only the `id` field per item is
/// load-bearing. `created`, `object`, `owned_by` are present in the wire
/// shape but we don't use them, so we skip them entirely instead of
/// deserialising into `serde_json::Value`.
#[derive(Deserialize)]
struct ModelsEnvelope {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// reqwest client for health probes: NO idle connection pooling. Probes fire
/// every [`PROBE_INTERVAL`], so a pooled keep-alive would sit idle between
/// them and get closed by the upstream/NAT — the next probe would reuse a dead
/// connection and fail with a spurious "connection reset" while the backend is
/// actually serving. A fresh connection per probe costs nothing at this
/// cadence and removes that whole class of false alarms. (Real traffic uses
/// the pooled `state.http` client.)
fn probe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Spawns one background task per backend. Awaits an initial parallel
/// probe round before returning, so the registry has at least one
/// model-set update per reachable backend before traffic starts.
pub async fn spawn(registry: Arc<UpstreamRegistry>) {
    let http = probe_client();
    let mut initial = Vec::new();
    for pool in registry.pools() {
        for backend in &pool.backends {
            let http = http.clone();
            let pool_name = pool.name.clone();
            let backend = Arc::clone(backend);
            initial.push(tokio::spawn(async move {
                probe_once(&http, &pool_name, &backend).await;
            }));
        }
    }
    // Block startup until every backend has been probed at least once
    // (or its probe has timed out at 2 s). Failures here aren't fatal —
    // the looping probe will retry, and an unreachable backend just
    // stays out of routing decisions until it recovers.
    for handle in initial {
        let _ = handle.await;
    }

    // Now arm the looping probe per backend. Each loop owns its own
    // failure counter — the bootstrap probe above doesn't pre-seed it
    // because mid-startup flaps shouldn't permanently mark a backend
    // unhealthy.
    for pool in registry.pools() {
        for backend in &pool.backends {
            let backend = Arc::clone(backend);
            let pool_name = pool.name.clone();
            let http = http.clone();
            tokio::spawn(async move {
                run_probe(http, pool_name, backend).await;
            });
        }
    }
}

/// Periodic liveness heartbeat. Per-probe failures are silent while a backend
/// stays healthy (only transitions log), so "quiet logs" no longer prove the
/// gateway is alive vs. hung. This emits one line every
/// [`HEARTBEAT_INTERVAL`] that affirmatively says the process is running and
/// summarises upstream health at a glance — INFO when all backends are
/// healthy, WARN when degraded so a filtered log still surfaces it. The first
/// line is emitted immediately at startup. Fire-and-forget; the task itself
/// running is the liveness proof.
pub fn spawn_heartbeat(registry: Arc<UpstreamRegistry>) {
    tokio::spawn(async move {
        loop {
            let mut total = 0usize;
            let mut healthy = 0usize;
            let mut per_pool: Vec<String> = Vec::new();
            for pool in registry.pools() {
                let p_total = pool.backends.len();
                let p_healthy = pool.backends.iter().filter(|b| b.is_healthy()).count();
                total += p_total;
                healthy += p_healthy;
                per_pool.push(format!("{}={p_healthy}/{p_total}", pool.name));
            }
            let pools = per_pool.join(" ");
            if total == 0 {
                tracing::info!("gateway alive — no upstream backends configured");
            } else if healthy == total {
                tracing::info!(
                    backends = format!("{healthy}/{total} healthy"),
                    pools = %pools,
                    "gateway alive — all upstreams healthy"
                );
            } else {
                tracing::warn!(
                    backends = format!("{healthy}/{total} healthy"),
                    pools = %pools,
                    "gateway alive — DEGRADED, some upstreams down"
                );
            }
            sleep(HEARTBEAT_INTERVAL).await;
        }
    });
}

/// Single round of probing — used by both the bootstrap path and the
/// looping path. Updates liveness + advertised-model set on success; on
/// failure, only returns the outcome (the caller decides whether one
/// failure flips health or only the third).
async fn probe_once(http: &reqwest::Client, pool_name: &str, backend: &Backend) -> ProbeOutcome {
    let url = format!("{}{}", backend.base_url, backend.health_path);
    // Send the backend's API key on the probe — same `Authorization:
    // Bearer …` header `proxy.rs` adds to real requests. Without it the
    // upstream's access log fills with anonymous-401s from the gateway
    // every 5 s.
    let mut req = http.get(&url).header(
        "user-agent",
        concat!("llm-gateway/", env!("CARGO_PKG_VERSION"), " healthcheck"),
    );
    if let Some(key) = backend.api_key.as_deref() {
        req = req.bearer_auth(key);
    }
    let result = tokio::time::timeout(PROBE_TIMEOUT, req.send()).await;

    let resp = match result {
        Ok(Ok(resp)) => resp,
        // Transport-level failure — no HTTP response came back. reqwest's
        // own Display is the opaque "error sending request for url (…)"; we
        // dig the concrete cause out of the source chain so the caller can
        // log something an admin can act on.
        Ok(Err(err)) => return ProbeOutcome::Failed(describe_transport_error(&err)),
        Err(_) => {
            return ProbeOutcome::Failed(format!(
                "no response within the {PROBE_TIMEOUT:?} probe timeout"
            ));
        }
    };

    let status = resp.status();

    // 401 counts as "alive" — even with the api_key header, some
    // upstreams scope `/models` differently from `/chat/completions`.
    // We can't discover models from a 401 body, but the backend is
    // reachable, so leave its model set alone and just mark it healthy.
    // If real requests get 401 too, they'll surface the failure end to
    // end; if the previous probe round populated the model set, that
    // state survives until a successful probe replaces it.
    if status.as_u16() == 401 {
        return ProbeOutcome::AliveNoData;
    }
    if !status.is_success() {
        return ProbeOutcome::Failed(format!(
            "upstream returned HTTP {} from {}",
            status.as_u16(),
            backend.health_path
        ));
    }

    // Parse the OpenAI `/models` envelope. A backend that returns 200
    // with a different shape (or non-JSON entirely — e.g. plain
    // whisper.cpp) is alive but unparseable: we mark it healthy and
    // leave the model set unchanged, so the operator can either keep
    // a previously-populated set or accept that the backend won't be
    // routable.
    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(err) => {
            tracing::debug!(
                pool = %pool_name, backend = %backend.name, error = %err,
                "reading /models body failed"
            );
            return ProbeOutcome::AliveNoData;
        }
    };
    let parsed: Result<ModelsEnvelope, _> = serde_json::from_slice(&body);
    let envelope = match parsed {
        Ok(env) => env,
        Err(err) => {
            tracing::debug!(
                pool = %pool_name, backend = %backend.name, error = %err,
                "parsing /models body failed; leaving model set unchanged"
            );
            return ProbeOutcome::AliveNoData;
        }
    };
    let mut new_set: HashSet<String> = envelope.data.into_iter().map(|m| m.id).collect();
    new_set.retain(|s| !s.is_empty());

    let previous = backend.probe_models();
    if previous != new_set {
        let added: Vec<&String> = new_set.difference(&previous).collect();
        let removed: Vec<&String> = previous.difference(&new_set).collect();
        tracing::info!(
            pool = %pool_name, backend = %backend.name,
            added = ?added, removed = ?removed,
            total = new_set.len(),
            "advertised models updated"
        );
    }
    backend.set_models(new_set);

    ProbeOutcome::AliveWithModels
}

#[derive(Debug, Clone)]
enum ProbeOutcome {
    /// 200 + parseable body, models updated.
    AliveWithModels,
    /// 200 (non-parseable) or 401 — reachable but no new model data.
    AliveNoData,
    /// Network error, timeout, or non-2xx — counts toward FAILURE_THRESHOLD.
    /// Carries a precise, admin-readable reason for the log.
    Failed(String),
}

/// Turn an opaque reqwest transport error into a precise, admin-readable
/// reason. reqwest's `Display` is only "error sending request for url (…)";
/// the concrete cause (refused / reset / DNS / unreachable) lives down the
/// `source()` chain. Surface a human headline plus the full chain so a probe
/// log line is never ambiguous.
fn describe_transport_error(err: &reqwest::Error) -> String {
    let mut chain: Vec<String> = Vec::new();
    let mut io_kind: Option<std::io::ErrorKind> = None;
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        chain.push(e.to_string());
        if io_kind.is_none()
            && let Some(io) = e.downcast_ref::<std::io::Error>()
        {
            io_kind = Some(io.kind());
        }
        cur = std::error::Error::source(e);
    }

    let headline = if err.is_timeout() {
        "no response before the timeout".to_string()
    } else if let Some(kind) = io_kind {
        use std::io::ErrorKind;
        match kind {
            ErrorKind::ConnectionRefused => {
                "connection refused — nothing is listening at that host:port".into()
            }
            ErrorKind::ConnectionReset => {
                "connection reset by the upstream — it dropped the connection \
                 mid-request (commonly an idle keep-alive closed by the server \
                 or a firewall)"
                    .into()
            }
            ErrorKind::ConnectionAborted => "connection aborted by the upstream".into(),
            ErrorKind::TimedOut => "TCP timed out — host:port unreachable".into(),
            other => format!("I/O error: {other:?}"),
        }
    } else if err.is_connect() {
        "could not establish a TCP connection".into()
    } else {
        "transport failure before any HTTP response".into()
    };

    format!("{headline} (chain: {})", chain.join(" ⇐ "))
}

async fn run_probe(http: reqwest::Client, pool_name: String, backend: Arc<Backend>) {
    tracing::debug!(
        pool = %pool_name,
        backend = %backend.name,
        url = format!("{}{}", backend.base_url, backend.health_path),
        "starting health probe loop"
    );
    let mut consecutive_failures = 0u32;
    loop {
        match probe_once(&http, &pool_name, &backend).await {
            ProbeOutcome::AliveWithModels | ProbeOutcome::AliveNoData => {
                if !backend.is_healthy() {
                    tracing::info!(pool = %pool_name, backend = %backend.name, "backend recovered — healthy again");
                }
                backend.set_healthy(true);
                consecutive_failures = 0;
            }
            ProbeOutcome::Failed(reason) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                match (
                    backend.is_healthy(),
                    consecutive_failures >= FAILURE_THRESHOLD,
                ) {
                    // Crossed the threshold → a real outage. Exactly one WARN
                    // with the precise cause — the only probe-failure line an
                    // admin running at INFO ever sees.
                    (true, true) => {
                        tracing::warn!(
                            pool = %pool_name, backend = %backend.name,
                            failures = consecutive_failures,
                            "backend DOWN: {reason}"
                        );
                        backend.set_healthy(false);
                    }
                    // Still serving traffic — a single blip, not an outage.
                    // Quiet (DEBUG) and explicitly labelled so it can't be
                    // mistaken for a failure.
                    (true, false) => tracing::debug!(
                        pool = %pool_name, backend = %backend.name,
                        attempt = consecutive_failures, threshold = FAILURE_THRESHOLD,
                        "transient probe blip (backend still healthy): {reason}"
                    ),
                    // Already known-down; keep ongoing failures at DEBUG.
                    (false, _) => tracing::debug!(
                        pool = %pool_name, backend = %backend.name,
                        "backend still down: {reason}"
                    ),
                }
            }
        }
        sleep(PROBE_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn describe_transport_error_surfaces_concrete_cause() {
        // Bind then drop → a port guaranteed to refuse connections, so the
        // probe's reqwest call fails with a real transport error.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{addr}/v1/models");
        let err = reqwest::Client::new().get(&url).send().await.unwrap_err();

        let desc = describe_transport_error(&err);
        let lower = desc.to_lowercase();
        // Must name a concrete cause, not just the opaque reqwest top line.
        assert!(
            lower.contains("refused") || lower.contains("connect"),
            "expected a concrete connection cause, got: {desc}"
        );
        // And it must include the full source chain.
        assert!(desc.contains("chain:"), "missing source chain: {desc}");
    }
}
