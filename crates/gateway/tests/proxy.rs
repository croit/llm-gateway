// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Bearer-gated /v1/* proxy routes. Covers the auth boundary, header
//! policy, model resolution, and response relay against a wiremock
//! upstream.

mod common;

use common::Service as _;
use gateway::server::upstreams::PoolKind;
use rama::http::{Body, Method, Request, StatusCode};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn v1_models_without_bearer_is_401() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/v1/models"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let www = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        www.contains("Bearer"),
        "missing WWW-Authenticate header: got `{www}`"
    );
}

#[tokio::test]
async fn v1_models_relays_upstream_list() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"id": "model-a", "object": "model"}]
        })))
        .mount(&upstream)
        .await;

    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/models")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::read_body(resp).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["data"][0]["id"], "model-a");
}

#[tokio::test]
async fn v1_chat_completions_relays_through_upstream() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "hi from upstream"}}]
        })))
        .mount(&upstream)
        .await;

    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let body = json!({"model": "model-a", "messages": []}).to_string();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = common::read_body(resp).await;
    // Streaming relay → bytes are upstream-shaped JSON, byte-for-byte.
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        parsed["choices"][0]["message"]["content"],
        "hi from upstream"
    );
}

#[tokio::test]
async fn v1_embeddings_relays_through_upstream() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "model": "embed-model",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}],
        })))
        .mount(&upstream)
        .await;

    let state = common::state_with_pool(&upstream.uri(), PoolKind::Embedding, "embed-model").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let body = json!({"model": "embed-model", "input": ["Schreibe einen Brief", "Write a letter"]})
        .to_string();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/embeddings")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = common::read_body(resp).await;
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed["data"][0]["embedding"][0], 0.1);
}

#[tokio::test]
async fn v1_embeddings_without_bearer_is_401() {
    let state =
        common::state_with_pool("http://unused.invalid", PoolKind::Embedding, "embed-model").await;
    let app = common::app(state);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"embed-model","input":["x"]}"#))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn v1_embeddings_missing_model_field_is_400() {
    let state =
        common::state_with_pool("http://unused.invalid", PoolKind::Embedding, "embed-model").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/embeddings")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"input":["x"]}"#))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn v1_embeddings_unknown_model_is_404_model_not_found() {
    let state =
        common::state_with_pool("http://unused.invalid", PoolKind::Embedding, "embed-model").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/embeddings")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"no-such-model","input":["x"]}"#))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// --- pool-kind routing isolation -------------------------------------------
// A model is reachable ONLY through its own pool kind's endpoint. These pin
// that `acquire_for(model, kind)` filters by kind, so an embedding model can't
// be driven through /v1/chat/completions and vice-versa — even though both
// models are advertised in /v1/models.

#[tokio::test]
async fn chat_endpoint_rejects_embedding_model_with_404() {
    let state = common::state_with_chat_and_embed("chat-model", "embed-model").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    // The embedding model IS known to the gateway (listed in /v1/models)…
    let list = app
        .serve(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/models")
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let models: serde_json::Value = serde_json::from_slice(&common::read_body(list).await).unwrap();
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"embed-model") && ids.contains(&"chat-model"));

    // …but it must NOT be routable as a chat model.
    let resp = app
        .serve(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {bearer}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"embed-model","messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "embedding model must not be usable on /v1/chat/completions"
    );
}

#[tokio::test]
async fn embeddings_endpoint_rejects_chat_model_with_404() {
    let state = common::state_with_chat_and_embed("chat-model", "embed-model").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let resp = app
        .serve(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/embeddings")
                .header("authorization", format!("Bearer {bearer}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"chat-model","input":["hi"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "chat model must not be usable on /v1/embeddings"
    );
}

#[tokio::test]
async fn v1_chat_completions_with_missing_model_field_is_400() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"messages":[]}"#))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn v1_chat_completions_with_unknown_model_is_404_model_not_found() {
    // OpenAI parity: a model no backend serves is a client error (404
    // `model_not_found`), not a transient 503 — so clients surface a config
    // problem instead of silently retrying.
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let body = json!({"model": "not-routed", "messages": []}).to_string();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["error"]["code"], "model_not_found");
    assert_eq!(parsed["error"]["type"], "invalid_request_error");
    assert_eq!(parsed["error"]["param"], "model");
    assert!(
        parsed["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("not-routed"),
        "expected error to name the unknown model: {parsed}"
    );
}

#[tokio::test]
async fn v1_chat_completions_known_model_all_replicas_down_is_503() {
    // The model IS known, but every replica is unhealthy → transient 503,
    // NOT 404. This is the distinction the OpenAI contract draws.
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    for pool in state.upstreams.pools() {
        for backend in &pool.backends {
            backend.set_healthy(false);
        }
    }
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let body = json!({"model": "model-a", "messages": []}).to_string();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["error"]["code"], "upstream_unreachable");
}

#[tokio::test]
async fn v1_models_lists_all_pools_deduped_with_full_objects() {
    // Lists EVERY pool/kind, de-duplicated by id, even when a backend (the
    // transcription one) never reported a `/models` probe — its id comes
    // from the pool's config fallback.
    let state = common::state_with_chat_and_config_transcription(
        "Qwen/Qwen3.6-35B-A3B-FP8",
        "mistralai/Voxtral-Mini-4B-Realtime-2602",
    )
    .await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/models")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["object"], "list");
    let data = parsed["data"].as_array().expect("data array");
    let ids: Vec<&str> = data
        .iter()
        .map(|m| m["id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        ids.contains(&"Qwen/Qwen3.6-35B-A3B-FP8"),
        "chat model missing: {ids:?}"
    );
    assert!(
        ids.contains(&"mistralai/Voxtral-Mini-4B-Realtime-2602"),
        "transcription (config-fallback) model missing: {ids:?}"
    );
    // Two chat replicas serve the same id → exactly two distinct models.
    assert_eq!(data.len(), 2, "expected de-duped list of 2: {ids:?}");
    // Each entry is a full OpenAI model object incl. `created`.
    for m in data {
        assert_eq!(m["object"], "model");
        assert_eq!(m["owned_by"], "llm-gateway");
        assert!(
            m["created"].as_u64().is_some(),
            "created must be a unix-seconds integer: {m}"
        );
    }
}

#[tokio::test]
async fn v1_models_retrieve_returns_model_object_for_id_with_slash() {
    let state = common::state_with_chat_and_config_transcription(
        "Qwen/Qwen3.6-35B-A3B-FP8",
        "mistralai/Voxtral-Mini-4B-Realtime-2602",
    )
    .await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    // The id contains `/` — exercises the `{*id}` catch-all route.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/models/mistralai/Voxtral-Mini-4B-Realtime-2602")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["id"], "mistralai/Voxtral-Mini-4B-Realtime-2602");
    assert_eq!(parsed["object"], "model");
    assert!(parsed["created"].as_u64().is_some());
}

#[tokio::test]
async fn v1_models_retrieve_unknown_id_is_404() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/models/does-not-exist")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["error"]["code"], "model_not_found");
    assert_eq!(parsed["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn v1_models_retrieve_without_bearer_is_401() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/v1/models/model-a"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn v1_chat_completions_drops_client_authorization_and_injects_upstream_key() {
    // Mount that asserts the upstream Authorization header is exactly
    // what we configured on the BackendConfig, NOT the gateway-token
    // bearer the client sent.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer SK-UPSTREAM",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices":[]})))
        .mount(&upstream)
        .await;

    let upstream_uri = upstream.uri();
    let state = state_with_backend_api_key(&upstream_uri, "SK-UPSTREAM").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let body = json!({"model": "model-a", "messages": []}).to_string();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "wiremock would 404 if the assertion failed"
    );
}

/// Variant of `state_with_chat_pool` that configures an `api_key_env`
/// pointing at a test-scoped env var. The integration test sets/clears
/// the env around the lookup so we don't leak state between tests.
async fn state_with_backend_api_key(
    upstream_url: &str,
    key: &str,
) -> gateway::rama_server::RamaState {
    use std::collections::HashMap;
    use std::sync::Arc;

    use gateway::rama_server::{RamaState, SessionStore};
    use gateway::server::rbac::Resolver;
    use gateway::server::tools::ToolRegistry;
    use gateway::server::upstreams::{
        self,
        config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
    };
    use gateway::server::{AppState, Config, db};

    const ENV_KEY: &str = "TEST_UPSTREAM_KEY";
    // SAFETY: integration tests run in the same process — this set
    // races with itself in parallel runs but the value we set is the
    // same across all callers so the race is benign.
    unsafe { std::env::set_var(ENV_KEY, key) };

    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mut pools = HashMap::new();
    pools.insert(
        "pool".to_string(),
        UpstreamPoolConfig {
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                name: "mock".into(),
                base_url: upstream_url.into(),
                api_key_env: Some(ENV_KEY.into()),
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    common::seed_pool_models(&registry, "pool", 0, &["model-a"]);
    let tools = Arc::new(ToolRegistry::new());
    let rbac = Arc::new(Resolver::empty());
    let app = AppState::new(Config::default(), pool.clone(), registry, tools, rbac);
    let sessions = SessionStore::new(pool, common::TEST_SECRET);
    RamaState::new(app, sessions)
}
