// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Rama-based HTTP routing — the gateway's request-dispatch surface.
//!
//! Reuses the database, OIDC client, upstream registry, RBAC resolver, and tool
//! registry from `gateway_core::server` unchanged — those modules are
//! framework-neutral — and mounts the HTML handlers from `gateway_web::pages`.
//! Only the routes themselves live here.
//!
//! The pieces both this crate and `gateway-web` need — [`RamaState`], the
//! signed-cookie session store, the `/v1` bearer middleware, and CORS — sit
//! below both in `gateway_core::rama_server`, and are re-exported here so the
//! router and the integration tests keep a single import path.

pub mod api;
pub mod comfyui_api;
pub mod first_run;
pub mod messages;
pub mod oidc_handlers;
pub mod proxy;
pub mod rag_api;
pub mod router;
pub mod sandbox_api;
pub mod vad;

pub use gateway_core::rama_server::{SessionStore, cors, session};
pub use gateway_runtime::rama_server::{RamaState, auth, state};
pub use gateway_web::pages;
pub use router::router;
