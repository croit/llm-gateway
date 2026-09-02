// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Framework-neutral server building blocks.
//!
//! These modules don't depend on a particular HTTP framework — they're
//! consumed by `rama_server` for I/O, but they could just as well be
//! reused by tests, a CLI tool, or a future second binding.

pub mod auth;
pub mod capabilities;
pub mod config;
pub mod crypto;
pub mod db;
pub mod feature_defaults;
pub mod limits;
pub mod model_defaults;
pub mod oidc_settings;
pub mod rbac;
pub mod reasoning;
pub mod settings;
pub mod setup;
pub mod sse;
pub mod tool_naming;
pub mod upstreams;
pub mod usage;

pub use config::Config;
