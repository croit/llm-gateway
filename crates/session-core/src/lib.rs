// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `session-core` — shared scaffolding for the chat-style "session"
//! surface the gateway is built on.
//!
//! The session surface persists a conversation as a stream of turns,
//! broadcasts deltas to subscribed HTTP clients, and lets one user
//! have at most one producing worker at a time. The pieces live here
//! (instead of in the gateway crate) so a future second consumer can
//! drop in a different `SessionDriver` and paint the same bubbles
//! without forking.
//!
//! Modules:
//! - `workers` — the per-user worker registry.
//! - `driver` — the `SessionDriver` trait + `SessionContext` value
//!   that decouples "drive one turn" from "where the deltas come
//!   from".
//! - `chat` / `render` / `chrome` / `assets` — the chat page
//!   renderer, SSE primitives, and the bundled CSS/JS.
//! - `db` — sessions + turns + tool calls.

pub mod assets;
pub mod attachments;
pub mod chat;
pub mod chrome;
pub mod db;
pub mod driver;
pub mod export;
pub mod icons;
pub mod render;
pub mod worker;
pub mod workers;

pub use driver::{SessionContext, SessionDriver, TurnError};
pub use workers::{ActiveWorker, RegisterOutcome, SessionWorkers, TurnUpdate};
