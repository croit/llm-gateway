// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Role-based access control.
//!
//! Two concerns:
//!
//! 1. **Mapping** — translate the raw OIDC claim values stored on `User.roles`
//!    into internal role IDs (e.g. `"ad-group-eng-team-7"` → `"engineering"`).
//!    Configured by `[[rbac.mapping]]` rows. A `default_role` (if set)
//!    applies to every authenticated user regardless of claims.
//!
//! 2. **Permission** — for a set of internal role IDs, derive the set of
//!    tools the user can call and the set of model patterns they can route
//!    to. Configured by `[[roles]]`. The wildcard `"*"` is supported on
//!    tools (expands to "all registered tools") and on models.
//!
//! The resolver is built once at startup and held in `AppState` as an `Arc`.

pub mod config;
pub mod resolver;

pub use config::{RbacConfig, RoleConfig, RoleMapping};
pub use resolver::{ResolveError, Resolver};
