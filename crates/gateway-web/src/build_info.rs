// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Build/version metadata surfaced in the UI.
//!
//! The gateway is licensed under the GNU AGPL-3.0, whose §13 requires anyone
//! running a *modified* version as a network service to offer that service's
//! users the corresponding source. To make that practical the UI carries a
//! persistent "Source" link pointing at the repository for the running build.

use std::sync::LazyLock;

/// Canonical public source repository.
const DEFAULT_SOURCE_URL: &str = "https://github.com/croit/llm-gateway";

/// Crate version (`CARGO_PKG_VERSION`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git commit the binary was built from, or `"unknown"`. Set by `build.rs`.
pub const GIT_SHA: &str = env!("GATEWAY_GIT_SHA");

/// The source URL the running build is offered from.
///
/// AGPL §13: if you fork and deploy modifications as a network service, set
/// `GATEWAY_SOURCE_URL` in the gateway's environment to *your* repository so
/// the in-app link points at the source actually running.
static SOURCE_URL: LazyLock<String> = LazyLock::new(|| {
    std::env::var("GATEWAY_SOURCE_URL").unwrap_or_else(|_| DEFAULT_SOURCE_URL.to_string())
});

/// The source URL to advertise (env override for forks, else the default).
pub fn source_url() -> &'static str {
    &SOURCE_URL
}

/// Human-readable build label, e.g. `v0.1.0 (a1b2c3d4e5f6)`.
pub fn version_label() -> String {
    format!("v{VERSION} ({GIT_SHA})")
}
