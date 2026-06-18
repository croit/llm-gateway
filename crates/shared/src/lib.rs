// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Types shared between the `gateway` server and the `gw` CLI / web UI.
//!
//! This crate is intentionally I/O-free — pure data types only. Everything
//! that crosses an API boundary lives here so the wire format stays in one
//! place.

pub mod api;

/// Healthcheck response body shape. Tiny, but having it here keeps the gateway
/// and CLI from drifting on string literals.
pub const HEALTHZ_BODY: &str = "ok";
