// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Workflow substitution + execution orchestration.
//!
//! Given a [`WorkflowManifest`] and the model-supplied args, the runner:
//!
//! 1. resolves + validates the args against the manifest,
//! 2. injects each resolved value into `workflow[<node_id>].inputs[<input_key>]`,
//! 3. submits the substituted workflow to ComfyUI via [`super::client::Client`],
//! 4. polls `/history/{prompt_id}` until the workflow finishes,
//! 5. fetches the produced asset bytes from the output node.
//!
//! The result is a [`RunOutcome`] holding the downloaded bytes + mime; the
//! tool wrapper decides how to re-host them (chat-attachment S3 bucket).

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

use super::client::{Client, ComfyuiClientError, DownloadedAsset, ProducedAsset};
use super::manifest::{ArgError, OutputKind, ParamType, WorkflowManifest};
use crate::server::chat_attachments::{self, AttachmentError};
use gateway_core::server::config::S3Config;

/// Hardcoded poll interval. The operator-facing `queue_poll_interval_ms`
/// lives on the (future) handle that owns the client; the runner stays
/// stateless and takes it as a parameter via [`Runner::new`].
pub struct Runner {
    client: Client,
    poll_interval: Duration,
    timeout: Duration,
    /// Chat-attachment S3 config — when `Some`, the runner resolves
    /// `ImageAttachment` / `VideoAttachment` / `AudioAttachment` params
    /// (chat attachment ids `<turn_id>/<filename>`) into ComfyUI-side
    /// filenames before injecting. When `None`, attachment-kind params
    /// are rejected at resolve time — ComfyUI can't reach the bucket and
    /// we won't ship unauthenticated S3 URLs to the worker.
    s3: Option<Arc<S3Config>>,
}

impl Runner {
    pub fn new(client: Client, poll_interval: Duration, timeout: Duration) -> Self {
        Self {
            client,
            poll_interval,
            timeout,
            s3: None,
        }
    }

    /// Install the chat-attachment S3 handle. Required for any workflow
    /// that declares an `ImageAttachment` / `VideoAttachment` /
    /// `AudioAttachment` parameter. Mirrors the same `chat_attachments`
    /// path `fetch_attachment` and `edit_image` use, so the resolution
    /// semantics stay identical across the gateway.
    pub fn with_s3(mut self, s3: Arc<S3Config>) -> Self {
        self.s3 = Some(s3);
        self
    }

    /// Test-only constructor that lets tests reuse a pre-built `Client`.
    #[cfg(test)]
    pub fn with_client(client: Client) -> Self {
        Self::new(client, Duration::from_millis(5), Duration::from_secs(2))
    }

    /// Substitute `args` into the workflow and execute it end-to-end
    /// (blocking). The first asset on the manifest's `output_node_id`
    /// is the result. Use this for short workflows (text_to_image).
    /// For long workflows (video, music), use [`Self::prepare_and_submit`]
    /// + the scheduler instead.
    pub async fn run(
        &self,
        manifest: &WorkflowManifest,
        args: &Value,
    ) -> Result<RunOutcome, RunError> {
        let prompt_id = self.prepare_and_submit(manifest, args).await?;
        self.await_and_fetch(manifest, &prompt_id).await
    }

    /// Phase 1 of async execution: resolve args, upload attachments,
    /// inject params, and submit the workflow to ComfyUI. Returns the
    /// `prompt_id` immediately — does NOT wait for the workflow to
    /// finish. The caller (tool) stores this in `comfyui_jobs` and
    /// returns; the scheduler later polls for completion.
    pub async fn prepare_and_submit(
        &self,
        manifest: &WorkflowManifest,
        args: &Value,
    ) -> Result<String, RunError> {
        let mut resolved = manifest.resolve_args(args)?;
        self.resolve_seed_sentinel(manifest, &mut resolved);
        self.resolve_uploads(manifest, &mut resolved).await?;
        // Clone the pre-parsed workflow.json from the manifest's cache.
        // The manifest loaded + parsed it once at scan time; we just
        // deep-clone the Value for parameter injection. Cheaper than
        // re-reading + re-parsing the file on every call.
        let mut workflow = (*manifest.workflow_json).clone();
        strip_metadata_keys(&mut workflow);
        workflow = inject_params(workflow, manifest, &resolved);
        workflow = inject_output_prefix(workflow, manifest);
        self.client
            .submit_workflow(&workflow)
            .await
            .map_err(RunError::Client)
    }

    /// Phase 2 of async execution: poll for completion and fetch the
    /// asset. Called by the scheduler when it detects a completed job,
    /// or by the blocking `run()` path directly.
    pub async fn await_and_fetch(
        &self,
        manifest: &WorkflowManifest,
        prompt_id: &str,
    ) -> Result<RunOutcome, RunError> {
        let assets = self
            .client
            .await_completion(
                prompt_id,
                &manifest.output_node_id,
                self.poll_interval,
                self.timeout,
            )
            .await
            .map_err(RunError::Client)?;

        let first = assets
            .into_iter()
            .next()
            .ok_or_else(|| RunError::NoOutput {
                node_id: manifest.output_node_id.clone(),
                prompt_id: prompt_id.to_string(),
            })?;

        let downloaded = self
            .client
            .fetch_asset(&first)
            .await
            .map_err(RunError::Client)?;

        Ok(RunOutcome {
            prompt_id: prompt_id.to_string(),
            asset: first,
            downloaded,
            output_kind: manifest.output_kind,
        })
    }

    /// Resolve the conventional "-1 = fresh random" sentinel into a concrete
    /// value for every param the manifest marks `randomize_on_sentinel`.
    /// Driven by the manifest declaration, not the parameter's name, so a
    /// seed-role param can be called anything and a param that merely
    /// contains "seed" in its key isn't silently rewritten.
    fn resolve_seed_sentinel(&self, manifest: &WorkflowManifest, resolved: &mut Value) {
        let Some(obj) = resolved.as_object_mut() else {
            return;
        };
        for param in &manifest.params {
            if !param.schema.randomize_on_sentinel {
                continue;
            }
            if let Some(v) = obj.get_mut(&param.key)
                && v.as_i64() == Some(-1)
            {
                let n: u64 = rand::random();
                *v = Value::Number(serde_json::Number::from(n));
            }
        }
    }
}

#[derive(Debug)]
pub struct RunOutcome {
    pub prompt_id: String,
    pub asset: ProducedAsset,
    pub downloaded: DownloadedAsset,
    pub output_kind: OutputKind,
}

#[derive(Debug, Error)]
pub enum RunError {
    #[error("workflow arguments did not match the manifest")]
    InvalidArgs(#[from] ArgError),
    #[error("ComfyUI communication failed")]
    Client(#[source] ComfyuiClientError),
    #[error("ComfyUI recorded no output for node `{node_id}` (workflow `{prompt_id}`)")]
    NoOutput { node_id: String, prompt_id: String },
    #[error(
        "chat attachments are not configured (operator must set [chat.s3]) — workflow `{workflow_id}` needs attachment `{param_key}`"
    )]
    S3NotConfigured {
        workflow_id: String,
        param_key: String,
    },
    #[error(
        "attachment `{id}` referenced by parameter `{param_key}` could not be loaded: {source}"
    )]
    AttachmentFetch {
        id: String,
        param_key: String,
        #[source]
        source: AttachmentError,
    },
}

impl Runner {
    /// Walk the manifest's attachment-kind params, fetch each from S3,
    /// upload the bytes to ComfyUI's `/upload/image`, and overwrite the
    /// resolved value with the ComfyUI-side filename. After this pass,
    /// `inject_params` sees plain strings it can splice straight into
    /// the LoadImage/LoadVideo/LoadAudio inputs. Mirrors the same
    /// `chat_attachments::fetch` path `edit_image` and `fetch_attachment`
    /// use — single source of truth for `<turn_id>/<filename>` → bytes.
    async fn resolve_uploads(
        &self,
        manifest: &WorkflowManifest,
        resolved: &mut Value,
    ) -> Result<(), RunError> {
        let attachment_kind = |ty: ParamType| {
            matches!(
                ty,
                ParamType::ImageAttachment
                    | ParamType::VideoAttachment
                    | ParamType::AudioAttachment
            )
        };
        let Some(s3) = self.s3.as_ref() else {
            // No S3: reject only if an attachment-kind param is actually
            // in the resolved set. Plain-text workflows (text_to_image,
            // text_to_music) have no such params and stay usable.
            if let Some(p) = manifest
                .params
                .iter()
                .find(|p| attachment_kind(p.schema.ty) && resolved.get(&p.key).is_some())
            {
                return Err(RunError::S3NotConfigured {
                    workflow_id: manifest.id.clone(),
                    param_key: p.key.clone(),
                });
            }
            return Ok(());
        };
        let resolved_map = resolved
            .as_object_mut()
            .ok_or_else(|| RunError::InvalidArgs(ArgError::NotObject))?;
        for param in &manifest.params {
            if !attachment_kind(param.schema.ty) {
                continue;
            }
            let Some(value) = resolved_map.get_mut(&param.key) else {
                continue;
            };
            let id = value.as_str().unwrap_or("").to_string();
            let Some((turn_id, filename)) = id.split_once('/') else {
                // `validate_value` already rejected ids without `/`, so
                // this is unreachable in practice — surface a clean
                // error instead of panicking if the invariant ever drifts.
                continue;
            };
            let fetched = chat_attachments::fetch(s3, turn_id, filename)
                .await
                .map_err(|source| RunError::AttachmentFetch {
                    id: id.clone(),
                    param_key: param.key.clone(),
                    source,
                })?;
            let uploaded = self
                .client
                .upload_input(fetched.bytes, filename, &fetched.mime)
                .await
                .map_err(RunError::Client)?;
            *value = Value::String(uploaded.stored_path());
        }
        Ok(())
    }
}

/// Strip author-side metadata keys (any top-level key starting with `_`,
/// e.g. `_comment` / `_notes`) — ComfyUI's `/prompt` treats every top-
/// level entry as a node and rejects entries without a `class_type` with
/// `prompt_no_outputs`, so a `_comment` field would fail the whole
/// submission. Runs in-place on the cloned workflow value.
fn strip_metadata_keys(workflow: &mut Value) {
    if let Some(obj) = workflow.as_object_mut() {
        obj.retain(|k, _| !k.starts_with('_'));
    }
}

/// Walk the manifest's parameters and overwrite each `(node_id,
/// input_key)` slot in `workflow` with the resolved value. Validates that
/// every target exists — a manifest pointing at a non-existent node is a
/// real bug, not a recoverable runtime state.
pub(crate) fn inject_params(
    mut workflow: Value,
    manifest: &WorkflowManifest,
    resolved: &Value,
) -> Value {
    let resolved_map = resolved
        .as_object()
        .expect("resolve_args returns an object; invariant violated");
    for p in &manifest.params {
        let Some(value) = resolved_map.get(&p.key) else {
            continue;
        };
        inject_one(&mut workflow, p, value);
    }
    workflow
}

fn inject_one(workflow: &mut Value, param: &super::manifest::Param, value: &Value) {
    let Some(workflow_obj) = workflow.as_object_mut() else {
        tracing::error!(
            node_id = %param.node_id,
            input_key = %param.input_key,
            "workflow.json root is not a JSON object — cannot inject param"
        );
        return;
    };
    let Some(node) = workflow_obj
        .get_mut(&param.node_id)
        .and_then(Value::as_object_mut)
    else {
        tracing::error!(
            node_id = %param.node_id,
            input_key = %param.input_key,
            "workflow.json has no node `{}` — manifest references a node that doesn't exist",
            param.node_id
        );
        return;
    };
    let Some(inputs) = node.get_mut("inputs").and_then(Value::as_object_mut) else {
        tracing::error!(
            node_id = %param.node_id,
            input_key = %param.input_key,
            "node `{}` has no `inputs` object — cannot inject param",
            param.node_id
        );
        return;
    };
    inputs.insert(param.input_key.clone(), value.clone());
}

/// Overwrite the `filename_prefix` input on the manifest's output node
/// (typically a `SaveImage`/`SaveVideo`/`SaveAudio`) so produced assets
/// are attributable to the gateway. No-op when the manifest declares no
/// prefix, the output node has no `inputs` object, OR the node has no
/// pre-existing `filename_prefix` slot — the runner stays tolerant of
/// unusual output nodes (e.g. a debug `PreviewImage` that has no filename
/// concept and would reject the unknown input).
fn inject_output_prefix(mut workflow: Value, manifest: &WorkflowManifest) -> Value {
    let prefix = manifest.output_filename_prefix.trim();
    if prefix.is_empty() {
        return workflow;
    }
    let Some(node) = workflow
        .get_mut(&manifest.output_node_id)
        .and_then(Value::as_object_mut)
    else {
        return workflow;
    };
    let Some(inputs) = node.get_mut("inputs").and_then(Value::as_object_mut) else {
        return workflow;
    };
    if inputs.contains_key("filename_prefix") {
        inputs.insert("filename_prefix".into(), Value::String(prefix.to_string()));
    }
    workflow
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::comfyui::manifest::ParamSchema;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn text_workflow() -> Value {
        json!({
            "5": {
                "class_type": "EmptyLatentImage",
                "inputs": { "width": 1024, "height": 1024, "batch_size": 1 }
            },
            "6": {
                "class_type": "CLIPTextEncode",
                "inputs": { "text": "default", "clip": ["2", 0] }
            },
            "9": {
                "class_type": "SaveImage",
                "inputs": { "images": ["8", 0], "filename_prefix": "x" }
            }
        })
    }

    fn manifest_with(_prompt: &str, _width: i64) -> WorkflowManifest {
        WorkflowManifest {
            id: "text_to_image".into(),
            title: "T".into(),
            description: "D".into(),
            output_kind: OutputKind::Image,
            output_node_id: "9".into(),
            output_filename_prefix: "x".into(),
            params: vec![
                super::super::manifest::Param {
                    key: "prompt".into(),
                    node_id: "6".into(),
                    input_key: "text".into(),
                    description: "d".into(),
                    default: Some(json!("default")),
                    required: false,
                    schema: ParamSchema {
                        ty: super::super::manifest::ParamType::String,
                        min: None,
                        max: None,
                        enum_values: None,
                        max_length: None,
                        randomize_on_sentinel: false,
                    },
                },
                super::super::manifest::Param {
                    key: "width".into(),
                    node_id: "5".into(),
                    input_key: "width".into(),
                    description: "d".into(),
                    default: Some(json!(1024)),
                    required: false,
                    schema: ParamSchema {
                        ty: super::super::manifest::ParamType::Integer,
                        min: Some(256.0),
                        max: Some(2048.0),
                        enum_values: None,
                        max_length: None,
                        randomize_on_sentinel: false,
                    },
                },
            ],
            workflow_json: std::sync::Arc::new(json!({})),
        }
    }

    #[test]
    fn inject_params_overwrites_only_targeted_slots() {
        let m = manifest_with("ignored", 0);
        let resolved = json!({ "prompt": "a cat", "width": 768 });
        let out = inject_params(text_workflow(), &m, &resolved);
        assert_eq!(out["5"]["inputs"]["width"], 768);
        assert_eq!(out["5"]["inputs"]["height"], 1024); // untouched
        assert_eq!(out["6"]["inputs"]["text"], "a cat");
    }

    #[test]
    fn inject_params_skips_keys_not_in_resolved() {
        let m = manifest_with("ignored", 0);
        let resolved = json!({ "prompt": "a cat" });
        let out = inject_params(text_workflow(), &m, &resolved);
        assert_eq!(out["6"]["inputs"]["text"], "a cat");
        // width untouched because not in resolved args
        assert_eq!(out["5"]["inputs"]["width"], 1024);
    }

    #[test]
    fn inject_output_prefix_overwrites_saveimage_filename_prefix() {
        let mut m = manifest_with("ignored", 0);
        m.output_filename_prefix = "llmgw-text2image".into();
        let out = inject_output_prefix(text_workflow(), &m);
        assert_eq!(out["9"]["inputs"]["filename_prefix"], "llmgw-text2image");
    }

    #[test]
    fn inject_output_prefix_is_noop_when_manifest_prefix_empty() {
        let mut m = manifest_with("ignored", 0);
        m.output_filename_prefix = String::new();
        let original = text_workflow();
        let out = inject_output_prefix(original.clone(), &m);
        // The SaveImage keeps whatever workflow.json declared.
        assert_eq!(
            out["9"]["inputs"]["filename_prefix"],
            original["9"]["inputs"]["filename_prefix"]
        );
    }

    #[test]
    fn inject_output_prefix_is_noop_when_output_node_lacks_filename_prefix() {
        // SaveImage without filename_prefix — no field gets injected.
        let mut workflow = text_workflow();
        workflow["9"]["inputs"]
            .as_object_mut()
            .unwrap()
            .remove("filename_prefix");
        let m = manifest_with("ignored", 0);
        let out = inject_output_prefix(workflow.clone(), &m);
        // No filename_prefix was added.
        assert!(out["9"]["inputs"].get("filename_prefix").is_none());
    }

    #[test]
    fn image2video_example_wires_motion_params() {
        // Regression: the exposed motion knobs (`lora_strength_high`, `shift`)
        // must resolve their defaults, overwrite the `{{…}}` placeholders with
        // real numbers, and land on the right nodes — and the two sampling
        // stages must both keep reading `shift` from the shared source node.
        // Guards against a placeholder leaking through to ComfyUI (which would
        // fail the whole workflow) and against the param↔node mapping drifting.
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/comfyui-workflows/llmgw-image2video");
        let m = super::super::manifest::load(&dir).expect("example manifest loads");

        // Defaults only (model supplied just the required fields).
        let resolved = m
            .resolve_args(&json!({ "image": "t/pic.png", "prompt": "runs forward" }))
            .expect("resolve defaults");
        assert_eq!(resolved["lora_strength_high"], json!(0.75));
        assert_eq!(resolved["shift"], json!(4.0));

        let wf = inject_params((*m.workflow_json).clone(), &m, &resolved);
        // Placeholders replaced by real numbers on the target nodes.
        assert_eq!(wf["129:101"]["inputs"]["strength_model"], json!(0.75));
        assert_eq!(wf["129:200"]["inputs"]["value"], json!(4.0));
        // Both sampling stages still pull shift from the shared source node.
        assert_eq!(wf["129:103"]["inputs"]["shift"], json!(["129:200", 0]));
        assert_eq!(wf["129:104"]["inputs"]["shift"], json!(["129:200", 0]));

        // A high-motion request threads the lowered values through.
        let hot = m
            .resolve_args(&json!({
                "image": "t/pic.png",
                "prompt": "sprints toward camera",
                "lora_strength_high": 0.5,
                "shift": 3.0,
            }))
            .expect("resolve high-motion");
        let wf2 = inject_params((*m.workflow_json).clone(), &m, &hot);
        assert_eq!(wf2["129:101"]["inputs"]["strength_model"], json!(0.5));
        assert_eq!(wf2["129:200"]["inputs"]["value"], json!(3.0));
    }

    #[tokio::test]
    async fn run_end_to_end_returns_downloaded_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/prompt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "prompt_id": "p-1",
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/history/p-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "p-1": {
                    "status": { "completed": true, "status_str": "success" },
                    "outputs": { "9": { "images": [{ "filename": "out.png", "subfolder": "", "type": "output" }] } }
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/view"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(b"\x89PNG\r\n\x1a\n".to_vec()),
            )
            .mount(&server)
            .await;

        let m = manifest_with("ignored", 0);

        let client = Client::with_http(server.uri(), reqwest::Client::new());
        let runner = Runner::with_client(client);
        let outcome = runner
            .run(&m, &json!({ "prompt": "a cat", "width": 512 }))
            .await
            .expect("ok");
        assert_eq!(outcome.prompt_id, "p-1");
        assert_eq!(outcome.downloaded.mime, "image/png");
        assert_eq!(outcome.downloaded.bytes, b"\x89PNG\r\n\x1a\n");
        assert_eq!(outcome.output_kind, OutputKind::Image);
    }

    #[tokio::test]
    async fn run_surfaces_invalid_args_before_calling_comfyui() {
        let server = MockServer::start().await;
        // No mocks — if we hit ComfyUI the test fails.
        let client = Client::with_http(server.uri(), reqwest::Client::new());
        let runner = Runner::with_client(client);
        // required: true via required_int_param — width missing.
        let mut m = manifest_with("ignored", 0);
        m.params[1].required = true;
        m.params[1].default = None;
        let err = runner.run(&m, &json!({ "prompt": "x" })).await.unwrap_err();
        assert!(matches!(err, RunError::InvalidArgs(_)), "{err:?}");
    }

    #[tokio::test]
    async fn run_surfaces_comfyui_workflow_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/prompt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "prompt_id": "p-fail",
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/history/p-fail"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "p-fail": {
                    "status": { "completed": false, "status_str": "error" },
                    "outputs": {}
                }
            })))
            .mount(&server)
            .await;

        let m = manifest_with("ignored", 0);

        let client = Client::with_http(server.uri(), reqwest::Client::new());
        let runner = Runner::with_client(client);
        let err = runner.run(&m, &json!({ "prompt": "x" })).await.unwrap_err();
        assert!(matches!(err, RunError::Client(_)), "{err:?}");
    }
}
