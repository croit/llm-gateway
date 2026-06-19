// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Multi-provider routing.
//!
//! Stack:
//!
//! - **Config** (`config.rs`): the typed `[upstream_pools]` map. No
//!   `[[models]]` routing table — routes are discovered at runtime from
//!   each backend's own `/models` response (see Health below).
//! - **Runtime** (`registry.rs`): the `UpstreamRegistry` owns `Pool`s; each
//!   backend tracks the set of model IDs it currently advertises.
//!   `acquire_for(model, kind)` walks pools matching the kind, finds the
//!   first one whose backends serve `model`, picks one of those backends
//!   via the pool's strategy, and returns an `Acquired` RAII guard that
//!   releases the in-flight slot on drop.
//! - **Health** (`health.rs`): one background task per backend, hitting
//!   `<base_url>/models`. On every successful probe the response is
//!   parsed as the OpenAI envelope (`{"data": [{"id": ...}]}`) and the
//!   backend's advertised-model set is replaced. Three consecutive
//!   failures mark unhealthy; one success flips back. `spawn` blocks on
//!   an initial parallel probe round so the first request finds populated
//!   sets.
//!
//! See `docs/upstreams.md` for the wire/config shape and rationale.

pub mod config;
pub mod health;
pub mod registry;

pub use config::{BackendConfig, Compliance, PickerStrategy, PoolKind, UpstreamPoolConfig};
pub use registry::{AcquireError, Acquired, Backend, Pool, RouteError, UpstreamRegistry};
