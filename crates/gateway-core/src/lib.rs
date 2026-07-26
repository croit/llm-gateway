// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The gateway's application body — everything below the HTML presentation
//! layer ([`gateway_web`](../gateway_web/index.html)) and the routing glue
//! (the `gateway` binary crate).
//!
//! - [`server`] — framework-neutral building blocks: config, DB, OIDC,
//!   upstream registry, RBAC, crypto, the tool registry + tools, and the
//!   feature subsystems (RAG, skills, ComfyUI, push, scheduled, …). None of
//!   this depends on a particular HTTP framework.
//! - [`rama_server`] — the rama-flavoured shared handles the HTTP surface is
//!   built on: [`rama_server::state::RamaState`], the signed-cookie
//!   [`rama_server::session::SessionStore`], the `/v1` bearer middleware, and
//!   CORS. The routes themselves live in the `gateway` crate.
//! - [`openai_driver`] — the [`session_core::SessionDriver`] implementation
//!   that drives a streaming OpenAI chat completion for the chat pages.
//!
//! The crate split exists for dev-build speed: a page or router edit must not
//! recompile this crate. See `docs/architecture.md`.

pub mod loop_guard;
pub mod openai_driver;
pub mod rama_server;
pub mod server;

pub use rama_server::state::RamaState;
pub use server::AppState;
pub use server::config::Config;
