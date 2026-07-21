// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Tool wrapper that turns each loaded workflow manifest into a
//! `comfyui_<id>` tool the model can call, plus the [`ComfyuiToolSource`]
//! the runner composes with the static [`ToolRegistry`] (mirrors the MCP
//! per-request overlay pattern).
//!
//! The wrapper is **stateless**: it holds only the `ComfyuiStore` (the
//! hot-reloadable catalog) and the tool id, and re-reads the current
//! snapshot on every `schema()` / `run()` call. A [`ComfyuiStore::reload`]
//! therefore propagates immediately — no tool re-registration, no gateway
//! restart.

use std::sync::Arc;

use serde_json::{Value, json};
use shared::api::ToolDef;

use super::ChatUpdateRegistry;
use super::client::Client;
use super::manifest::WorkflowManifest;
use super::runner::{RunError, Runner};
use super::store::ComfyuiStore;
use crate::server::tools::registry::ToolSource;
use crate::server::tools::{Tool, ToolContext, ToolError, ToolFuture};

/// Cheaply-cloneable bundle of everything a [`ComfyuiWorkflowTool`] needs
/// to execute a workflow. Built once at startup from `[comfyui]` config;
/// held in `Arc` by the store and the tool source.
#[derive(Clone)]
pub struct ComfyuiHandle {
    pub store: Arc<ComfyuiStore>,
    pub client: Client,
    pub runner_poll_interval: std::time::Duration,
    pub runner_timeout: std::time::Duration,
    /// Chat-attachment S3 config. When `Some`, attachment-kind params
    /// (Image/Video/AudioAttachment) are resolved from the gateway's
    /// chat bucket and staged into ComfyUI's input store before
    /// workflow injection. `None` only when `[chat.s3]` is unconfigured
    /// — workflows with attachment-kind params then fail cleanly with
    /// `RunError::S3NotConfigured`.
    pub s3: Option<Arc<crate::server::config::S3Config>>,
    /// Maximum number of concurrently pending ComfyUI jobs. The tool
    /// rejects new submissions when the pending count reaches this
    /// limit, so a single 24 GB GPU isn't flooded with overlapping
    /// diffusion runs. Mirrors `[comfyui] max_concurrent_jobs`.
    pub max_concurrent_jobs: usize,
    pub chat_updates: ChatUpdateRegistry,
}

impl std::fmt::Debug for ComfyuiHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComfyuiHandle")
            .field("base_url", &self.client.base_url())
            .field(
                "poll_interval_ms",
                &(self.runner_poll_interval.as_millis() as u64),
            )
            .field("timeout_secs", &self.runner_timeout.as_secs())
            .field("max_concurrent_jobs", &self.max_concurrent_jobs)
            .finish_non_exhaustive()
    }
}

/// One workflow exposed as one tool. The tool's `id()` returns
/// `comfyui_<manifest.id>` (e.g. `comfyui_text_to_image`); the schema and
/// the runner are looked up live from the store, so a hot-reload of the
/// catalog takes effect on the next `schema()`/`run()` call without the
/// tool itself being rebuilt.
pub struct ComfyuiWorkflowTool {
    /// The tool id, prefixed `comfyui_`. Stored as a `String` (not
    /// `&'static str`) because tools are constructed at runtime from
    /// manifest data; the `Tool` trait's `&str` return is satisfied by
    /// borrowing through `&self`.
    id: String,
    /// Manifest `id` (no prefix) — used for catalog lookups so we don't
    /// have to strip the prefix on every call.
    manifest_id: String,
    handle: ComfyuiHandle,
}

impl ComfyuiWorkflowTool {
    pub fn new(manifest_id: String, handle: ComfyuiHandle) -> Self {
        Self {
            id: format!("comfyui_{manifest_id}"),
            manifest_id,
            handle,
        }
    }

    fn lookup_manifest(&self) -> Result<Arc<WorkflowManifest>, ToolError> {
        self.handle
            .store
            .current()
            .lookup(&self.manifest_id)
            .ok_or_else(|| {
                ToolError::Failed(format!(
                    "workflow `{}` is no longer in the catalog; run a ComfyUI reload",
                    self.manifest_id
                ))
            })
    }

    fn runner(&self) -> Runner {
        let mut runner = Runner::new(
            self.handle.client.clone(),
            self.handle.runner_poll_interval,
            self.handle.runner_timeout,
        );
        if let Some(s3) = self.handle.s3.clone() {
            runner = runner.with_s3(s3);
        }
        runner
    }
}

impl Tool for ComfyuiWorkflowTool {
    fn id(&self) -> &str {
        &self.id
    }

    fn schema(&self) -> ToolDef {
        let manifest = match self.handle.store.current().lookup(&self.manifest_id) {
            Some(m) => m,
            None => {
                return ToolDef::function(
                    self.id(),
                    "This workflow is no longer in the catalog; \
                     run a ComfyUI reload to refresh the available tools.",
                    json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false,
                    }),
                );
            }
        };
        let required = manifest.required_keys();
        ToolDef::function(
            self.id(),
            manifest.description.clone(),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": required,
                "properties": manifest.tool_properties(),
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let manifest = self.lookup_manifest()?;

            // ComfyUI tools need a chat session (turn to attach output to)
            // and S3 (to re-host the produced asset). Check both up front.
            let _s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "chat attachments are not configured on this gateway \
                     (operator must set [chat.s3]) — nowhere to store the output"
                        .into(),
                )
            })?;
            let session_id = ctx.session_id.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "ComfyUI tools are only available inside a chat session — \
                     there's no conversation to attach the output to"
                        .into(),
                )
            })?;
            let turn_id = ctx.assistant_turn_id.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "ComfyUI tools are only available inside a chat session — \
                     there's no assistant turn to attach the output to"
                        .into(),
                )
            })?;
            let _reservations = ctx.attachment_reservations.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "ComfyUI tools require a per-turn attachment-reservation \
                     set, which is only initialised on the chat-page path"
                        .into(),
                )
            })?;

            // Enforce the operator's concurrency limit before doing any
            // work. A single 24 GB GPU can only run one diffusion job at
            // a time; queuing more just floods ComfyUI's internal queue
            // and increases latency for everyone.
            let pending = super::jobs::pending_count(&ctx.db).await.unwrap_or(0);
            if pending >= self.handle.max_concurrent_jobs as i64 {
                return Err(ToolError::Failed(format!(
                    "ComfyUI is at its job limit ({} pending of {} max). \
                     Please wait for an ongoing generation to finish before \
                     starting a new one.",
                    pending, self.handle.max_concurrent_jobs
                )));
            }

            // Phase 1: resolve args, upload attachments, submit to ComfyUI.
            // Returns immediately with a prompt_id — does NOT wait for the
            // workflow to finish. The scheduler handles completion later.
            let runner = self.runner();
            let prompt_id = runner.prepare_and_submit(&manifest, &args).await.map_err(|e| match e {
                RunError::InvalidArgs(arg) => ToolError::InvalidArgs(format!("{arg}")),
                RunError::Client(source) => {
                    tracing::warn!(error = %source, workflow = %manifest.id, "ComfyUI submit failed");
                    ToolError::Failed(format!("ComfyUI submit failed: {source}"))
                }
                other => ToolError::Failed(format!("{other}")),
            })?;

            // Store the job in the DB so the scheduler can poll for
            // completion and re-host the result as a chat attachment.
            let job_id = super::jobs::create(
                &ctx.db,
                &prompt_id,
                session_id,
                turn_id,
                &ctx.user_id,
                &manifest.id,
                &manifest.output_kind.to_string(),
                &manifest.output_node_id,
                &manifest.output_filename_prefix,
            )
            .await
            .map_err(|e| ToolError::Failed(format!("persist comfyui job: {e}")))?;

            if let Some(feedback) = ctx.chat_feedback.as_ref() {
                self.handle
                    .chat_updates
                    .register(session_id.to_string(), feedback.broadcast.clone());
            }

            tracing::info!(
                job_id,
                prompt_id = %prompt_id,
                workflow = %manifest.id,
                "ComfyUI job submitted — scheduler will handle completion",
            );

            // Return immediately. The LLM tells the user "generation started";
            // the result appears as an attachment when the scheduler completes.
            Ok(json!({
                "status": "started",
                "job_id": job_id,
                "prompt_id": prompt_id,
                "workflow": manifest.id,
                "note": "The generation is running in the background. The result \
                         will appear as an attachment in your message bubble \
                         when it finishes — usually within 30 seconds to a few \
                         minutes depending on the model. Do NOT tell the user \
                         the image is ready yet; wait for it to appear."
            }))
        })
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        // The tool only does Phase 1 (submit), which is a single HTTP POST
        // to ComfyUI + attachment uploads. It should be fast (< 30s even
        // with large image uploads). The scheduler handles the long wait.
        Some(std::time::Duration::from_secs(60))
    }
}

/// Per-request tool source for the dynamic ComfyUI catalog. Built once
/// per gateway process from the shared `ComfyuiHandle`; [`ToolSource`]
/// methods read the current snapshot, so hot-reload just works.
pub struct ComfyuiToolSource {
    handle: ComfyuiHandle,
}

impl ComfyuiToolSource {
    pub fn new(handle: ComfyuiHandle) -> Self {
        Self { handle }
    }

    pub fn handle(&self) -> &ComfyuiHandle {
        &self.handle
    }

    fn build_tool(&self, manifest_id: &str) -> Arc<dyn Tool> {
        Arc::new(ComfyuiWorkflowTool::new(
            manifest_id.to_string(),
            self.handle.clone(),
        ))
    }
}

impl ToolSource for ComfyuiToolSource {
    fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        let tool_id = id.strip_prefix("comfyui_")?;
        if self.handle.store.current().lookup(tool_id).is_some() {
            Some(self.build_tool(tool_id))
        } else {
            None
        }
    }

    fn defs_for(&self, allowed: &[String]) -> Vec<ToolDef> {
        let snapshot = self.handle.store.current();
        let mut out = Vec::new();
        for id in allowed {
            let Some(manifest_id) = id.strip_prefix("comfyui_") else {
                continue;
            };
            if let Some(manifest) = snapshot.lookup(manifest_id) {
                let required = manifest.required_keys();
                out.push(ToolDef::function(
                    id,
                    manifest.description.clone(),
                    json!({
                        "type": "object",
                        "additionalProperties": false,
                        "required": required,
                        "properties": manifest.tool_properties(),
                    }),
                ));
            }
        }
        out
    }

    fn ids(&self) -> Vec<String> {
        self.handle
            .store
            .current()
            .workflows()
            .into_iter()
            .map(|m| format!("comfyui_{}", m.id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::comfyui::manifest::{
        OutputKind, Param, ParamSchema, ParamType, WorkflowManifest,
    };
    use std::path::PathBuf;

    fn make_manifest(id: &str) -> WorkflowManifest {
        WorkflowManifest {
            id: id.into(),
            title: format!("{id} title"),
            description: format!("{id} description"),
            output_kind: OutputKind::Image,
            output_node_id: "9".into(),
            output_filename_prefix: format!("comfyui-{id}"),
            params: vec![Param {
                key: "prompt".into(),
                node_id: "6".into(),
                input_key: "text".into(),
                description: "what to draw".into(),
                default: None,
                required: true,
                schema: ParamSchema {
                    ty: ParamType::String,
                    min: None,
                    max: None,
                    enum_values: None,
                    max_length: None,
                },
            }],
            workflow_path: PathBuf::from("/dev/null"),
            workflow_json: std::sync::Arc::new(json!({})),
        }
    }

    fn store_with(manifests: Vec<WorkflowManifest>) -> Arc<ComfyuiStore> {
        use std::collections::HashMap;
        let dir = tempfile::tempdir().unwrap();
        for m in &manifests {
            let sub = dir.path().join(&m.id);
            std::fs::create_dir_all(&sub).unwrap();
            let toml = format!(
                r#"
id = "{id}"
title = "{title}"
description = "{desc}"
output_kind = "image"
output_node_id = "9"
output_filename_prefix = "{id}"

[[params]]
key = "prompt"
node_id = "6"
input_key = "text"
required = true
description = "what to draw"

[params.schema]
type = "string"
"#,
                id = m.id,
                title = m.title,
                desc = m.description,
            );
            std::fs::write(sub.join("manifest.toml"), toml).unwrap();
            std::fs::write(sub.join("workflow.json"), "{}").unwrap();
        }
        let mut map = HashMap::new();
        for m in manifests {
            map.insert(m.id.clone(), Arc::new(m));
        }
        let _ = map; // store does its own scan; manifests provided via dir.
        Arc::new(ComfyuiStore::load(dir.path().to_path_buf()))
    }

    #[test]
    fn tool_id_is_prefixed_with_comfyui_() {
        let store = store_with(vec![make_manifest("text_to_image")]);
        let handle = ComfyuiHandle {
            store,
            client: Client::with_http("http://unused.invalid".into(), reqwest::Client::new()),
            runner_poll_interval: std::time::Duration::from_millis(5),
            runner_timeout: std::time::Duration::from_secs(1),
            s3: None,
            max_concurrent_jobs: 1,
            chat_updates: super::ChatUpdateRegistry::default(),
        };
        let tool = ComfyuiWorkflowTool::new("text_to_image".into(), handle);
        assert_eq!(tool.id(), "comfyui_text_to_image");
    }

    #[test]
    fn schema_lists_params_from_manifest() {
        let store = store_with(vec![make_manifest("text_to_image")]);
        let handle = ComfyuiHandle {
            store,
            client: Client::with_http("http://unused.invalid".into(), reqwest::Client::new()),
            runner_poll_interval: std::time::Duration::from_millis(5),
            runner_timeout: std::time::Duration::from_secs(1),
            s3: None,
            max_concurrent_jobs: 1,
            chat_updates: super::ChatUpdateRegistry::default(),
        };
        let tool = ComfyuiWorkflowTool::new("text_to_image".into(), handle);
        let schema = tool.schema();
        assert_eq!(schema.function.name, "comfyui_text_to_image");
        let props = schema.function.parameters["properties"]
            .as_object()
            .unwrap();
        assert!(props.contains_key("prompt"));
        assert_eq!(schema.function.parameters["required"][0], "prompt");
    }

    #[test]
    fn tool_source_lists_current_catalog_ids() {
        let store = store_with(vec![make_manifest("alpha"), make_manifest("bravo")]);
        let handle = ComfyuiHandle {
            store,
            client: Client::with_http("http://unused.invalid".into(), reqwest::Client::new()),
            runner_poll_interval: std::time::Duration::from_millis(5),
            runner_timeout: std::time::Duration::from_secs(1),
            s3: None,
            max_concurrent_jobs: 1,
            chat_updates: super::ChatUpdateRegistry::default(),
        };
        let source = ComfyuiToolSource::new(handle);
        let mut ids = source.ids();
        ids.sort();
        assert_eq!(ids, vec!["comfyui_alpha", "comfyui_bravo"]);
    }

    #[test]
    fn tool_source_get_returns_tool_for_known_id_only() {
        let store = store_with(vec![make_manifest("alpha")]);
        let handle = ComfyuiHandle {
            store,
            client: Client::with_http("http://unused.invalid".into(), reqwest::Client::new()),
            runner_poll_interval: std::time::Duration::from_millis(5),
            runner_timeout: std::time::Duration::from_secs(1),
            s3: None,
            max_concurrent_jobs: 1,
            chat_updates: super::ChatUpdateRegistry::default(),
        };
        let source = ComfyuiToolSource::new(handle);
        assert!(source.get("comfyui_alpha").is_some());
        assert!(source.get("comfyui_unknown").is_none());
        // Non-comfyui ids never match.
        assert!(source.get("company_echo").is_none());
    }

    #[test]
    fn tool_source_defs_for_filters_unknown_ids() {
        let store = store_with(vec![make_manifest("alpha")]);
        let handle = ComfyuiHandle {
            store,
            client: Client::with_http("http://unused.invalid".into(), reqwest::Client::new()),
            runner_poll_interval: std::time::Duration::from_millis(5),
            runner_timeout: std::time::Duration::from_secs(1),
            s3: None,
            max_concurrent_jobs: 1,
            chat_updates: super::ChatUpdateRegistry::default(),
        };
        let source = ComfyuiToolSource::new(handle);
        let defs = source.defs_for(&[
            "comfyui_alpha".into(),
            "comfyui_unknown".into(),
            "company_echo".into(),
        ]);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "comfyui_alpha");
    }
}
