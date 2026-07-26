// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The rama-flavoured shared handles the HTTP surface is built on.
//!
//! Everything here is needed by *both* the HTML pages (`gateway-web`) and the
//! routing glue (the `gateway` crate), so it sits below both: the
//! [`state::RamaState`] handle, the hand-rolled signed-cookie
//! [`session::SessionStore`], the `/v1` bearer middleware in [`auth`], and the
//! [`cors`] layer. The routes, the proxy, and the JSON API live in `gateway`.

pub mod cors;
pub mod session;

pub use session::SessionStore;
