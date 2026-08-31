// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Single integration-test harness: every former `tests/<name>.rs` is a
//! module here, so the 36 test files link into ONE binary instead of 36.
//! (The live, env-gated `sandbox_e2e_live.rs` stays a separate binary.)
//! nextest still runs each #[test] in its own process, so tests that touch
//! process-global state (env vars) stay isolated.

mod admin_models;
mod admin_users;
mod ask_feedback;
mod chat_actions;
mod chat_attachment;
mod chat_document_edit;
mod chat_fork;
mod chat_pin;
mod chat_search;
mod chat_sharing;
mod chat_stream;
mod comfyui_integration;
mod comfyui_routes;
mod common;
mod cors;
mod healthz;
mod icon_smoke;
mod landing;
mod memory_page;
mod oidc_integration;
mod proxy;
mod push_routes;
mod pwa_routes;
mod rag;
mod rag_api;
mod rag_eval;
mod rag_extract;
mod rag_gdrive;
mod rag_incremental;
mod rag_page;
mod rag_profile;
mod rag_webdav;
mod rbac;
mod readme_routes;
mod scheduled;
mod session_routes;
mod sidebar_nav;
mod skills_user_page;
mod speech_voice;
mod tool_loop;
mod tool_prefs;
mod tools_inventory;
mod transcriptions;
mod typst_compile;
mod ui_chrome;
mod webhooks;
