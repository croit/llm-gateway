// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Fetches a single URL via HTTP GET and returns the body. The
//! return shape mirrors `fetch_attachment` so the model sees the
//! same contract regardless of where bytes came from:
//!
//! - **Text-ish** (text/*, JSON/YAML/HTML/CSV/Markdown/code/…):
//!   decoded UTF-8 in `content`.
//! - **Image** (image/*): `data:` URI returned as a typed
//!   `image_url` part via `tool_content_parts(...)` — vision
//!   models can actually look at it.
//! - **Other binary** (PDF, zip, audio, …): metadata only, with a
//!   note. The caller knows the bytes exist but can't read them
//!   inline.
//!
//! No SSRF guard by design — anything reachable from the gateway
//! is fair game; if you have internal services on the same network
//! that don't authenticate, that's a deployment problem, not a
//! gateway one (per the operator's explicit policy decision).

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture, tool_content_parts};
use crate::server::chat_attachments::{self, BinaryDisposition, PayloadLimits};

/// Hard cap on the response body we keep — generous (4 MB) so we
/// don't aggressively truncate real documentation pages, while
/// still bounding per-call memory. Shared ceiling with
/// `fetch_attachment` so the two tools have identical limits.
const HARD_MAX_BYTES: usize = 4 * 1024 * 1024;
const HARD_MAX_BYTES_DEFAULT: usize = HARD_MAX_BYTES;
/// Image ceiling — matches `fetch_attachment` so the model sees the
/// same limits regardless of where bytes came from. 25 MB covers
/// phone photos and screenshots; anything larger degrades to a
/// `kind: "image-too-large"` metadata response.
const MAX_IMAGE_BYTES: usize = 25 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

pub struct FetchUrl;

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
    /// Optional cap on bytes returned. Clamped to `HARD_MAX_BYTES`.
    #[serde(default)]
    max_bytes: Option<usize>,
}

impl Tool for FetchUrl {
    fn id(&self) -> &str {
        "fetch_url"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Fetch a URL with an HTTP GET and return the response body. \
             Text-ish responses (HTML, JSON, plain text, code, etc.) come \
             back as decoded UTF-8 in `content`. Images come back as a \
             visible `image_url` part you can look at. Other binary content \
             (PDF, zip, audio, …) returns metadata only; ask the user to \
             provide the file directly if you need its bytes.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["url"],
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute http:// or https:// URL to fetch."
                    },
                    "max_bytes": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": HARD_MAX_BYTES,
                        "description": "Optional cap on bytes returned for \
                                        text content. Defaults to the full \
                                        response body up to 4 MB (the hard cap)."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, _ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: FetchArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{url, max_bytes?}}: {e}"))
            })?;

            // Reject anything that isn't http(s) up front. Saves a
            // round-trip and prevents `file://`, `data:`, etc. from
            // reaching the HTTP client.
            let url = url::Url::parse(&args.url)
                .map_err(|e| ToolError::InvalidArgs(format!("invalid URL `{}`: {e}", args.url)))?;
            match url.scheme() {
                "http" | "https" => {}
                other => {
                    return Err(ToolError::InvalidArgs(format!(
                        "unsupported URL scheme `{other}` — only http and https"
                    )));
                }
            }

            let cap = args
                .max_bytes
                .unwrap_or(HARD_MAX_BYTES_DEFAULT)
                .min(HARD_MAX_BYTES);

            let client = reqwest::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .user_agent(concat!(
                    "llm-gateway/",
                    env!("CARGO_PKG_VERSION"),
                    " fetch_url"
                ))
                .build()
                .map_err(|e| ToolError::Failed(format!("HTTP client build: {e}")))?;

            let resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| ToolError::Failed(format!("fetch failed: {e}")))?;
            let status = resp.status().as_u16();
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
            let final_url = resp.url().to_string();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| ToolError::Failed(format!("read body: {e}")))?
                .to_vec();

            // No filename to disambiguate octet-stream — the URL
            // path could carry one, but `classify_payload` falls
            // back to mime-only when filename is empty, which is
            // the right call for HTTP fetches.
            let limits = PayloadLimits {
                max_text_bytes: cap,
                max_image_bytes: MAX_IMAGE_BYTES,
            };
            match chat_attachments::classify_payload(&content_type, "", bytes, limits) {
                BinaryDisposition::Text {
                    content,
                    bytes_returned,
                    truncated,
                    original_len,
                } => Ok(json!({
                    "url": final_url,
                    "status": status,
                    "content_type": content_type,
                    "kind": "text",
                    "content": content,
                    "bytes_returned": bytes_returned,
                    "bytes_original": original_len,
                    "truncated": truncated,
                })),
                BinaryDisposition::Image {
                    data_uri,
                    original_len,
                } => {
                    let summary = format!(
                        "Fetched image from {final_url} (HTTP {status}, {content_type}, \
                         {original_len} bytes)."
                    );
                    Ok(tool_content_parts(vec![
                        json!({"type": "text", "text": summary}),
                        json!({"type": "image_url", "image_url": {"url": data_uri}}),
                    ]))
                }
                BinaryDisposition::Binary { original_len } => {
                    // Same two-case split as `fetch_attachment`:
                    // over-cap images land here and deserve a
                    // dedicated `kind` so the model knows the
                    // bytes exist but were too big to inline.
                    let (kind, note) = if content_type.starts_with("image/") {
                        (
                            "image-too-large",
                            format!(
                                "Image is {original_len} bytes; ceiling is \
                                 {MAX_IMAGE_BYTES} bytes for inline return. \
                                 Try a downscaled URL if you have one."
                            ),
                        )
                    } else {
                        (
                            "binary",
                            "Non-image binary response — bytes can't be \
                             returned via a tool result. If you need this \
                             file's contents, ask the user to provide it \
                             directly."
                                .to_string(),
                        )
                    };
                    Ok(json!({
                        "url": final_url,
                        "status": status,
                        "content_type": content_type,
                        "size": original_len,
                        "kind": kind,
                        "note": note,
                    }))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn ctx() -> ToolContext {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: pool,
            s3: None,
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
        }
    }

    #[tokio::test]
    async fn html_response_lands_in_content_field() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "<html><body>hi</body></html>".as_bytes(),
                "text/html; charset=utf-8",
            ))
            .mount(&server)
            .await;
        let url = format!("{}/page", server.uri());
        let out = FetchUrl
            .run(ctx().await, json!({"url": url}))
            .await
            .unwrap();
        assert_eq!(out["status"], 200);
        assert_eq!(out["kind"], "text");
        assert_eq!(out["content_type"], "text/html");
        assert!(out["content"].as_str().unwrap().contains("hi"));
        assert_eq!(out["truncated"], false);
    }

    #[tokio::test]
    async fn json_response_returned_as_text_content() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"ok":true}"#.as_bytes(), "application/json"),
            )
            .mount(&server)
            .await;
        let url = format!("{}/api", server.uri());
        let out = FetchUrl
            .run(ctx().await, json!({"url": url}))
            .await
            .unwrap();
        assert_eq!(out["kind"], "text");
        assert!(out["content"].as_str().unwrap().contains("\"ok\":true"));
    }

    #[tokio::test]
    async fn image_response_comes_back_as_image_url_part() {
        let server = MockServer::start().await;
        // Minimal valid PNG (8-byte signature + IHDR chunk).
        let png_bytes: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            b'I', b'H', b'D', b'R', 0x00, 0x00, 0x00, 0x01, // width = 1
            0x00, 0x00, 0x00, 0x01, // height = 1
            0x08, 0x06, 0x00, 0x00, 0x00, // bit depth, color type, etc.
            0x1F, 0x15, 0xC4, 0x89, // CRC
        ];
        Mock::given(method("GET"))
            .and(path("/logo.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(png_bytes.clone())
                    .insert_header("content-type", "image/png"),
            )
            .mount(&server)
            .await;
        let url = format!("{}/logo.png", server.uri());
        let out = FetchUrl
            .run(ctx().await, json!({"url": url}))
            .await
            .unwrap();
        // tool_content_parts envelope: the driver detects this and
        // splices the parts into the upstream `role:"tool"` message
        // as an array — what gets bridged to the LLM as an actual
        // image, not a garbage UTF-8 string.
        let parts = crate::server::tools::extract_content_parts(&out)
            .expect("image branch must emit content-parts envelope");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1]["type"], "image_url");
        let uri = parts[1]["image_url"]["url"].as_str().unwrap();
        assert!(uri.starts_with("data:image/png;base64,"), "got: {uri}");
    }

    #[tokio::test]
    async fn pdf_response_returns_binary_metadata() {
        let server = MockServer::start().await;
        let pdf_bytes: Vec<u8> = b"%PDF-1.4\n%fake\n".to_vec();
        Mock::given(method("GET"))
            .and(path("/doc.pdf"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(pdf_bytes)
                    .insert_header("content-type", "application/pdf"),
            )
            .mount(&server)
            .await;
        let url = format!("{}/doc.pdf", server.uri());
        let out = FetchUrl
            .run(ctx().await, json!({"url": url}))
            .await
            .unwrap();
        assert_eq!(out["kind"], "binary");
        assert_eq!(out["content_type"], "application/pdf");
        // The model gets a clear "you can't read this" signal
        // instead of garbage UTF-8 inside `body`.
        assert!(
            out["note"].as_str().unwrap().contains("can't be returned"),
            "{out:?}"
        );
        assert!(
            out.get("content").is_none(),
            "should not include text content"
        );
    }

    #[tokio::test]
    async fn oversized_image_degrades_to_metadata_with_clear_kind() {
        // Build a synthetic image body larger than MAX_IMAGE_BYTES.
        // We don't actually base64-encode it (the classifier
        // short-circuits to Binary before that) so the test stays
        // fast even with a 25 MB+ buffer.
        let server = MockServer::start().await;
        let oversized: Vec<u8> = vec![0u8; MAX_IMAGE_BYTES + 1];
        Mock::given(method("GET"))
            .and(path("/huge.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(oversized)
                    .insert_header("content-type", "image/png"),
            )
            .mount(&server)
            .await;
        let url = format!("{}/huge.png", server.uri());
        let out = FetchUrl
            .run(ctx().await, json!({"url": url}))
            .await
            .unwrap();
        // Not a content-parts envelope — the image branch refused
        // to inline a 25 MB+ PNG and gave the model a metadata
        // record with a precise `kind`.
        assert!(crate::server::tools::extract_content_parts(&out).is_none());
        assert_eq!(out["kind"], "image-too-large");
        assert_eq!(out["content_type"], "image/png");
        assert!(
            out["note"].as_str().unwrap().contains("ceiling"),
            "note should explain why: {}",
            out["note"]
        );
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let err = FetchUrl
            .run(ctx().await, json!({"url": "file:///etc/passwd"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }

    #[tokio::test]
    async fn honours_max_bytes_for_text_responses() {
        let server = MockServer::start().await;
        let big = "x".repeat(5_000);
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(big.as_bytes(), "text/plain"))
            .mount(&server)
            .await;
        let url = format!("{}/big", server.uri());
        let out = FetchUrl
            .run(ctx().await, json!({"url": url, "max_bytes": 1024}))
            .await
            .unwrap();
        assert_eq!(out["bytes_returned"], 1024);
        assert_eq!(out["bytes_original"], 5_000);
        assert_eq!(out["truncated"], true);
    }

    #[tokio::test]
    async fn surfaces_upstream_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404).set_body_raw("nope".as_bytes(), "text/plain"))
            .mount(&server)
            .await;
        let url = format!("{}/missing", server.uri());
        let out = FetchUrl
            .run(ctx().await, json!({"url": url}))
            .await
            .unwrap();
        assert_eq!(out["status"], 404);
    }

    #[test]
    fn schema_names_match_id() {
        assert_eq!(FetchUrl.id(), FetchUrl.schema().function.name);
    }
}
