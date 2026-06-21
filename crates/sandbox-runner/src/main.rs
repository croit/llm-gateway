// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `sandbox-runner` — a small service that executes untrusted / LLM-
//! generated code inside ephemeral, single-use sandboxes and returns
//! stdout/stderr plus any files the run produced.
//!
//! It is the privileged half of the gateway's sandbox tool: it holds
//! podman access and spawns the isolated sandboxes, while the gateway
//! stays unprivileged and only talks HTTP to it (see `docs/sandbox.md`).
//!
//! Run:
//! ```sh
//! SANDBOX_BIND=127.0.0.1:9000 cargo run --bin sandbox-runner
//! ```

mod backend;
mod config;
mod isolation;
mod pool;
mod server;

use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use rama::net::address::SocketAddress;

use crate::backend::{ContainerBackend, LocalBackend, PodmanBackend};
use crate::config::Config;
use crate::pool::Pool;
use crate::server::RunnerState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sandbox_runner=info")),
        )
        .init();

    let cfg = Arc::new(Config::parse());
    tracing::info!(
        image = %cfg.image, runtime = %cfg.runtime, pool_size = cfg.pool_size,
        max_concurrent = cfg.max_concurrent, egress = cfg.egress_available(),
        "sandbox-runner starting"
    );

    // `local-unsafe` runs code directly on the host (no container, no
    // isolation) so the feature is exercisable on a dev machine without
    // podman (e.g. macOS). Every other value drives podman.
    let backend: Arc<dyn ContainerBackend> = if cfg.runtime == "local-unsafe" {
        tracing::warn!(
            "SANDBOX_RUNTIME=local-unsafe — running code DIRECTLY ON THE HOST with NO \
             isolation. For local development only; NEVER use this in production."
        );
        Arc::new(LocalBackend::new().context("initialising local backend")?)
    } else {
        Arc::new(PodmanBackend::new(cfg.clone()))
    };
    let pool = Pool::new(backend, cfg.clone());

    // Warm the pool in the background so we start serving immediately;
    // `refresh_image` seeds the target image id, then fills. The first few
    // default-deny calls may pay cold-start until it fills.
    {
        let pool = pool.clone();
        tokio::spawn(async move { pool.refresh_image().await });
    }

    // Periodically re-resolve the workload image; on a rebuild / re-tag,
    // `refresh_image` drains the stale warm containers and re-warms on the
    // new image, so subsequent jobs pick it up without a manual restart.
    // `SANDBOX_IMAGE_CHECK_SECS=0` disables this.
    if cfg.image_check_secs > 0 {
        let pool = pool.clone();
        let every = std::time::Duration::from_secs(cfg.image_check_secs);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.tick().await; // immediate first tick — boot already seeded
            loop {
                tick.tick().await;
                pool.refresh_image().await;
            }
        });
    }

    // Verify the configured runtime actually isolates (catches a silently
    // non-isolating runtime — e.g. `--runtime` ignored over the socket).
    // Skip local-unsafe: it's intentionally not isolated and already warns.
    if cfg.runtime != "local-unsafe" {
        let pool = pool.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move { isolation::check(&pool, &cfg).await });
    }

    let sa: std::net::SocketAddr = cfg
        .bind
        .parse()
        .with_context(|| format!("SANDBOX_BIND `{}` is not host:port", cfg.bind))?;
    let addr = SocketAddress::new(sa.ip(), sa.port());
    tracing::info!(bind = %cfg.bind, "listening");

    let state = Arc::new(RunnerState { pool });
    server::serve(state, addr).await
}
