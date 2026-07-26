// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The gateway's tool implementations — one module per tool family.
//!
//! Each module holds `Tool` impls: the async functions the gateway exposes to
//! the model through the OpenAI `tools` array. The machinery they plug into —
//! the [`Tool`](gateway_runtime::server::tools::Tool) trait itself,
//! `ToolContext`, the `ToolRegistry`, the round-loop `runner`, the `catalog`
//! that groups ids into toggle keys, the MCP connection manager, and the
//! sandbox client — stays in `gateway-core`, because `AppState` holds those or
//! the chat driver needs them.
//!
//! Adding a tool means writing it here, registering it in the `ToolRegistry`
//! that `gateway`'s `main.rs` builds, and granting it to one or more roles in
//! `[rbac]`. We do **not** discover tools at runtime.
//!
//! This crate sits above `gateway-core` and beside `gateway-web`: only the
//! binary's wiring depends on it, so adding or editing a tool is a leaf-crate
//! rebuild. See `docs/architecture.md`.

pub mod currency;
pub mod document;
pub mod edit_image;
pub mod enable_tools;
pub mod fetch_attachment;
pub mod fetch_url;
pub mod generate_image;
pub mod html_text;
pub mod json_patch;
pub mod list_attachments;
pub mod load_image_url;
pub mod location;
pub mod lookup_ip;
pub mod memory;
pub mod netcheck;
pub mod qr;
pub mod rag;
pub mod read_skill;
pub mod search_web;
pub mod text_edit;
pub mod typst_render;
pub mod upload_attachment;
pub mod wikipedia;
