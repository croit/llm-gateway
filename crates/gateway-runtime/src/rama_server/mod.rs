// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The rama-flavoured handles that need `AppState`: the `RamaState` the whole
//! HTTP surface is built on, and the `/v1` bearer middleware. The session store
//! and CORS layer stay in `gateway-core` — they don't need state.

pub mod auth;
pub mod state;

pub use state::RamaState;
