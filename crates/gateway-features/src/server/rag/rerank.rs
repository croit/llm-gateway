// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Cross-encoder reranking: a second, more expensive opinion on the
//! candidates hybrid retrieval already found.
//!
//! Hybrid search scores a query and a passage *independently* — the embedding
//! of a chunk is computed once, at index time, with no knowledge of what will
//! be asked. That is what makes it fast enough to run over a whole corpus,
//! and also what makes it blunt on documents that look alike. Three thousand
//! invoices share a layout, a vocabulary and most of their words; the thing
//! that distinguishes the right one is a relationship between the *question*
//! and the *passage* that neither vector saw.
//!
//! A cross-encoder sees both at once and scores the pair. It cannot run over
//! a corpus — that is a model call per passage — but it runs comfortably over
//! the few dozen candidates fusion already narrowed to. So retrieval widens
//! its net, and this re-sorts what came back.
//!
//! Entirely optional, and silently so: with no `rerank` pool configured,
//! search returns the fused ranking exactly as before. A reranker that errors
//! or times out is a warning in the log, never a failed search — degraded
//! ordering beats no answer.

use gateway_core::server::upstreams::{PoolKind, UpstreamRegistry};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RerankError {
    #[error("no rerank model available: {0}")]
    NoModel(String),
    #[error("rerank request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("rerank backend returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("rerank backend sent an unusable response: {0}")]
    Parse(String),
}

/// The de-facto response shape for `/rerank`, as served by TEI,
/// Infinity and vLLM's scoring endpoint: one entry per input document,
/// carrying its original index and a relevance score.
#[derive(Debug, Deserialize)]
struct RerankResult {
    index: usize,
    #[serde(alias = "score")]
    relevance_score: f32,
}

#[derive(Debug, Deserialize)]
struct RerankResponse {
    /// Some backends wrap the list in `results`, others return it bare; the
    /// caller-side parse below handles both.
    #[serde(default)]
    results: Vec<RerankResult>,
}

/// The model to rerank with: the configured one, or the first the `rerank`
/// pool advertises. `None` means the feature is simply off.
pub fn model(upstreams: &UpstreamRegistry, configured: Option<&str>) -> Option<String> {
    let candidate = match configured.filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        None => upstreams
            .models_for_kind(PoolKind::Rerank)
            .into_iter()
            .next()?,
    };
    // Routable *and* healthy — `acquire_for` answers both, and the permit is
    // released immediately.
    upstreams
        .acquire_for(&candidate, PoolKind::Rerank)
        .ok()
        .map(|_| candidate)
}

/// Score `documents` against `query`, returning `(original_index, score)`
/// best-first.
///
/// Only indices the backend actually scored come back: a backend that
/// returns fewer results than it was given has dropped some, and inventing a
/// score for those would silently sink real hits to the bottom.
pub async fn rerank(
    http: &reqwest::Client,
    upstreams: &UpstreamRegistry,
    model: &str,
    query: &str,
    documents: &[String],
) -> Result<Vec<(usize, f32)>, RerankError> {
    if documents.is_empty() {
        return Ok(Vec::new());
    }
    let acquired = upstreams
        .acquire_for(model, PoolKind::Rerank)
        .map_err(|e| RerankError::NoModel(e.to_string()))?;
    let real_model = acquired.resolved_model().to_string();
    let backend = acquired.backend();
    let url = format!("{}/rerank", backend.base_url);
    let mut req = http.post(&url).json(&json!({
        "model": real_model,
        "query": query,
        "documents": documents,
        // The gateway does its own truncation to `k`; asking the backend to
        // also trim would silently drop candidates on backends that default
        // `top_n` to something small.
        "top_n": documents.len(),
    }));
    if let Some(key) = backend.api_key.as_deref() {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(RerankError::Status {
            status: status.as_u16(),
            body: resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(400)
                .collect(),
        });
    }
    let value: serde_json::Value = resp.json().await?;
    let mut scored = parse_results(&value, documents.len())?;
    // Descending by score; ties break by original position so the fused
    // ranking still shows through when the reranker cannot separate two.
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    Ok(scored)
}

/// Pull `(index, score)` pairs out of either response shape, discarding
/// entries that point outside the batch we sent.
fn parse_results(value: &serde_json::Value, sent: usize) -> Result<Vec<(usize, f32)>, RerankError> {
    let list: Vec<RerankResult> = if value.is_array() {
        serde_json::from_value(value.clone()).map_err(|e| RerankError::Parse(e.to_string()))?
    } else {
        let wrapped: RerankResponse =
            serde_json::from_value(value.clone()).map_err(|e| RerankError::Parse(e.to_string()))?;
        wrapped.results
    };
    if list.is_empty() {
        return Err(RerankError::Parse(
            "the response contained no scored documents".into(),
        ));
    }
    Ok(list
        .into_iter()
        // An out-of-range index would panic the caller's lookup; a backend
        // that sends one is misbehaving, and dropping the entry is the
        // conservative reading.
        .filter(|r| r.index < sent)
        .map(|r| (r.index, r.relevance_score))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_wrapped_response_shape_parses() {
        let v = json!({"results": [
            {"index": 2, "relevance_score": 0.9},
            {"index": 0, "relevance_score": 0.1}
        ]});
        let out = parse_results(&v, 3).unwrap();
        assert_eq!(out, vec![(2, 0.9), (0, 0.1)]);
    }

    #[test]
    fn a_bare_array_response_parses_too() {
        // Not every backend wraps the list; losing every rerank to that
        // would be a silly way to fail.
        let v = json!([{"index": 1, "relevance_score": 0.5}]);
        assert_eq!(parse_results(&v, 2).unwrap(), vec![(1, 0.5)]);
    }

    #[test]
    fn the_score_alias_is_accepted() {
        let v = json!({"results": [{"index": 0, "score": 0.7}]});
        assert_eq!(parse_results(&v, 1).unwrap(), vec![(0, 0.7)]);
    }

    #[test]
    fn an_index_outside_the_batch_is_dropped_not_trusted() {
        let v = json!({"results": [
            {"index": 0, "relevance_score": 0.5},
            {"index": 99, "relevance_score": 0.9}
        ]});
        let out = parse_results(&v, 1).unwrap();
        assert_eq!(
            out,
            vec![(0, 0.5)],
            "a bogus index must not reach the caller"
        );
    }

    #[test]
    fn an_empty_result_list_is_an_error_not_a_silent_wipe() {
        // Returning an empty ranking would drop every hit the search found.
        let v = json!({"results": []});
        assert!(parse_results(&v, 3).is_err());
    }

    #[test]
    fn a_garbage_response_is_an_error() {
        assert!(parse_results(&json!({"nonsense": true}), 1).is_err());
    }
}
