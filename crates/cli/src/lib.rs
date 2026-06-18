// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `gw` CLI library — exposes the inner pieces so integration tests can drive
//! them without spawning a subprocess. The binary entry lives in `main.rs`.

pub mod client;
pub mod cmd;
pub mod credentials;
pub mod parser;
pub mod pkce;
