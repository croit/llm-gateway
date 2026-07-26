// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Server-side image generation.
//!
//! A small, cheaply-cloneable handle ([`ImageGenerator`]) that the
//! `generate_image` tool holds via its `ToolContext`. It routes an image
//! request through the normal upstream registry (a `PoolKind::Image` pool),
//! POSTs an OpenAI `/images/generations`-shaped body, and returns the
//! generated image *as bytes in hand* — decoding a `b64_json` payload or,
//! when the backend returns a URL instead (z.AI's GLM-Image does), fetching
//! those bytes server-side. The caller then re-hosts them in the gateway's
//! own S3 bucket, so no upstream URL (which may expire, or leak the provider)
//! ever reaches the browser or the model.
//!
//! This is deliberately **not** used by the `/v1/images/generations` proxy
//! endpoint: that path is a byte-dumb 1:1 relay (see `rama_server::proxy`),
//! which is the correct behaviour for an OpenAI-compatible surface — it hands
//! the client the provider's exact response. The two paths share only
//! `PoolKind::Image` and `UsageKind::Image`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::server::chat_attachments;
use gateway_core::server::db::Pool;
use gateway_core::server::db::usage::{UnitUsage, UsageKind, UsageRecord, UsageSource};
use gateway_core::server::feature_defaults::{self, Feature};
use gateway_core::server::upstreams::{PoolKind, UpstreamRegistry, registry::RouteError};
use gateway_core::server::usage::UsageHandle;

/// Cap on the decoded image we keep in memory / hand to S3. Matches the
/// `fetch_url` / `fetch_attachment` image ceiling so limits are uniform.
const MAX_IMAGE_BYTES: usize = 25 * 1024 * 1024;
/// Timeout for the generation call itself. Diffusion backends are slow; this
/// is generous. The tool's own `max_duration` sits above it.
const GENERATE_TIMEOUT: Duration = Duration::from_secs(120);
/// Timeout for the follow-up GET when a backend returns a URL rather than
/// inline bytes.
const URL_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// One generated image, ready to upload.
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub mime: String,
}

impl std::fmt::Debug for GeneratedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Elide the bytes — a test failure printing a few MB is unreadable.
        f.debug_struct("GeneratedImage")
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .field("mime", &self.mime)
            .finish()
    }
}

/// Who to attribute a metered image call to. The tool builds this from its
/// `ToolContext`; email/token aren't threaded that far, so they're `None`
/// (the usage rollup keys primarily on `user_id`).
pub struct UsageMeta {
    pub user_id: String,
    pub source: UsageSource,
}

#[derive(Debug, Error)]
pub enum ImageGenError {
    /// No image pool serves the requested (or default) model — the operator
    /// hasn't wired a `kind = "image"` pool, or the model id is wrong.
    #[error("no image-generation backend is configured for model `{0}`")]
    NoBackend(String),
    /// The upstream returned a non-success status or was unreachable.
    #[error("image backend error: {0}")]
    Upstream(String),
    /// The response parsed but carried neither `b64_json` nor a `url`, or the
    /// URL's bytes weren't a usable image.
    #[error("image backend returned an unusable response: {0}")]
    BadResponse(String),
    /// The generated image exceeded [`MAX_IMAGE_BYTES`].
    #[error("generated image is larger than the {} MB limit", MAX_IMAGE_BYTES / 1024 / 1024)]
    TooLarge,
    /// Editing was requested but no image backend advertises edit support.
    #[error("no configured image backend supports editing")]
    EditUnsupported,
    /// Editing was blocked because the target backend is non-GDPR-compliant —
    /// we won't ship existing user images to it.
    #[error("image editing is blocked for this backend (non-GDPR-compliant provider)")]
    ComplianceBlocked,
}

/// OpenAI `/images/generations` response envelope (the subset we read). Both
/// the b64 and URL variants of a single datum are optional so we can accept
/// either provider convention.
#[derive(Deserialize)]
struct ImagesResponse {
    #[serde(default)]
    data: Vec<ImageDatum>,
}

#[derive(Deserialize)]
struct ImageDatum {
    #[serde(default)]
    b64_json: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

/// Count image units consistently for the raw proxy and the built-in image
/// tool. `None` means the provider response did not contain a generated image.
pub fn image_units(count: usize) -> Option<f64> {
    (count > 0).then_some(count as f64)
}

pub fn image_output_units_from_value(value: &Value) -> Option<f64> {
    value
        .get("data")
        .and_then(Value::as_array)
        .and_then(|data| image_units(data.len()))
}

/// Cheaply-cloneable image-generation handle. Built from state fields the
/// tool-context construction sites already own; see `ToolContext::image_gen`.
#[derive(Clone)]
pub struct ImageGenerator {
    upstreams: Arc<UpstreamRegistry>,
    http: reqwest::Client,
    usage: UsageHandle,
    /// Read-only handle for the operator-configured default image model
    /// (`app_settings` → `default_model.image`). See [`Self::default_model`].
    db: Pool,
}

impl ImageGenerator {
    pub fn new(
        upstreams: Arc<UpstreamRegistry>,
        http: reqwest::Client,
        usage: UsageHandle,
        db: Pool,
    ) -> Self {
        Self {
            upstreams,
            http,
            usage,
            db,
        }
    }

    /// Whether any configured image backend advertises image *editing*. Used
    /// to decide whether the Phase-2 `edit_image` tool is offered at all.
    pub fn edit_available(&self) -> bool {
        self.upstreams
            .pools()
            .into_iter()
            .any(|p| p.kind == PoolKind::Image && p.backends.iter().any(|b| b.supports_edit()))
    }

    /// Generate an image from a text prompt. `model` selects a specific image
    /// model / alias; `None` falls back to whatever the image pool advertises.
    pub async fn generate(
        &self,
        model: Option<&str>,
        prompt: &str,
        size: Option<&str>,
        meta: &UsageMeta,
    ) -> Result<GeneratedImage, ImageGenError> {
        let requested = match model {
            Some(m) => m.to_string(),
            None => self
                .default_model()
                .await
                .ok_or_else(|| ImageGenError::NoBackend(String::new()))?,
        };

        // Route + hold an inflight slot for the duration of the call, exactly
        // like the proxy paths.
        let acquired = self
            .upstreams
            .route(&requested, PoolKind::Image)
            .map_err(|e| match e {
                RouteError::UnknownModel(m) => ImageGenError::NoBackend(m),
                other => ImageGenError::Upstream(other.to_string()),
            })?;
        let real_model = acquired.resolved_model().to_string();
        let backend = acquired.backend();
        let backend_name = backend.name.clone();
        let url = format!("{}/images/generations", backend.base_url);

        // Deliberately NO `response_format`: it isn't portable. DALL·E accepts
        // `b64_json`/`url`, z.AI ignores it (always URL), and OpenAI
        // `gpt-image-1` *rejects* it outright (always b64). We let each backend
        // use its default and cope with whichever it returns in
        // `handle_response` (decode b64, or fetch a URL server-side).
        let mut body = json!({
            "model": real_model,
            "prompt": prompt,
        });
        if let Some(size) = size {
            body["size"] = json!(size);
        }

        let mut req = self.http.post(&url).timeout(GENERATE_TIMEOUT).json(&body);
        if let Some(key) = backend.api_key.as_deref() {
            req = req.bearer_auth(key);
        }

        let started = Instant::now();
        let resp = req
            .send()
            .await
            .map_err(|e| ImageGenError::Upstream(format!("request failed: {e}")))?;
        self.handle_response(resp, meta, &backend_name, &real_model, started, None)
            .await
    }

    /// Edit an existing image with a text instruction (image-to-image). Only
    /// works when a configured image backend advertises `supports_edit`, and
    /// — because it uploads *existing user content* to the provider — refuses
    /// to run against a backend whose pool is flagged non-GDPR-compliant.
    /// Posts an OpenAI `/images/edits`-shaped multipart body.
    pub async fn edit(
        &self,
        model: Option<&str>,
        image: Vec<u8>,
        image_mime: &str,
        prompt: &str,
        size: Option<&str>,
        meta: &UsageMeta,
    ) -> Result<GeneratedImage, ImageGenError> {
        let requested = match model {
            Some(m) => m.to_string(),
            None => self
                .default_model()
                .await
                .ok_or_else(|| ImageGenError::NoBackend(String::new()))?,
        };

        let acquired = self
            .upstreams
            .route(&requested, PoolKind::Image)
            .map_err(|e| match e {
                RouteError::UnknownModel(m) => ImageGenError::NoBackend(m),
                other => ImageGenError::Upstream(other.to_string()),
            })?;
        let real_model = acquired.resolved_model().to_string();
        let backend = acquired.backend();
        if !backend.supports_edit() {
            return Err(ImageGenError::EditUnsupported);
        }
        // Editing sends the user's *existing* image to the provider — a
        // stronger data-egress concern than a text prompt. Block it against a
        // non-GDPR-compliant (e.g. cloud) backend even if that backend claims
        // edit support.
        if !self.model_is_gdpr_compliant(&real_model) {
            return Err(ImageGenError::ComplianceBlocked);
        }
        let backend_name = backend.name.clone();
        let url = format!("{}/images/edits", backend.base_url);

        let file_name = format!(
            "image{}",
            crate::server::chat_attachments::ext_for_mime(image_mime).unwrap_or(".png")
        );
        let part = reqwest::multipart::Part::bytes(image)
            .file_name(file_name)
            .mime_str(image_mime)
            .map_err(|e| ImageGenError::Upstream(format!("bad image mime `{image_mime}`: {e}")))?;
        // No `response_format` field — same portability reason as `generate`
        // (gpt-image-1's edits endpoint rejects it; we handle b64 or URL).
        let mut form = reqwest::multipart::Form::new()
            .text("model", real_model.clone())
            .text("prompt", prompt.to_string())
            .part("image", part);
        if let Some(size) = size {
            form = form.text("size", size.to_string());
        }

        let mut req = self
            .http
            .post(&url)
            .timeout(GENERATE_TIMEOUT)
            .multipart(form);
        if let Some(key) = backend.api_key.as_deref() {
            req = req.bearer_auth(key);
        }

        let started = Instant::now();
        let resp = req
            .send()
            .await
            .map_err(|e| ImageGenError::Upstream(format!("request failed: {e}")))?;
        self.handle_response(
            resp,
            meta,
            &backend_name,
            &real_model,
            started,
            image_units(1),
        )
        .await
    }

    /// Shared tail for `generate`/`edit`: meter the call, then turn the OpenAI
    /// `/images/*` response into bytes (decoding `b64_json`, or fetching a
    /// returned `url` server-side).
    async fn handle_response(
        &self,
        resp: reqwest::Response,
        meta: &UsageMeta,
        backend_name: &str,
        real_model: &str,
        started: Instant,
        input_units: Option<f64>,
    ) -> Result<GeneratedImage, ImageGenError> {
        let status = resp.status();
        let (result, units) = if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            let detail = detail.chars().take(500).collect::<String>();
            (
                Err(ImageGenError::Upstream(format!("HTTP {status}: {detail}"))),
                UnitUsage::default(),
            )
        } else {
            match resp.json::<ImagesResponse>().await {
                Err(e) => (
                    Err(ImageGenError::BadResponse(format!("unparseable body: {e}"))),
                    UnitUsage::default(),
                ),
                Ok(parsed) => {
                    // One output unit per returned image, mirroring the raw proxy.
                    let output_units = image_units(parsed.data.len());
                    match parsed.data.into_iter().next() {
                        None => (
                            Err(ImageGenError::BadResponse("empty `data` array".into())),
                            UnitUsage::default(),
                        ),
                        Some(datum) => {
                            let result = match (datum.b64_json, datum.url) {
                                (Some(b64), _) => chat_attachments::decode_base64(&b64)
                                    .map_err(ImageGenError::BadResponse)
                                    .and_then(|bytes| {
                                        if bytes.len() > MAX_IMAGE_BYTES {
                                            return Err(ImageGenError::TooLarge);
                                        }
                                        let mime = sniff_image_mime(&bytes)
                                            .unwrap_or("image/png")
                                            .to_string();
                                        Ok(GeneratedImage { bytes, mime })
                                    }),
                                (None, Some(url)) => self.fetch_image_url(&url).await,
                                (None, None) => Err(ImageGenError::BadResponse(
                                    "datum has neither `b64_json` nor `url`".into(),
                                )),
                            };
                            (
                                result,
                                UnitUsage {
                                    input: input_units,
                                    output: output_units,
                                },
                            )
                        }
                    }
                }
            }
        };
        self.emit_usage(
            meta,
            backend_name,
            real_model,
            status.as_u16(),
            started,
            units,
        );
        result
    }

    /// Whether the image model's pool is GDPR-compliant (drives the edit gate).
    /// Unknown model → treat as non-compliant (fail safe).
    fn model_is_gdpr_compliant(&self, model: &str) -> bool {
        self.upstreams
            .models_with_compliance_for_kind(PoolKind::Image)
            .into_iter()
            .find(|(m, _)| m == model)
            .map(|(_, c)| c.gdpr)
            .unwrap_or(false)
    }

    /// The model id the image pool advertises, if exactly one is discoverable.
    /// Lets `generate_image` be called without an explicit `model` on the
    /// common single-model deployment.
    async fn default_model(&self) -> Option<String> {
        let mut models: Vec<String> = self.upstreams.models_for_kind(PoolKind::Image);
        models.sort();
        // Honour the operator-configured default when it's still being served;
        // otherwise fall back to the alphabetically-first model (the historical
        // behaviour) so a single-model deployment still needs no config.
        let configured = feature_defaults::get(&self.db, Feature::Image).await;
        feature_defaults::resolve(configured.as_deref(), &models)
    }

    /// GET an upstream-returned image URL server-side and validate it's an
    /// image within the size cap. The bytes are re-hosted by the caller; the
    /// URL never reaches the browser or the model.
    async fn fetch_image_url(&self, url: &str) -> Result<GeneratedImage, ImageGenError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| ImageGenError::BadResponse(format!("invalid image url `{url}`: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(ImageGenError::BadResponse(format!(
                "image url scheme `{}` is not http(s)",
                parsed.scheme()
            )));
        }

        let resp = self
            .http
            .get(parsed)
            .timeout(URL_FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| ImageGenError::Upstream(format!("fetching image url failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(ImageGenError::Upstream(format!(
                "image url returned HTTP {}",
                resp.status()
            )));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .trim()
            .to_string();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ImageGenError::Upstream(format!("reading image url body: {e}")))?
            .to_vec();

        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(ImageGenError::TooLarge);
        }
        // Content-type from the header is the primary signal; fall back to
        // sniffing the container bytes, and reject anything that isn't an image.
        let mime = if content_type.starts_with("image/") {
            content_type
        } else {
            sniff_image_mime(&bytes)
                .ok_or_else(|| {
                    ImageGenError::BadResponse(format!(
                        "image url served non-image content ({content_type})"
                    ))
                })?
                .to_string()
        };
        Ok(GeneratedImage { bytes, mime })
    }

    fn emit_usage(
        &self,
        meta: &UsageMeta,
        backend: &str,
        model: &str,
        status: u16,
        started: Instant,
        units: UnitUsage,
    ) {
        if !self.usage.is_enabled() {
            return;
        }
        self.usage.emit(UsageRecord {
            created_at: Timestamp::now(),
            user_id: meta.user_id.clone(),
            user_email: None,
            token_id: None,
            token_name: None,
            source: meta.source,
            kind: UsageKind::Image,
            backend: backend.to_string(),
            model: model.to_string(),
            status,
            duration_ms: started.elapsed().as_millis() as i64,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            input_units: units.input,
            output_units: units.output,
            enforce_limits: self
                .upstreams
                .enforce_limits_for_model(model, gateway_core::server::upstreams::PoolKind::Image),
        });
    }
}

/// Sniff a handful of common image container signatures so a `b64_json` image
/// (which carries no mime) or a mislabelled URL gets a correct content-type.
fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use serde_json::json as j;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::server::chat_attachments::to_data_uri;
    use gateway_core::server::upstreams::config::{
        BackendConfig, PickerStrategy, UpstreamPoolConfig,
    };

    // 1x1 transparent PNG (valid signature so sniffing yields image/png).
    const PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89,
    ];

    fn image_backend(base_url: &str) -> BackendConfig {
        BackendConfig {
            name: "img".into(),
            base_url: base_url.into(),
            api_key_env: None,
            api_key: None,
            weight: 1,
            max_inflight: 16,
            health_path: "/models".into(),
            models: vec!["test-image".into()],
            alias: None,
            probe_models: false,
            supports_edit: false,
        }
    }

    async fn generator(pool: Option<UpstreamPoolConfig>) -> ImageGenerator {
        let mut pools = HashMap::new();
        if let Some(p) = pool {
            pools.insert("img".to_string(), p);
        }
        let reg = UpstreamRegistry::new(&pools).unwrap();
        let db = gateway_core::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        ImageGenerator::new(reg, reqwest::Client::new(), UsageHandle::disabled(), db)
    }

    fn image_pool(base_url: &str) -> UpstreamPoolConfig {
        UpstreamPoolConfig {
            voices: Default::default(),
            allowed_groups: Vec::new(),
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Image,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            fallback_offline: None,
            backend: vec![image_backend(base_url)],
        }
    }

    /// An image pool whose backend advertises edit support, with tunable
    /// GDPR compliance (for the edit gate tests).
    fn edit_pool(base_url: &str, gdpr: bool) -> UpstreamPoolConfig {
        use gateway_core::server::upstreams::config::Compliance;
        let mut backend = image_backend(base_url);
        backend.supports_edit = true;
        UpstreamPoolConfig {
            voices: Default::default(),
            allowed_groups: Vec::new(),
            compliance: Compliance { gdpr, nda: true },
            enforce_limits: true,
            kind: PoolKind::Image,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            fallback_offline: None,
            backend: vec![backend],
        }
    }

    fn meta() -> UsageMeta {
        UsageMeta {
            user_id: "u".into(),
            source: UsageSource::Chat,
        }
    }

    fn b64_png() -> String {
        to_data_uri("image/png", PNG)
            .strip_prefix("data:image/png;base64,")
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn parses_b64_json_response() {
        let server = MockServer::start().await;
        let b64 = to_data_uri("image/png", PNG)
            .strip_prefix("data:image/png;base64,")
            .unwrap()
            .to_string();
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(j!({
                "data": [{ "b64_json": b64 }]
            })))
            .mount(&server)
            .await;

        let imggen = generator(Some(image_pool(&server.uri()))).await;
        let img = imggen
            .generate(Some("test-image"), "a cat", None, &meta())
            .await
            .expect("b64 image");
        assert_eq!(img.mime, "image/png");
        assert_eq!(img.bytes, PNG);
    }

    #[tokio::test]
    async fn fetches_url_response_server_side() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(j!({
                "data": [{ "url": format!("{}/img.png", server.uri()) }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/img.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(PNG),
            )
            .mount(&server)
            .await;

        let imggen = generator(Some(image_pool(&server.uri()))).await;
        let img = imggen
            .generate(Some("test-image"), "a dog", None, &meta())
            .await
            .expect("url image");
        assert_eq!(img.mime, "image/png");
        assert_eq!(img.bytes, PNG);
    }

    #[tokio::test]
    async fn errors_when_datum_has_neither_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(j!({ "data": [{}] })))
            .mount(&server)
            .await;
        let imggen = generator(Some(image_pool(&server.uri()))).await;
        let err = imggen
            .generate(Some("test-image"), "x", None, &meta())
            .await
            .unwrap_err();
        assert!(matches!(err, ImageGenError::BadResponse(_)), "{err:?}");
    }

    #[tokio::test]
    async fn errors_when_url_serves_non_image() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(j!({
                "data": [{ "url": format!("{}/nope.txt", server.uri()) }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/nope.txt"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/plain")
                    .set_body_string("not an image"),
            )
            .mount(&server)
            .await;
        let imggen = generator(Some(image_pool(&server.uri()))).await;
        let err = imggen
            .generate(Some("test-image"), "x", None, &meta())
            .await
            .unwrap_err();
        assert!(matches!(err, ImageGenError::BadResponse(_)), "{err:?}");
    }

    #[tokio::test]
    async fn errors_when_no_image_pool_configured() {
        let imggen = generator(None).await;
        let err = imggen
            .generate(Some("test-image"), "x", None, &meta())
            .await
            .unwrap_err();
        assert!(matches!(err, ImageGenError::NoBackend(_)), "{err:?}");
    }

    #[tokio::test]
    async fn propagates_upstream_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let imggen = generator(Some(image_pool(&server.uri()))).await;
        let err = imggen
            .generate(Some("test-image"), "x", None, &meta())
            .await
            .unwrap_err();
        assert!(matches!(err, ImageGenError::Upstream(_)), "{err:?}");
    }

    #[tokio::test]
    async fn edit_succeeds_on_compliant_edit_backend() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/edits"))
            .respond_with(ResponseTemplate::new(200).set_body_json(j!({
                "data": [{ "b64_json": b64_png() }]
            })))
            .mount(&server)
            .await;

        let imggen = generator(Some(edit_pool(&server.uri(), true))).await;
        let img = imggen
            .edit(
                Some("test-image"),
                PNG.to_vec(),
                "image/png",
                "make the sky blue",
                None,
                &meta(),
            )
            .await
            .expect("edited image");
        assert_eq!(img.mime, "image/png");
        assert_eq!(img.bytes, PNG);
    }

    #[tokio::test]
    async fn edit_rejected_when_backend_lacks_edit_support() {
        // Plain image_pool → supports_edit = false.
        let imggen = generator(Some(image_pool("http://unused.invalid"))).await;
        let err = imggen
            .edit(
                Some("test-image"),
                PNG.to_vec(),
                "image/png",
                "x",
                None,
                &meta(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ImageGenError::EditUnsupported), "{err:?}");
    }

    #[tokio::test]
    async fn edit_blocked_on_non_gdpr_backend() {
        // Edit-capable but non-GDPR-compliant → refuse to ship user image.
        let imggen = generator(Some(edit_pool("http://unused.invalid", false))).await;
        let err = imggen
            .edit(
                Some("test-image"),
                PNG.to_vec(),
                "image/png",
                "x",
                None,
                &meta(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ImageGenError::ComplianceBlocked), "{err:?}");
    }

    #[tokio::test]
    async fn edit_available_reflects_backend_flag() {
        assert!(
            !generator(Some(image_pool("http://x")))
                .await
                .edit_available()
        );
        assert!(
            generator(Some(edit_pool("http://x", true)))
                .await
                .edit_available()
        );
        assert!(!generator(None).await.edit_available());
    }
}
