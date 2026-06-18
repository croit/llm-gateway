// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RbacConfig {
    /// Role applied to every authenticated user before mapping. Use this for
    /// a baseline "logged-in user" tier. Optional — when unset, users with
    /// no matching mapping get no role IDs.
    pub default_role: Option<String>,
    /// Maps an OIDC claim value to an internal role ID. Multiple mappings can
    /// point at the same role.
    #[serde(default, rename = "mapping")]
    pub mappings: Vec<RoleMapping>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleMapping {
    /// Documents which OIDC claim the value came from. Today we match by
    /// `oidc_value` across the user's whole `roles` list, so this is
    /// informational; if we later want per-claim disambiguation, this is
    /// where it lives.
    pub oidc_claim: String,
    pub oidc_value: String,
    pub role: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RoleConfig {
    pub id: String,
    /// Model patterns this role can route to. Each entry is either an exact
    /// model name or `"*"` (everything). `"name*"` prefix matches are also
    /// supported.
    #[serde(default)]
    pub models: Vec<String>,
    /// Tool IDs this role grants. `"*"` expands to every registered tool.
    #[serde(default)]
    pub tools: Vec<String>,
}
