// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Framework-neutral server building blocks.
//!
//! These modules don't depend on a particular HTTP framework — they're
//! consumed by `rama_server` for I/O, but they could just as well be
//! reused by tests, a CLI tool, or a future second binding.

pub mod auth;
pub mod chat_attachments;
pub mod config;
pub mod db;
pub mod embeddings;
pub mod geoip;
pub mod model_defaults;
pub mod pdf;
pub mod rag;
pub mod rbac;
pub mod scheduled;
pub mod skills;
pub mod state;
pub mod tools;
pub mod typst;
pub mod upstreams;
pub mod usage;

pub use config::Config;
pub use state::AppState;
