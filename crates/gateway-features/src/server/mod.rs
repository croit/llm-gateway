// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The optional subsystems. Module paths keep the historical `server::` prefix
//! so the split didn't have to churn every call site; see the crate docs.

pub mod chat_attachments;
pub mod comfyui;
pub mod document_canvas;
pub mod embeddings;
pub mod geoip;
pub mod github;
pub mod image_gen;
pub mod ocr;
pub mod pdf;
pub mod push;
pub mod rag;
pub mod search_settings;
pub mod skills;
pub mod speech;
pub mod typst;
