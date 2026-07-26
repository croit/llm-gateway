// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Authentication primitives shared across the server: the OIDC client
//! (framework-neutral) and gateway token minting/hashing helpers.
//!
//! Session cookies + bearer middleware are framework-specific and live in
//! `crate::rama_server::session` + `crate::rama_server::auth`.

pub mod mcp_oauth;
pub mod oidc;
pub mod token;

/// User identity resolved from a request's bearer token. Threaded into
/// handlers that need to know who's calling — currently the proxy path
/// (in-handler `require_bearer` in `rama_server::auth`).
#[derive(Debug, Clone)]
pub struct UserCtx {
    pub user_id: String,
    /// Resolved during auth (the user row is loaded anyway). Denormalised
    /// onto usage rows so the metrics page needs no join and survives a
    /// user deletion. Empty when unknown.
    pub user_email: String,
    pub token_id: String,
    /// The token's display name, resolved during auth. Denormalised onto
    /// usage rows for the per-token breakdown.
    pub token_name: String,
    pub roles: Vec<String>,
    /// The token's master "tool use" switch. When `false` the request
    /// path injects no gateway tools at all (pure passthrough), so the
    /// per-capability `token_tool_prefs` never come into play. Default
    /// for every token is off — see `RamaState::allowed_tools_for_token`.
    pub tools_enabled: bool,
}
