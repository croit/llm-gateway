// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The gateway's optional subsystems — the things a deployment turns on in
//! `gateway.toml` and can run entirely without.
//!
//! RAG indexing, Agent Skills, ComfyUI workflows, Web Push, GeoIP, typst
//! template discovery, image generation, chat-attachment storage, embeddings,
//! speech, PDF reading, OCR, and the web-search settings store.
//!
//! Each one stands on `gateway-core` (config, DB, crypto) and knows nothing
//! about `AppState`, the tool registry, or HTTP routing — which is exactly why
//! this layer can sit below the runtime. Keep it that way: a reference from here
//! up into `gateway-runtime` collapses the split.

pub mod server;
