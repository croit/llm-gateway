// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Rama-based HTTP server — the gateway's entire I/O surface.
//!
//! Reuses the database, OIDC client, upstream registry, RBAC resolver, and
//! tool registry from `crate::server` unchanged — those modules are
//! framework-neutral. Only the I/O surface (HTTP handlers, middleware, body
//! extractors) lives here, built on rama.

pub mod api;
pub mod auth;
pub mod cli_handlers;
pub mod oidc_handlers;
pub mod pages;
pub mod proxy;
pub mod rag_api;
pub mod router;
pub mod sandbox_api;
pub mod session;
pub mod state;
pub mod vad;

pub use router::router;
pub use session::SessionStore;
pub use state::RamaState;
