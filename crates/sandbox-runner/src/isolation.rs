// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Boot-time isolation self-check.
//!
//! A misconfigured or unsupported runtime can silently fail to apply a real
//! boundary — e.g. `--runtime` not honored over a remote podman socket
//! falls back to the default runtime, which shares the host kernel and gives
//! NO isolation. This module runs one probe sandbox at startup and compares
//! its kernel to the host's: a match means the sandbox shares the host kernel
//! (NOT isolated), and we log a loud warning so the operator notices instead
//! of trusting a boundary that isn't there.

use std::sync::Arc;

use shared::sandbox::{Language, RunRequest};

use crate::config::Config;
use crate::pool::Pool;

/// What the kernel comparison tells us about isolation.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Guest kernel differs from host → a separate-kernel boundary applied.
    Isolated,
    /// Guest kernel == host kernel → the sandbox shares the host kernel.
    NotIsolated,
    /// Couldn't determine (empty/missing kernel string).
    Inconclusive,
}

/// Pure comparison so it's unit-testable without spawning anything.
pub fn verdict(host_kernel: &str, guest_kernel: &str) -> Verdict {
    let h = host_kernel.trim();
    let g = guest_kernel.trim();
    if g.is_empty() || h.is_empty() {
        Verdict::Inconclusive
    } else if g == h {
        Verdict::NotIsolated
    } else {
        Verdict::Isolated
    }
}

/// Read the host kernel release (the runner process shares it). `None` off
/// Linux (e.g. macOS dev), where the check is skipped.
fn host_kernel() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().to_string())
}

/// Run one probe sandbox and log whether the runtime actually isolates.
/// Best-effort: never blocks or fails startup.
pub async fn check(pool: &Arc<Pool>, cfg: &Config) {
    let Some(host) = host_kernel() else {
        tracing::info!("isolation self-check skipped (no /proc/sys/kernel/osrelease — not Linux)");
        return;
    };
    let req = RunRequest {
        language: Language::Bash,
        code: "cat /proc/sys/kernel/osrelease 2>/dev/null || uname -r".into(),
        files: Vec::new(),
        timeout_secs: Some(60),
        network: false,
    };
    let guest = match pool.run(&req).await {
        Ok(resp) => resp.stdout,
        Err(e) => {
            tracing::warn!(error = %e, "isolation self-check could not run a probe sandbox");
            return;
        }
    };
    match verdict(&host, &guest) {
        Verdict::Isolated => tracing::info!(
            runtime = %cfg.runtime, host_kernel = %host, guest_kernel = %guest.trim(),
            "isolation confirmed: the sandbox runs a separate kernel from the host"
        ),
        Verdict::NotIsolated => tracing::warn!(
            runtime = %cfg.runtime, host_kernel = %host,
            "SANDBOX IS NOT ISOLATED — it shares the host kernel. Runtime '{}' did not apply a \
             separate-kernel (gVisor) boundary, so untrusted code is NOT contained. \
             Verify the runtime is installed and registered with podman (e.g. `runsc --version` \
             and a `runsc` entry in /etc/containers/containers.conf).",
            cfg.runtime
        ),
        Verdict::Inconclusive => {
            tracing::warn!("isolation self-check inconclusive (probe returned no kernel string)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn differing_kernels_means_isolated() {
        assert_eq!(verdict("6.12.0-amd64", "6.1.0-gvisor\n"), Verdict::Isolated);
    }

    #[test]
    fn same_kernel_means_not_isolated() {
        // The silent-failure case: runtime didn't apply, guest == host.
        assert_eq!(
            verdict("6.12.0-amd64", "6.12.0-amd64\n"),
            Verdict::NotIsolated
        );
    }

    #[test]
    fn empty_is_inconclusive() {
        assert_eq!(verdict("6.12.0", ""), Verdict::Inconclusive);
        assert_eq!(verdict("", "6.1.0"), Verdict::Inconclusive);
    }
}
