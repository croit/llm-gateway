// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Headless ComfyUI integration.
//!
//! ComfyUI runs as a separate, internal-only worker that owns the GPU and
//! the model files. The gateway owns the **workflow catalog** (a private
//! content directory the operator curates) and exposes each curated
//! workflow as a typed tool the model can call.
//!
//! Architecture, operator config, and the manifest format are documented
//! in `docs/comfyui.md`. This module is the runtime:
//!
//! - [`manifest`] parses + validates each workflow's `manifest.toml`;
//! - [`store`] is the hot-reloadable catalog (admin can trigger a rescan
//!   without restarting the gateway);
//! - [`jobs`] tracks async workflow submissions in the DB;
//! - [`scheduler`] is the background worker that polls ComfyUI for
//!   completion and re-hosts the result as a chat attachment;
//! - the tool wrapper and `ToolSource` live in `tool` and surface each
//!   loaded manifest to the model as `comfyui_<id>`.

pub mod client;
pub mod jobs;
pub mod manifest;
pub mod runner;
pub mod scheduler;
pub mod store;
pub mod tool;

pub use client::{Client, ComfyuiClientError, DownloadedAsset, ProducedAsset, StatusCheck};
pub use jobs::ComfyuiJob;
pub use manifest::{
    ArgError, ManifestError, OutputKind, Param, ParamSchema, ParamType, WorkflowManifest,
};
pub use runner::{RunError, RunOutcome, Runner};
pub use scheduler::ComfyuiScheduler;
pub use store::{ComfyuiStore, ReloadReport, ReloadSkip, Snapshot};
pub use tool::{ComfyuiHandle, ComfyuiToolSource, ComfyuiWorkflowTool};

/// Broadcast handles for chat sessions with a pending ComfyUI job. The
/// scheduler uses these handles to wake the live chat stream after it appends
/// the completed asset marker.
#[derive(Clone, Default)]
pub struct ChatUpdateRegistry(
    std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                tokio::sync::broadcast::Sender<session_core::workers::TurnUpdate>,
            >,
        >,
    >,
);

impl ChatUpdateRegistry {
    pub fn register(
        &self,
        session_id: impl Into<String>,
        sender: tokio::sync::broadcast::Sender<session_core::workers::TurnUpdate>,
    ) {
        self.0
            .lock()
            .expect("chat update registry lock poisoned")
            .insert(session_id.into(), sender);
    }

    pub fn notify(&self, session_id: &str) {
        let mut map = self.0.lock().expect("chat update registry lock poisoned");
        if let Some(sender) = map.get(session_id) {
            let _ = sender.send(session_core::workers::TurnUpdate::SidebarChanged);
        }
        // Prune senders whose live chat stream has gone away (no receivers
        // left). Without this the map grows by one entry per session that
        // ever ran a job, for the whole process lifetime. Live sessions —
        // including a second concurrent job on this same session — keep a
        // receiver, so they survive the sweep.
        map.retain(|_, sender| sender.receiver_count() > 0);
    }
}
