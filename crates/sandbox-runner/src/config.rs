// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Runner configuration, read from the environment (with flag overrides).
//!
//! Every knob has a safe, security-first default: no network, a fresh
//! single-use sandbox per job, conservative resource caps, and a bounded
//! output size. Operators tune them via the `sandbox-runner.container`
//! Quadlet's `Environment=` lines.

use std::time::Duration;

use clap::Parser;

/// `sandbox-runner` — executes untrusted/LLM-generated code in ephemeral,
/// single-use sandboxes and returns stdout/stderr + produced files.
#[derive(Debug, Clone, Parser)]
#[command(name = "sandbox-runner", version)]
pub struct Config {
    /// Address to bind the HTTP API to. Keep this on an internal network
    /// — the API runs arbitrary code and must never be publicly reachable.
    #[arg(long, env = "SANDBOX_BIND", default_value = "127.0.0.1:9000")]
    pub bind: String,

    /// OCI image holding the workload toolchain (python, LibreOffice,
    /// pandoc, typst, ffmpeg, chromium, …) plus `/usr/local/bin/sandbox-agent`.
    #[arg(
        long,
        env = "SANDBOX_IMAGE",
        default_value = "ghcr.io/croit/llm-gateway-sandbox:latest"
    )]
    pub image: String,

    /// OCI runtime to execute each sandbox under. Defaults to `runsc` (gVisor)
    /// — a real OCI runtime podman drives directly, with its own userspace
    /// kernel (a strong untrusted-code boundary). `crun`/`runc` are for
    /// trusted local testing only (they share the host kernel — no isolation).
    /// The special value `local-unsafe` bypasses the container runtime
    /// entirely and runs code on the host — dev only.
    #[arg(long, env = "SANDBOX_RUNTIME", default_value = "runsc")]
    pub runtime: String,

    /// `podman` binary to drive. A remote client (`CONTAINER_HOST` set to
    /// the host socket) works too — this is just the executable name.
    #[arg(long, env = "SANDBOX_PODMAN", default_value = "podman")]
    pub podman: String,

    /// How many pre-booted, idle sandboxes to keep warm so a tool call
    /// doesn't pay cold-start latency. Each is used at most once.
    #[arg(long, env = "SANDBOX_POOL_SIZE", default_value_t = 3)]
    pub pool_size: usize,

    /// Hard ceiling on concurrent in-flight jobs (warm + on-demand).
    /// Bounds host resource use under load.
    #[arg(long, env = "SANDBOX_MAX_CONCURRENT", default_value_t = 6)]
    pub max_concurrent: usize,

    /// How often (seconds) to re-resolve the workload image's id and, if it
    /// changed (a rebuild / re-tag), drain and rebuild the warm pool so the
    /// next jobs run the new image — no manual `systemctl restart` needed.
    /// `0` disables the check (pool only ever reflects the image present at
    /// boot). The warm pool is always single-use, so this only affects which
    /// image freshly-booted containers use.
    #[arg(long, env = "SANDBOX_IMAGE_CHECK_SECS", default_value_t = 60)]
    pub image_check_secs: u64,

    /// Default per-job wall-clock budget when the caller doesn't specify.
    #[arg(long, env = "SANDBOX_TIMEOUT_SECS", default_value_t = 60)]
    pub default_timeout_secs: u64,

    /// Upper bound the caller's requested timeout is clamped to.
    #[arg(long, env = "SANDBOX_MAX_TIMEOUT_SECS", default_value_t = 300)]
    pub max_timeout_secs: u64,

    /// Memory cap per sandbox (passed to `podman --memory`).
    #[arg(long, env = "SANDBOX_MEMORY", default_value = "1024m")]
    pub memory: String,

    /// CPU cap per sandbox (passed to `podman --cpus`).
    #[arg(long, env = "SANDBOX_CPUS", default_value = "2")]
    pub cpus: String,

    /// Process-count cap per sandbox (passed to `podman --pids-limit`).
    #[arg(long, env = "SANDBOX_PIDS_LIMIT", default_value_t = 256)]
    pub pids_limit: i64,

    /// Size of the writable `/work` scratch tmpfs (the job's CWD). It is
    /// RAM-backed and charged to the `--memory` cgroup, so
    /// `work_size + tmp_size` plus the job's own RAM must fit under
    /// `SANDBOX_MEMORY`. Raise all three together for large-file work like
    /// video transcoding.
    #[arg(long, env = "SANDBOX_WORK_SIZE", default_value = "512m")]
    pub work_size: String,

    /// Size of the writable `/tmp` tmpfs (exec-mounted; chromium and
    /// LibreOffice drop helper binaries here). Same RAM-backed caveat as
    /// [`Self::work_size`].
    #[arg(long, env = "SANDBOX_TMP_SIZE", default_value = "512m")]
    pub tmp_size: String,

    /// Truncate combined stdout/stderr to this many bytes in the response.
    /// Kept context-safe by default (128 KiB ≈ 30–40k tokens): stdout is fed
    /// straight back to the model, and ~1 MiB of text alone can overflow a
    /// model's context window. Raise it for jobs that must return more, but
    /// prefer having the code print a summary, not raw dumps.
    #[arg(long, env = "SANDBOX_MAX_OUTPUT_BYTES", default_value_t = 131_072)]
    pub max_output_bytes: usize,

    /// Podman network to attach a sandbox to when a call requests (and is
    /// granted) egress. This network must route only to the egress proxy.
    /// Empty disables network for every call regardless of the request.
    #[arg(long, env = "SANDBOX_EGRESS_NETWORK", default_value = "")]
    pub egress_network: String,

    /// `HTTP(S)_PROXY` value injected into network-enabled sandboxes so
    /// pip / browsers go through the allowlisting egress proxy. Empty →
    /// no proxy env set (and, with no egress network, no network at all).
    #[arg(long, env = "SANDBOX_EGRESS_PROXY", default_value = "")]
    pub egress_proxy: String,
}

impl Config {
    /// Whether the deployment is wired for any egress at all.
    pub fn egress_available(&self) -> bool {
        !self.egress_network.is_empty()
    }

    /// Resolve the effective timeout for a request: caller value (if any)
    /// clamped to `[1, max_timeout_secs]`, else the default.
    pub fn effective_timeout(&self, requested: Option<u64>) -> Duration {
        let secs = requested
            .unwrap_or(self.default_timeout_secs)
            .clamp(1, self.max_timeout_secs.max(1));
        Duration::from_secs(secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        Config {
            bind: "127.0.0.1:9000".into(),
            image: "img".into(),
            runtime: "runsc".into(),
            podman: "podman".into(),
            pool_size: 3,
            max_concurrent: 6,
            image_check_secs: 60,
            default_timeout_secs: 60,
            max_timeout_secs: 300,
            memory: "1024m".into(),
            cpus: "2".into(),
            pids_limit: 256,
            work_size: "512m".into(),
            tmp_size: "512m".into(),
            max_output_bytes: 1_048_576,
            egress_network: String::new(),
            egress_proxy: String::new(),
        }
    }

    #[test]
    fn timeout_clamps_to_max_and_floor() {
        let c = base();
        assert_eq!(c.effective_timeout(None), Duration::from_secs(60));
        assert_eq!(c.effective_timeout(Some(10)), Duration::from_secs(10));
        assert_eq!(c.effective_timeout(Some(99_999)), Duration::from_secs(300));
        assert_eq!(c.effective_timeout(Some(0)), Duration::from_secs(1));
    }

    #[test]
    fn egress_off_by_default() {
        assert!(!base().egress_available());
    }
}
