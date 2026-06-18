// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Authentication primitives shared across the server: the OIDC client
//! (framework-neutral) and gateway token minting/hashing helpers.
//!
//! Session cookies + bearer middleware are framework-specific and live in
//! `crate::rama_server::session` + `crate::rama_server::auth`.

pub mod oidc;
pub mod token;

/// User identity resolved from a request's bearer token. Threaded into
/// handlers that need to know who's calling — currently the proxy path
/// (in-handler `require_bearer` in `rama_server::auth`).
#[derive(Debug, Clone)]
pub struct UserCtx {
    pub user_id: String,
    pub token_id: String,
    pub roles: Vec<String>,
}
