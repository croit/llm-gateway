// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! In-process embedding call.
//!
//! The RAG indexer (and any other in-tree caller) needs vectors from a
//! configured embedding pool *without* paying the cost of a loopback HTTP
//! round-trip — bearer minting, auth middleware, byte-dumb forward, and
//! response re-parsing. [`embed`] does the same routing the
//! `/v1/embeddings` handler does (`UpstreamRegistry::acquire_for(model,
//! PoolKind::Embedding)`), then POSTs the OpenAI-shape `/embeddings` body
//! directly with the shared reqwest client and parses the response into
//! `Vec<Vec<f32>>` ordered by input index.
//!
//! Acquire→drop is RAII the same way `proxy::forward` does it: the
//! inflight slot is held for the duration of the upstream call.

use serde::Deserialize;
use thiserror::Error;

use crate::server::upstreams::{Acquired, RouteError, UpstreamRegistry, config::PoolKind};

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("routing model `{model}` to an embedding pool: {source}")]
    Route {
        model: String,
        #[source]
        source: RouteError,
    },
    #[error("calling embedding upstream: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("embedding upstream returned status {status}: {body}")]
    UpstreamStatus { status: u16, body: String },
    #[error("parsing embedding response: {0}")]
    Parse(serde_json::Error),
    #[error("embedding upstream returned {got} vectors for {expected} inputs")]
    CountMismatch { expected: usize, got: usize },
    #[error("embedding upstream returned out-of-range index {index} for {expected} inputs")]
    IndexOutOfRange { index: usize, expected: usize },
}

/// OpenAI-shape `/embeddings` request body the helper sends. Mirrors the
/// minimum the upstream needs; `encoding_format`, `dimensions`, `user` are
/// deliberately not threaded — add them when a caller needs them.
#[derive(serde::Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: Vec<f32>,
}

/// Embed `inputs` using `model` against the gateway's configured embedding
/// pool. Returns vectors in the same order as `inputs`. Empty input → empty
/// output (no upstream call).
pub async fn embed(
    http: &reqwest::Client,
    upstreams: &UpstreamRegistry,
    model: &str,
    inputs: &[String],
) -> Result<Vec<Vec<f32>>, EmbedError> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    let acquired = upstreams
        .acquire_for(model, PoolKind::Embedding)
        .map_err(|source| EmbedError::Route {
            model: model.into(),
            source,
        })?;
    let result = post(http, &acquired, model, inputs).await;
    drop(acquired);
    result
}

async fn post(
    http: &reqwest::Client,
    acquired: &Acquired,
    model: &str,
    inputs: &[String],
) -> Result<Vec<Vec<f32>>, EmbedError> {
    let backend = acquired.backend();
    let url = format!("{}/embeddings", backend.base_url);
    let mut req = http.post(&url).json(&EmbeddingsRequest {
        model,
        input: inputs,
    });
    if let Some(key) = backend.api_key.as_deref() {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(EmbedError::UpstreamStatus {
            status: status.as_u16(),
            body,
        });
    }
    let bytes = resp.bytes().await?;
    let parsed: EmbeddingsResponse = serde_json::from_slice(&bytes).map_err(EmbedError::Parse)?;
    if parsed.data.len() != inputs.len() {
        return Err(EmbedError::CountMismatch {
            expected: inputs.len(),
            got: parsed.data.len(),
        });
    }
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); inputs.len()];
    for item in parsed.data {
        if item.index >= inputs.len() {
            return Err(EmbedError::IndexOutOfRange {
                index: item.index,
                expected: inputs.len(),
            });
        }
        out[item.index] = item.embedding;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;
    use crate::server::upstreams::config::{
        BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig,
    };
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn registry_with_embedding_pool(
        upstream_url: &str,
        model: &str,
    ) -> std::sync::Arc<UpstreamRegistry> {
        let mut pools = HashMap::new();
        pools.insert(
            "embed".to_string(),
            UpstreamPoolConfig {
                kind: PoolKind::Embedding,
                strategy: PickerStrategy::RoundRobin,
                models: Vec::new(),
                backend: vec![BackendConfig {
                    name: "mock".into(),
                    base_url: upstream_url.into(),
                    api_key_env: None,
                    weight: 1,
                    max_inflight: 16,
                    health_path: "/models".into(),
                    models: Vec::new(),
                }],
            },
        );
        let registry = UpstreamRegistry::new(&pools).unwrap();
        // Skip the health probe in tests — seed the advertised set directly.
        let pool = registry.pools().find(|p| p.name == "embed").unwrap();
        pool.backends[0].set_models(HashSet::from([model.to_string()]));
        registry
    }

    #[tokio::test]
    async fn embed_returns_vectors_in_input_order_even_when_response_is_shuffled() {
        let upstream = MockServer::start().await;
        // Wiremock returns indices 1 then 0 — `embed` must reorder by `index`.
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(body_partial_json(json!({"model": "embed-model"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "model": "embed-model",
                "data": [
                    { "object": "embedding", "index": 1, "embedding": [0.4, 0.5, 0.6] },
                    { "object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3] },
                ],
            })))
            .mount(&upstream)
            .await;

        let registry = registry_with_embedding_pool(&upstream.uri(), "embed-model");
        let http = reqwest::Client::new();
        let out = embed(
            &http,
            &registry,
            "embed-model",
            &["first".to_string(), "second".to_string()],
        )
        .await
        .unwrap();
        assert_eq!(out, vec![vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]]);
    }

    #[tokio::test]
    async fn embed_short_circuits_on_empty_input_without_upstream_call() {
        // Backend points at a black hole; if we touched it the test would
        // fail (connection refused).
        let registry = registry_with_embedding_pool("http://127.0.0.1:1", "embed-model");
        let http = reqwest::Client::new();
        let out = embed(&http, &registry, "embed-model", &[]).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn embed_unknown_model_returns_route_error() {
        let registry = registry_with_embedding_pool("http://unused.invalid", "embed-model");
        let http = reqwest::Client::new();
        let err = embed(&http, &registry, "no-such-model", &["x".to_string()])
            .await
            .unwrap_err();
        match err {
            EmbedError::Route { model, source } => {
                assert_eq!(model, "no-such-model");
                assert!(matches!(source, RouteError::UnknownModel(_)));
            }
            other => panic!("expected Route(UnknownModel), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn embed_surfaces_upstream_5xx_with_body() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(503).set_body_string("backend down"))
            .mount(&upstream)
            .await;
        let registry = registry_with_embedding_pool(&upstream.uri(), "embed-model");
        let http = reqwest::Client::new();
        let err = embed(&http, &registry, "embed-model", &["x".to_string()])
            .await
            .unwrap_err();
        match err {
            EmbedError::UpstreamStatus { status, body } => {
                assert_eq!(status, 503);
                assert_eq!(body, "backend down");
            }
            other => panic!("expected UpstreamStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn embed_rejects_response_with_wrong_vector_count() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "model": "embed-model",
                "data": [
                    { "object": "embedding", "index": 0, "embedding": [0.1] }
                ],
            })))
            .mount(&upstream)
            .await;
        let registry = registry_with_embedding_pool(&upstream.uri(), "embed-model");
        let http = reqwest::Client::new();
        let err = embed(
            &http,
            &registry,
            "embed-model",
            &["a".to_string(), "b".to_string()],
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            EmbedError::CountMismatch {
                expected: 2,
                got: 1
            }
        ));
    }
}
