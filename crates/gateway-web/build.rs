// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Embeds the git commit the binary was built from into `GATEWAY_GIT_SHA`
//! so the running gateway can link network users to the exact corresponding
//! source — the source-offer obligation of the GNU AGPL-3.0 (§13).
//!
//! Resolution order: a CI-provided SHA (`CI_COMMIT_SHA` / `GIT_SHA`), else
//! `git rev-parse`, else `"unknown"` (e.g. a source tarball with no VCS).
//! Pure std — no build-dependencies.

use std::process::Command;

fn main() {
    let sha = std::env::var("CI_COMMIT_SHA")
        .or_else(|_| std::env::var("GIT_SHA"))
        .ok()
        .map(|s| s.chars().take(12).collect::<String>())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GATEWAY_GIT_SHA={sha}");
    // Rebuild when the commit could have changed.
    println!("cargo:rerun-if-env-changed=CI_COMMIT_SHA");
    println!("cargo:rerun-if-env-changed=GIT_SHA");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
