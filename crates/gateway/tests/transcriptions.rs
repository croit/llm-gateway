// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! /v1/audio/transcriptions — bearer-gated multipart routing.
//!
//! The test exercises the multipart `model` extraction + the
//! boundary-preserving forward to the upstream. wiremock acts as the
//! upstream "whisper" backend.

mod common;

use common::Service as _;
use gateway::server::upstreams::config::PoolKind;
use rama::http::{Body, Method, Request, StatusCode};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BOUNDARY: &str = "------TESTBND";

fn multipart(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut out = String::new();
    for (name, value) in parts {
        out.push_str(&format!("--{BOUNDARY}\r\n"));
        out.push_str(&format!(
            "Content-Disposition: form-data; name=\"{name}\"\r\n\r\n"
        ));
        out.push_str(value);
        out.push_str("\r\n");
    }
    out.push_str(&format!("--{BOUNDARY}--\r\n"));
    out.into_bytes()
}

fn transcribe_request(bearer: &str, body: Vec<u8>) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri("/v1/audio/transcriptions")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn anonymous_request_is_401() {
    let state = common::state_with_pool(
        "http://unused.invalid",
        PoolKind::Transcription,
        "whisper-1",
    )
    .await;
    let app = common::app(state);
    let body = multipart(&[("model", "whisper-1")]);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/audio/transcriptions")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn happy_path_relays_text() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "hello world"})))
        .mount(&upstream)
        .await;

    let state =
        common::state_with_pool(&upstream.uri(), PoolKind::Transcription, "whisper-1").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let body = multipart(&[("model", "whisper-1"), ("language", "en")]);
    let resp = app.serve(transcribe_request(&bearer, body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["text"], "hello world");
}

#[tokio::test]
async fn missing_model_field_is_400() {
    let state = common::state_with_pool(
        "http://unused.invalid",
        PoolKind::Transcription,
        "whisper-1",
    )
    .await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);
    let body = multipart(&[("language", "en")]);
    let resp = app.serve(transcribe_request(&bearer, body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert!(
        parsed["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("missing required `model`"),
        "expected missing-model error: {parsed}"
    );
}

#[tokio::test]
async fn unknown_model_is_404_model_not_found() {
    // OpenAI parity: same as the chat path — a transcription model no
    // backend serves is a client error (404 `model_not_found`), not a 4xx
    // generic or a transient 5xx.
    let state = common::state_with_pool(
        "http://unused.invalid",
        PoolKind::Transcription,
        "whisper-1",
    )
    .await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);
    let body = multipart(&[("model", "not-whisper")]);
    let resp = app.serve(transcribe_request(&bearer, body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["error"]["code"], "model_not_found");
    assert_eq!(parsed["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn upstream_5xx_is_relayed() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error":"out of capacity"})))
        .mount(&upstream)
        .await;

    let state =
        common::state_with_pool(&upstream.uri(), PoolKind::Transcription, "whisper-1").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);
    let body = multipart(&[("model", "whisper-1")]);
    let resp = app.serve(transcribe_request(&bearer, body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}
