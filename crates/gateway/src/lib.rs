// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Gateway crate root — the routing glue and the binary.
//!
//! [`rama_server`] is the crate's whole content: the rama router, the `/v1`
//! proxy, the session-authed `/api/v0` JSON surface, the OIDC handlers, and the
//! audio pre-processing they need. It mounts handlers from the two crates below
//! it and owns nothing else:
//!
//! - [`gateway_core`] — the application body: config, DB, crypto, RBAC, the
//!   upstream registry, the tool registry + tools, the feature subsystems, and
//!   the shared [`gateway_runtime::RamaState`] handle.
//! - [`gateway_web`] — the server-rendered HTML pages and their datastar SSE
//!   patch handlers.
//!
//! Keeping this crate thin is the point of the split: a page edit rebuilds
//! `gateway-web` and this crate, never `gateway-core`. See
//! `docs/architecture.md`.
//!
//! The crate produces one binary (`gateway`) defined in `main.rs`. The lib
//! target exists so the integration tests in `tests/` can build the router and
//! drive it with `router.serve(req)` without binding a socket.

pub mod rama_server;
pub mod tool_families;
