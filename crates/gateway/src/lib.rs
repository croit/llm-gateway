// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Gateway crate root.
//!
//! - [`server`] — framework-neutral building blocks: config, DB, OIDC,
//!   upstream registry, RBAC, tool registry. None of this depends on
//!   a particular HTTP framework.
//! - [`rama_server`] — rama-based HTTP surface that ties those building
//!   blocks together: proxy routes, OIDC handlers, session-authed
//!   token CRUD, server-rendered HTML pages (plait + daisyUI +
//!   datastar SSE patches).
//!
//! The crate produces one binary (`gateway`) defined in `main.rs`.

pub mod build_info;
pub mod loop_guard;
pub mod openai_driver;
pub mod rama_server;
pub mod server;
