// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! HTTP client for the ComfyUI prompt-API.
//!
//! Three endpoints in scope:
//!
//! - `POST /prompt` — queue a workflow; returns `{"prompt_id": "…"}`.
//! - `GET  /history/{id}` — poll the workflow's status + outputs.
//! - `GET  /view?filename=…&subfolder=…&type=output` — fetch produced bytes.
//!
//! ComfyUI's `/prompt` expects `{"prompt": <api-format workflow>,
//! "client_id": "…"}`. The client inserts its own `client_id` (a fresh
//! UUID per call) so the gateway never confuses two queued jobs. The
//! `/history` payload is large (every job the worker has ever run); we
//! pluck just our `prompt_id` out of it.
//!
//! No websocket support — `/history` polling is plenty for the gateway's
//! low-volume use, and avoids dragging in `tokio-tungstenite`. If latency
//! ever demands it, a future PR can add a `/ws` listener that flips
//! [`Client::submit_workflow`] to await the `execution_success` message.

use std::time::Duration;

use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

/// Cap on the produced asset we keep in memory / hand to chat attachments.
/// Matches `image_gen::MAX_IMAGE_BYTES` so limits are uniform across the
/// two image paths.
pub(crate) const MAX_OUTPUT_BYTES: usize = 25 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ComfyuiClientError {
    #[error("ComfyUI at {base_url} did not respond: {source}")]
    Unreachable {
        base_url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("ComfyUI returned HTTP {status} from `{method} {path}`: {body}")]
    HttpStatus {
        method: &'static str,
        path: String,
        status: u16,
        body: String,
    },
    #[error("ComfyUI returned an unparseable response from `{method} {path}`: {source}")]
    BadResponse {
        method: &'static str,
        path: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("ComfyUI history for `{prompt_id}` is missing or malformed")]
    HistoryMissing { prompt_id: String },
    #[error("ComfyUI workflow `{prompt_id}` finished with status: {status}")]
    WorkflowFailed { prompt_id: String, status: String },
    #[error("ComfyUI workflow `{prompt_id}` did not finish within {timeout_secs}s")]
    WorkflowTimeout {
        prompt_id: String,
        timeout_secs: u64,
    },
    #[error("ComfyUI produced no output for node `{node_id}` (workflow `{prompt_id}`)")]
    NoOutput { prompt_id: String, node_id: String },
    #[error("produced asset is larger than the {} MB limit", MAX_OUTPUT_BYTES / 1024 / 1024)]
    OutputTooLarge,
    #[error("building the HTTP client")]
    ClientBuild(#[source] reqwest::Error),
}

/// Cheaply-cloneable handle — wraps a `reqwest::Client` and the operator-
/// configured `base_url`. Held in `Arc` by the runner and the tool wrapper.
#[derive(Clone)]
pub struct Client {
    http: HttpClient,
    base_url: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComfyuiClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(base_url: String) -> Result<Self, ComfyuiClientError> {
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(ComfyuiClientError::ClientBuild)?;
        Ok(Self { http, base_url })
    }

    /// Test-only constructor that takes a pre-built `reqwest::Client`
    /// (so wiremock tests can inject their own).
    #[cfg(test)]
    pub fn with_http(base_url: String, http: HttpClient) -> Self {
        Self { http, base_url }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Queue a workflow and return its `prompt_id`. The caller passes the
    /// fully-substituted ComfyUI prompt-API document; the client only
    /// injects its own `client_id`.
    pub async fn submit_workflow(&self, prompt: &Value) -> Result<String, ComfyuiClientError> {
        let body = serde_json::json!({
            "prompt": prompt,
            "client_id": Uuid::new_v4().to_string(),
        });
        let resp = self
            .http
            .post(format!("{}/prompt", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| ComfyuiClientError::Unreachable {
                base_url: self.base_url.clone(),
                source: e,
            })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ComfyuiClientError::HttpStatus {
                method: "POST",
                path: "/prompt".into(),
                status: status.as_u16(),
                body: truncate(body, 500),
            });
        }
        let parsed: SubmitResponse =
            resp.json()
                .await
                .map_err(|e| ComfyuiClientError::BadResponse {
                    method: "POST",
                    path: "/prompt".into(),
                    source: e,
                })?;
        Ok(parsed.prompt_id)
    }

    /// Single-shot status check (no blocking). Returns `None` when the
    /// workflow is still running, `Some(Ok(assets))` when completed, and
    /// `Some(Err(reason))` when ComfyUI reported a failure. The scheduler
    /// uses this instead of `await_completion` so it doesn't block on
    /// any single job.
    pub async fn check_status(
        &self,
        prompt_id: &str,
        output_node_id: &str,
    ) -> Result<StatusCheck, ComfyuiClientError> {
        match self.history(prompt_id).await? {
            HistoryState::Pending => Ok(StatusCheck::Pending),
            HistoryState::Failed(status) => Ok(StatusCheck::Failed(status)),
            HistoryState::Completed(outputs) => {
                let assets = outputs.get(output_node_id).cloned().unwrap_or_default();
                if assets.is_empty() {
                    Ok(StatusCheck::Failed("no output asset for the node".into()))
                } else {
                    Ok(StatusCheck::Completed(assets))
                }
            }
        }
    }

    /// Poll `/history/{prompt_id}` until the workflow reaches a terminal
    /// state (success or failure) or `timeout` elapses. On success,
    /// returns the output blob for `output_node_id` as ComfyUI recorded it.
    pub async fn await_completion(
        &self,
        prompt_id: &str,
        output_node_id: &str,
        poll_interval: Duration,
        timeout: Duration,
    ) -> Result<Vec<ProducedAsset>, ComfyuiClientError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(ComfyuiClientError::WorkflowTimeout {
                    prompt_id: prompt_id.into(),
                    timeout_secs: timeout.as_secs(),
                });
            }
            match self.history(prompt_id).await? {
                HistoryState::Pending => {
                    tokio::time::sleep(poll_interval).await;
                }
                HistoryState::Failed(status) => {
                    return Err(ComfyuiClientError::WorkflowFailed {
                        prompt_id: prompt_id.into(),
                        status,
                    });
                }
                HistoryState::Completed(outputs) => {
                    let node = outputs.get(output_node_id).ok_or_else(|| {
                        ComfyuiClientError::NoOutput {
                            prompt_id: prompt_id.into(),
                            node_id: output_node_id.into(),
                        }
                    })?;
                    return Ok(node.clone());
                }
            }
        }
    }

    async fn history(&self, prompt_id: &str) -> Result<HistoryState, ComfyuiClientError> {
        let path = format!("/history/{prompt_id}");
        let resp = self
            .http
            .get(format!("{}{path}", self.base_url))
            .send()
            .await
            .map_err(|e| ComfyuiClientError::Unreachable {
                base_url: self.base_url.clone(),
                source: e,
            })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ComfyuiClientError::HttpStatus {
                method: "GET",
                path,
                status: status.as_u16(),
                body: truncate(body, 500),
            });
        }
        let parsed: HistoryResponse =
            resp.json()
                .await
                .map_err(|e| ComfyuiClientError::BadResponse {
                    method: "GET",
                    path,
                    source: e,
                })?;
        Ok(parsed.into_state(prompt_id))
    }

    /// Upload a file (image/video/audio) into ComfyUI's input store via
    /// `/upload/image`. Used by the runner to stage a chat attachment
    /// before substituting its id into a LoadImage/LoadVideo/LoadAudio
    /// node — ComfyUI can't reach the gateway's S3 bucket, so the bytes
    /// have to land in the worker's `input/` directory first.
    ///
    /// Returns the ComfyUI-side filename (which may carry a generated
    /// suffix or land in a subfolder when the worker dedupes); the
    /// caller writes that into the target node's `image`/`video`/`audio`
    /// input slot.
    pub async fn upload_input(
        &self,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
    ) -> Result<UploadedInput, ComfyuiClientError> {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str(mime)
            .map_err(|e| ComfyuiClientError::BadResponse {
                method: "POST",
                path: "/upload/image".into(),
                source: e,
            })?;
        let form = reqwest::multipart::Form::new()
            .text("type", "input")
            .text("overwrite", "true")
            .part("image", part);
        let resp = self
            .http
            .post(format!("{}/upload/image", self.base_url))
            .multipart(form)
            .send()
            .await
            .map_err(|e| ComfyuiClientError::Unreachable {
                base_url: self.base_url.clone(),
                source: e,
            })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ComfyuiClientError::HttpStatus {
                method: "POST",
                path: "/upload/image".into(),
                status: status.as_u16(),
                body: truncate(body, 500),
            });
        }
        let parsed: UploadResponse =
            resp.json()
                .await
                .map_err(|e| ComfyuiClientError::BadResponse {
                    method: "POST",
                    path: "/upload/image".into(),
                    source: e,
                })?;
        Ok(UploadedInput {
            name: parsed.name,
            subfolder: parsed.subfolder.unwrap_or_default(),
        })
    }

    /// Fetch the bytes of one produced asset. ComfyUI's `/view` returns
    /// the raw file content; we trust the caller's `mime` only as a hint
    /// and sniff the bytes when we can.
    pub async fn fetch_asset(
        &self,
        asset: &ProducedAsset,
    ) -> Result<DownloadedAsset, ComfyuiClientError> {
        let path = format!(
            "/view?filename={}&subfolder={}&type={}",
            urlencoding::encode_path(&asset.filename),
            urlencoding::encode_path(&asset.subfolder),
            urlencoding::encode_path(&asset.r#type),
        );
        let resp = self
            .http
            .get(format!("{}{path}", self.base_url))
            .send()
            .await
            .map_err(|e| ComfyuiClientError::Unreachable {
                base_url: self.base_url.clone(),
                source: e,
            })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ComfyuiClientError::HttpStatus {
                method: "GET",
                path,
                status: status.as_u16(),
                body: truncate(body, 500),
            });
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or("").trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| sniff_mime_from_name(&asset.filename).to_string());
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ComfyuiClientError::BadResponse {
                method: "GET",
                path,
                source: e,
            })?
            .to_vec();
        if bytes.len() > MAX_OUTPUT_BYTES {
            return Err(ComfyuiClientError::OutputTooLarge);
        }
        Ok(DownloadedAsset {
            bytes,
            mime: content_type,
        })
    }
}

/// One file ComfyUI recorded as an output of the workflow.
/// `r#type` is typically `"output"` (or `"temp"`); `subfolder` is often empty.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ProducedAsset {
    pub filename: String,
    #[serde(default)]
    pub subfolder: String,
    #[serde(default = "default_output_type")]
    pub r#type: String,
}

fn default_output_type() -> String {
    "output".into()
}

impl ProducedAsset {
    pub fn output_kind(&self) -> &str {
        &self.r#type
    }
}

/// Result of a single-shot [`Client::check_status`] call — non-blocking
/// status snapshot from ComfyUI's `/history/{id}` endpoint.
#[derive(Debug)]
pub enum StatusCheck {
    Pending,
    Completed(Vec<ProducedAsset>),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct DownloadedAsset {
    pub bytes: Vec<u8>,
    pub mime: String,
}

#[derive(Debug, Deserialize)]
struct SubmitResponse {
    prompt_id: String,
}

/// `/upload/image` response. ComfyUI returns the stored filename split
/// into `name` + optional `subfolder`; together they identify the file
/// for the subsequent LoadImage/LoadVideo/LoadAudio node.
#[derive(Debug, Deserialize)]
struct UploadResponse {
    name: String,
    #[serde(default)]
    subfolder: Option<String>,
}

/// Successful result of [`Client::upload_input`]. Callers compose
/// `subfolder/name` (or just `name` when the subfolder is empty) into
/// the LoadImage/LoadVideo/LoadAudio input slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedInput {
    pub name: String,
    pub subfolder: String,
}

impl UploadedInput {
    /// ComfyUI-side path the LoadImage/LoadVideo/LoadAudio node receives.
    /// `<subfolder>/<name>` when the worker placed the file in a
    /// subfolder, otherwise just `<name>`.
    pub fn stored_path(&self) -> String {
        if self.subfolder.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", self.subfolder, self.name)
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
struct HistoryResponse(std::collections::HashMap<String, HistoryEntry>);

/// `/history` entries nest outputs as `{node_id: {kind: [values]}}`.
/// ComfyUI has both asset-producing nodes (SaveImage → `{images:
/// [{filename, ...}]}`) and text-producing nodes (PreviewAny →
/// `{text: ["some string"]}`). The `kind` key tells us which, but the
/// inner values are heterogeneous. We deserialize loosely as
/// `Vec<serde_json::Value>` and then try to coerce each element to a
/// `ProducedAsset` — non-asset values (strings, bools) are silently
/// skipped by [`HistoryEntry::assets_for`].
#[derive(Debug, Deserialize)]
struct HistoryEntry {
    #[serde(default)]
    status: Option<HistoryStatus>,
    #[serde(default)]
    outputs: std::collections::HashMap<
        String,
        std::collections::HashMap<String, Vec<serde_json::Value>>,
    >,
}

impl HistoryEntry {
    /// Extract produced-asset metadata for `output_node_id`, ignoring
    /// any non-asset values (PreviewAny text, etc.) that can't be
    /// deserialized into [`ProducedAsset`].
    fn assets_for(&self, output_node_id: &str) -> Vec<ProducedAsset> {
        let mut out = Vec::new();
        if let Some(node_outputs) = self.outputs.get(output_node_id) {
            let mut kinds: Vec<&String> = node_outputs.keys().collect();
            kinds.sort();
            for kind in kinds {
                if let Some(values) = node_outputs.get(kind) {
                    for v in values {
                        if let Ok(asset) = serde_json::from_value::<ProducedAsset>(v.clone()) {
                            out.push(asset);
                        }
                    }
                }
            }
        }
        out
    }
}

#[derive(Debug, Deserialize)]
struct HistoryStatus {
    #[serde(default)]
    completed: bool,
    #[serde(default)]
    status_str: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    status: Option<serde_json::Number>,
}

enum HistoryState {
    Pending,
    Failed(String),
    Completed(std::collections::HashMap<String, Vec<ProducedAsset>>),
}

impl HistoryResponse {
    fn into_state(self, prompt_id: &str) -> HistoryState {
        let Some(entry) = self.0.get(prompt_id) else {
            return HistoryState::Pending;
        };
        match &entry.status {
            None => HistoryState::Pending,
            Some(status) => {
                if !status.completed {
                    if let Some(reason) = &status.status_str
                        && reason.eq_ignore_ascii_case("error")
                    {
                        return HistoryState::Failed(reason.clone());
                    }
                    return HistoryState::Pending;
                }
                HistoryState::Completed(flatten_outputs(entry))
            }
        }
    }
}

/// Flatten ComfyUI's nested `{node_id: {kind: [values]}}` outputs into
/// `{node_id: [asset, ...]}`. Non-asset values (PreviewAny text, bools)
/// are silently skipped — the runner only cares about file-producing
/// nodes (SaveImage / SaveVideo / SaveAudio). Uses
/// [`HistoryEntry::assets_for`] internally.
fn flatten_outputs(entry: &HistoryEntry) -> std::collections::HashMap<String, Vec<ProducedAsset>> {
    let mut out = std::collections::HashMap::new();
    for node_id in entry.outputs.keys() {
        let assets = entry.assets_for(node_id);
        if !assets.is_empty() {
            out.insert(node_id.clone(), assets);
        }
    }
    out
}

fn truncate(s: String, max: usize) -> String {
    s.chars().take(max).collect()
}

fn sniff_mime_from_name(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".mp4") || lower.ends_with(".mov") {
        "video/mp4"
    } else if lower.ends_with(".webm") {
        "video/webm"
    } else if lower.ends_with(".mp3") {
        "audio/mpeg"
    } else if lower.ends_with(".wav") {
        "audio/wav"
    } else {
        "application/octet-stream"
    }
}

// Tiny inline path-encoder (avoids adding `urlencoding` as a dep — the
// workspace already has `url`, but that's heavier than what we need).
mod urlencoding {
    pub fn encode_path(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        for b in input.bytes() {
            // Unreserved + path-safe per RFC 3986; everything else gets %xx.
            if b.is_ascii_alphanumeric()
                || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/' | b':' | b'@')
            {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{:02X}", b));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn base_url(server: &MockServer) -> String {
        server.uri()
    }

    #[tokio::test]
    async fn submit_workflow_returns_prompt_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/prompt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "prompt_id": "abc-123",
                "number": 1,
                "node_errors": {},
            })))
            .mount(&server)
            .await;

        let client = Client::new(base_url(&server)).unwrap();
        let id = client.submit_workflow(&json!({})).await.expect("ok");
        assert_eq!(id, "abc-123");
    }

    #[tokio::test]
    async fn submit_workflow_propagates_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/prompt"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad prompt"))
            .mount(&server)
            .await;

        let client = Client::new(base_url(&server)).unwrap();
        let err = client.submit_workflow(&json!({})).await.unwrap_err();
        match err {
            ComfyuiClientError::HttpStatus {
                method,
                status,
                body,
                ..
            } => {
                assert_eq!(method, "POST");
                assert_eq!(status, 400);
                assert!(body.contains("bad prompt"));
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn await_completion_returns_assets_when_history_completed() {
        let server = MockServer::start().await;
        let prompt_id = "p-1";
        Mock::given(method("GET"))
            .and(path(format!("/history/{prompt_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                prompt_id: {
                    "status": { "completed": true, "status_str": "success" },
                    "outputs": {
                        "9": { "images": [{
                            "filename": "out.png",
                            "subfolder": "",
                            "type": "output"
                        }] }
                    }
                }
            })))
            .mount(&server)
            .await;

        let client = Client::new(base_url(&server)).unwrap();
        let assets = client
            .await_completion(
                prompt_id,
                "9",
                Duration::from_millis(10),
                Duration::from_secs(1),
            )
            .await
            .expect("ok");
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].filename, "out.png");
        assert_eq!(assets[0].r#type, "output");
    }

    #[tokio::test]
    async fn await_completion_polls_until_history_completed() {
        let server = MockServer::start().await;
        let prompt_id = "p-2";
        // First response: empty history (pending). Second: completed.
        Mock::given(method("GET"))
            .and(path(format!("/history/{prompt_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/history/{prompt_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                prompt_id: {
                    "status": { "completed": true, "status_str": "success" },
                    "outputs": { "9": { "images": [{ "filename": "done.png", "subfolder": "", "type": "output" }] } }
                }
            })))
            .mount(&server)
            .await;

        let client = Client::new(base_url(&server)).unwrap();
        let assets = client
            .await_completion(
                prompt_id,
                "9",
                Duration::from_millis(5),
                Duration::from_secs(2),
            )
            .await
            .expect("ok");
        assert_eq!(assets[0].filename, "done.png");
    }

    #[tokio::test]
    async fn await_completion_errors_when_workflow_failed() {
        let server = MockServer::start().await;
        let prompt_id = "p-3";
        Mock::given(method("GET"))
            .and(path(format!("/history/{prompt_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                prompt_id: {
                    "status": { "completed": false, "status_str": "error" },
                    "outputs": {}
                }
            })))
            .mount(&server)
            .await;

        let client = Client::new(base_url(&server)).unwrap();
        let err = client
            .await_completion(
                prompt_id,
                "9",
                Duration::from_millis(5),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ComfyuiClientError::WorkflowFailed { .. }));
    }

    #[tokio::test]
    async fn await_completion_times_out() {
        let server = MockServer::start().await;
        let prompt_id = "p-4";
        Mock::given(method("GET"))
            .and(path(format!("/history/{prompt_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let client = Client::new(base_url(&server)).unwrap();
        let err = client
            .await_completion(
                prompt_id,
                "9",
                Duration::from_millis(5),
                Duration::from_millis(20),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ComfyuiClientError::WorkflowTimeout { .. }));
    }

    #[tokio::test]
    async fn fetch_asset_returns_bytes_and_sniffs_mime() {
        let server = MockServer::start().await;
        let png_bytes = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        Mock::given(method("GET"))
            .and(path("/view"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(png_bytes.to_vec()),
            )
            .mount(&server)
            .await;

        let client = Client::new(base_url(&server)).unwrap();
        let asset = ProducedAsset {
            filename: "out.png".into(),
            subfolder: "".into(),
            r#type: "output".into(),
        };
        let dl = client.fetch_asset(&asset).await.expect("ok");
        assert_eq!(dl.bytes, png_bytes);
        assert_eq!(dl.mime, "image/png");
    }

    #[tokio::test]
    async fn fetch_asset_errors_when_http_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/view"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let client = Client::new(base_url(&server)).unwrap();
        let asset = ProducedAsset {
            filename: "missing.png".into(),
            subfolder: "".into(),
            r#type: "output".into(),
        };
        let err = client.fetch_asset(&asset).await.unwrap_err();
        assert!(matches!(err, ComfyuiClientError::HttpStatus { .. }));
    }

    #[test]
    fn sniff_mime_handles_common_types() {
        assert_eq!(sniff_mime_from_name("a.png"), "image/png");
        assert_eq!(sniff_mime_from_name("a.JPG"), "image/jpeg");
        assert_eq!(sniff_mime_from_name("clip.mp4"), "video/mp4");
        assert_eq!(sniff_mime_from_name("voice.wav"), "audio/wav");
        assert_eq!(
            sniff_mime_from_name("unknown.bin"),
            "application/octet-stream"
        );
    }
}
