// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The gateway's server-rendered HTML presentation layer.
//!
//! Every browser-facing surface — the dashboard, `/chat`, `/tokens`, the
//! `/admin/*` screens — plus the datastar SSE patch handlers that keep them
//! live. Handlers take a
//! [`gateway_runtime::RamaState`](gateway_runtime::rama_server::state::RamaState) and
//! return rama responses carrying plait-rendered HTML.
//!
//! This crate is a pure sink: nothing in `gateway-core` references it, and only
//! the router in the `gateway` binary crate mounts it. That's deliberate — it
//! keeps a page edit to a leaf-crate rebuild. See `docs/architecture.md`.
//!
//! [`build_info`] (and the `build.rs` that feeds it the git SHA) lives here
//! rather than in `gateway-core` for the same reason: the page chrome is its
//! only consumer, so a new commit invalidates this crate and the binary but
//! leaves the much larger `gateway-core` cached.

pub mod build_info;
pub mod pages;
