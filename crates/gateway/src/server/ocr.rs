// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Internal document-OCR adapter.
//!
//! The first OCR backend is Baidu's Unlimited-OCR, exposed through an
//! OpenAI-compatible chat-completions endpoint. PDF rasterization stays in
//! [`crate::server::pdf`]; this module only owns the wire request and response
//! handling. OCR requests use the dedicated `ocr` upstream pool.

use serde_json::{Value, json};
use thiserror::Error;

use crate::server::chat_attachments::to_data_uri;
use crate::server::upstreams::{PoolKind, RouteError, UpstreamRegistry};

const OCR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20 * 60);
const MAX_PAGES_PER_REQUEST: usize = 64;

#[derive(Debug, Error)]
pub enum OcrError {
    #[error("routing OCR model `{model}` through the OCR pool: {source}")]
    Route {
        model: String,
        #[source]
        source: RouteError,
    },
    #[error("calling OCR upstream: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("OCR upstream returned status {status}: {body}")]
    UpstreamStatus { status: u16, body: String },
    #[error("parsing OCR response: {0}")]
    Parse(String),
    #[error("OCR request contains {got} pages; the limit is {max}")]
    TooManyPages { got: usize, max: usize },
}

#[derive(Debug, Clone)]
pub struct OcrPage {
    pub page_number: usize,
    pub png: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct OcrOptions {
    pub prompt: String,
    pub max_tokens: usize,
    pub ngram_window: usize,
}

impl Default for OcrOptions {
    fn default() -> Self {
        Self {
            prompt: "Document parsing.".to_string(),
            max_tokens: 32_768,
            ngram_window: 1_024,
        }
    }
}

/// Parse ordered PNG pages with an Unlimited-OCR-compatible upstream.
pub async fn recognize(
    http: &reqwest::Client,
    upstreams: &UpstreamRegistry,
    model: &str,
    pages: &[OcrPage],
    options: &OcrOptions,
) -> Result<String, OcrError> {
    if pages.len() > MAX_PAGES_PER_REQUEST {
        return Err(OcrError::TooManyPages {
            got: pages.len(),
            max: MAX_PAGES_PER_REQUEST,
        });
    }

    let acquired = upstreams
        .acquire_for(model, PoolKind::Ocr)
        .map_err(|source| OcrError::Route {
            model: model.to_string(),
            source,
        })?;
    let real_model = acquired.resolved_model().to_string();
    let backend = acquired.backend();
    let url = format!("{}/chat/completions", backend.base_url);
    let prompt = if pages.len() == 1 {
        format!("<image>{}", options.prompt)
    } else {
        "<image>Multi page parsing.".to_string()
    };
    let body = json!({
        "model": real_model,
        "messages": [{"role": "user", "content": build_content(&prompt, pages)}],
        "temperature": 0,
        "max_tokens": options.max_tokens,
        "skip_special_tokens": false,
        "vllm_xargs": {
            "ngram_size": 35,
            "window_size": options.ngram_window,
        },
    });

    let mut request = http.post(url).timeout(OCR_TIMEOUT).json(&body);
    if let Some(key) = backend.api_key.as_deref() {
        request = request.bearer_auth(key);
    }
    let response = request.send().await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    drop(acquired);

    if !status.is_success() {
        return Err(OcrError::UpstreamStatus {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).into_owned(),
        });
    }
    parse_content(&bytes)
}

fn build_content(prompt: &str, pages: &[OcrPage]) -> Vec<Value> {
    let mut content = Vec::with_capacity(pages.len() + 1);
    content.push(json!({
        "type": "text",
        "text": prompt,
    }));
    content.extend(pages.iter().map(|page| {
        json!({
            "type": "image_url",
            "image_url": {
                "url": to_data_uri("image/png", &page.png),
            },
        })
    }));
    content
}

fn parse_content(bytes: &[u8]) -> Result<String, OcrError> {
    let body: Value = serde_json::from_slice(bytes)
        .map_err(|err| OcrError::Parse(format!("invalid JSON: {err}")))?;
    let content = body
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())
        .ok_or_else(|| OcrError::Parse("response contains no text content".to_string()))?;
    Ok(clean_grounding_tokens(content))
}

fn clean_grounding_tokens(mut content: &str) -> String {
    let mut cleaned = String::with_capacity(content.len());
    while let Some(start) = content.find("<|det|>") {
        cleaned.push_str(&content[..start]);
        content = &content[start + "<|det|>".len()..];
        if let Some(end) = content.find("<|/det|>") {
            content = &content[end + "<|/det|>".len()..];
        } else {
            content = "";
            break;
        }
    }
    cleaned.push_str(content);
    cleaned.replace("<|ref|>", "").replace("<|/ref|>", "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::upstreams::UpstreamRegistry;
    use crate::server::upstreams::config::{BackendConfig, PickerStrategy, UpstreamPoolConfig};
    use serde_json::json;
    use std::collections::HashMap;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn registry(url: &str) -> std::sync::Arc<UpstreamRegistry> {
        let mut pools = HashMap::new();
        pools.insert(
            "ocr".to_string(),
            UpstreamPoolConfig {
                kind: PoolKind::Ocr,
                strategy: PickerStrategy::LeastInflight,
                backend: vec![BackendConfig {
                    name: "ocr".into(),
                    base_url: format!("{url}/v1"),
                    api_key_env: None,
                    api_key: None,
                    weight: 1,
                    max_inflight: 2,
                    health_path: "/models".into(),
                    models: vec!["unlimited-ocr".into()],
                    alias: None,
                    probe_models: false,
                    supports_edit: false,
                }],
                models: vec![],
                fallback_offline: None,
                compliance: Default::default(),
                enforce_limits: true,
                allowed_groups: Vec::new(),
                voices: HashMap::new(),
            },
        );
        UpstreamRegistry::new(&pools).expect("test registry should build")
    }

    #[tokio::test]
    async fn recognize_sends_ordered_images_and_returns_markdown() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_partial_json(json!({
                "model": "unlimited-ocr",
                "skip_special_tokens": false,
                "vllm_xargs": {"ngram_size": 35, "window_size": 1024}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "# Page one\n\nText"}}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let output = recognize(
            &client,
            &registry(&server.uri()),
            "unlimited-ocr",
            &[
                OcrPage {
                    page_number: 1,
                    png: vec![1, 2, 3],
                },
                OcrPage {
                    page_number: 2,
                    png: vec![4, 5, 6],
                },
            ],
            &OcrOptions::default(),
        )
        .await
        .expect("OCR response should parse");

        assert_eq!(output, "# Page one\n\nText");
        server.verify().await;
    }

    #[test]
    fn parse_content_rejects_missing_text() {
        let err = parse_content(br#"{"choices":[{"message":{}}]}"#).unwrap_err();
        assert!(matches!(err, OcrError::Parse(message) if message.contains("no text")));
    }

    #[test]
    fn clean_grounding_tokens_keeps_references_and_drops_coordinates() {
        assert_eq!(
            clean_grounding_tokens("<|ref|>Invoice<|/ref|> <|det|>1,2,3<|/det|>"),
            "Invoice "
        );
    }
}
