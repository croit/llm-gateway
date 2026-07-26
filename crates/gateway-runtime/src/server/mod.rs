// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Tool API, shared state, and the state-dependent background workers. Module
//! paths keep the historical `server::` prefix; see the crate docs.

pub mod comfyui_tool;
pub mod compaction;
pub mod headless;
pub mod scheduled;
pub mod state;
pub mod tools;
pub mod webhooks;

pub use state::AppState;
