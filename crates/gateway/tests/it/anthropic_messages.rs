// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `POST /v1/messages` — the Anthropic Messages compatibility layer that
//! Claude Code talks to.
//!
//! The unit tests in `gateway_core::server::anthropic` pin the format
//! mapping in isolation; these drive the whole route against a wiremock
//! upstream, so they cover the parts only the wiring can get wrong: which
//! credential headers authenticate, what the backend actually receives, how
//! a tool turn round-trips, what the SSE stream looks like on the wire, and
//! whether routing/limit/error decisions match the OpenAI endpoint's.

use crate::common;

use common::Service as _;
use rama::http::{Body, Method, Request, StatusCode};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A `POST /v1/messages` request authenticated with `Authorization: Bearer`.
fn messages_req(bearer: &str, body: &Value) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A `POST /v1/messages/count_tokens` request, authenticated the same way.
fn count_req(bearer: &str, body: &Value) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri("/v1/messages/count_tokens")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A minimal Anthropic request body.
fn ask(model: &str, text: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": text}],
    })
}

/// One OpenAI completion the wiremock upstream answers with.
fn completion(content: &str) -> Value {
    json!({
        "id": "chatcmpl-x",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": content},
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 5, "total_tokens": 16},
    })
}

/// Mount a `POST /chat/completions` mock returning `body`.
async fn mount_chat(upstream: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(upstream)
        .await;
}

/// The single request body the upstream received, as JSON.
async fn upstream_body(upstream: &MockServer) -> Value {
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "expected exactly one upstream call");
    serde_json::from_slice(&received[0].body).expect("upstream body is JSON")
}

async fn json_body(resp: rama::http::Response) -> Value {
    let bytes = common::read_body(resp).await;
    serde_json::from_slice(&bytes).expect("response body is JSON")
}

// ---------------------------------------------------------------- auth

#[tokio::test]
async fn messages_without_a_credential_is_401_in_the_anthropic_shape() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ask("model-a", "hi").to_string()))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = json_body(resp).await;
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");
}

/// `ANTHROPIC_API_KEY` puts the gateway token in `x-api-key`, not in
/// `Authorization`. Both must work, or which variable a developer was told to
/// set decides whether they can connect.
#[tokio::test]
async fn an_x_api_key_header_authenticates() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream, completion("hello")).await;
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("x-api-key", &bearer)
        .header("content-type", "application/json")
        .body(Body::from(ask("model-a", "hi").to_string()))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json_body(resp).await["content"][0]["text"], "hello");
}

/// The caller's credential is ours, not the backend's — it must never be
/// forwarded upstream in either spelling.
#[tokio::test]
async fn the_callers_credential_is_not_forwarded_upstream() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream, completion("ok")).await;
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("x-api-key", &bearer)
        .header("content-type", "application/json")
        .body(Body::from(ask("model-a", "hi").to_string()))
        .unwrap();
    assert_eq!(app.serve(req).await.unwrap().status(), StatusCode::OK);

    let received = upstream.received_requests().await.unwrap();
    let headers = &received[0].headers;
    assert!(
        headers.get("x-api-key").is_none(),
        "the gateway token leaked to the backend in x-api-key"
    );
    assert!(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|v| !v.contains(bearer.as_str()))
            .unwrap_or(true),
        "the gateway token leaked to the backend in authorization"
    );
}

// ------------------------------------------------------- buffered turns

#[tokio::test]
async fn a_buffered_turn_is_translated_in_both_directions() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream, completion("hello from the backend")).await;
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let body = json!({
        "model": "model-a",
        "max_tokens": 512,
        "system": [{"type": "text", "text": "You are Claude Code."}],
        "messages": [{"role": "user", "content": "hi"}],
    });
    let resp = app.serve(messages_req(&bearer, &body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let out = json_body(resp).await;

    // Anthropic-shaped answer…
    assert_eq!(out["type"], "message");
    assert_eq!(out["role"], "assistant");
    assert_eq!(out["model"], "model-a");
    assert_eq!(out["content"][0]["type"], "text");
    assert_eq!(out["content"][0]["text"], "hello from the backend");
    assert_eq!(out["stop_reason"], "end_turn");
    assert_eq!(out["usage"]["input_tokens"], 11);
    assert_eq!(out["usage"]["output_tokens"], 5);

    // …from an OpenAI-shaped request.
    let sent = upstream_body(&upstream).await;
    assert_eq!(sent["model"], "model-a");
    assert_eq!(sent["max_tokens"], 512);
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["messages"][0]["content"], "You are Claude Code.");
    assert_eq!(sent["messages"][1]["role"], "user");
    assert_eq!(sent["messages"][1]["content"], "hi");
}

/// Everything Anthropic-only is the gateway's job to absorb: forwarding any
/// of it to an OpenAI-compatible backend is a hard `400`.
#[tokio::test]
async fn anthropic_only_request_fields_never_reach_the_backend() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream, completion("ok")).await;
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let body = json!({
        "model": "model-a",
        "max_tokens": 64,
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "high"},
        "context_management": {"edits": [{"type": "clear_tool_uses_20250919"}]},
        "system": [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}},
        ]}],
    });
    assert_eq!(
        app.serve(messages_req(&bearer, &body))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    let sent = upstream_body(&upstream).await.to_string();
    for gone in ["cache_control", "context_management", "output_config"] {
        assert!(!sent.contains(gone), "`{gone}` reached the backend: {sent}");
    }
    // `thinking` may only appear as a backend-specific reasoning parameter,
    // never as the Anthropic request field.
    assert!(
        !sent.contains(r#""thinking":{"type""#),
        "the Anthropic thinking field reached the backend: {sent}"
    );
}

/// The shape Claude Code actually sends: a user turn followed by a
/// `role: "system"` entry. Passed through in place it is
/// `400 System message must be at the beginning.` from the backend's chat
/// template — on every request, so nothing works at all.
#[tokio::test]
async fn a_mid_conversation_system_message_never_reaches_the_backend_in_place() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream, completion("ok")).await;
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let body = json!({
        "model": "model-a",
        "max_tokens": 64,
        "system": [{"type": "text", "text": "You are Claude Code."}],
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "system", "content": [{"type": "text", "text": "Available agent types: …"}]},
        ],
    });
    assert_eq!(
        app.serve(messages_req(&bearer, &body))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    let sent = upstream_body(&upstream).await;
    let roles: Vec<&str> = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        vec!["system", "user"],
        "roles sent upstream: {roles:?}"
    );
    // Both instructions survive, joined, in the one leading system message.
    let system = sent["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("You are Claude Code."), "{system}");
    assert!(system.contains("Available agent types"), "{system}");
}

#[tokio::test]
async fn a_tool_use_conversation_round_trips() {
    let upstream = MockServer::start().await;
    mount_chat(
        &upstream,
        json!({
            "id": "chatcmpl-t",
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": "checking",
                    "tool_calls": [{
                        "id": "call_0",
                        "type": "function",
                        "function": {"name": "Read", "arguments": "{\"path\":\"b.rs\"}"},
                    }],
                },
            }],
        }),
    )
    .await;
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    // A second turn: the client already ran one tool and is sending the result.
    let body = json!({
        "model": "model-a",
        "max_tokens": 512,
        "tools": [{
            "name": "Read",
            "description": "read a file",
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
        }],
        "messages": [
            {"role": "user", "content": "read a.rs then b.rs"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_a", "name": "Read", "input": {"path": "a.rs"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_a", "content": "fn a() {}"},
            ]},
        ],
    });
    let resp = app.serve(messages_req(&bearer, &body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let out = json_body(resp).await;

    // The client's tool call comes back as a tool_use block it can execute.
    assert_eq!(out["stop_reason"], "tool_use");
    assert_eq!(out["content"][0]["text"], "checking");
    assert_eq!(out["content"][1]["type"], "tool_use");
    assert_eq!(out["content"][1]["id"], "call_0");
    assert_eq!(out["content"][1]["name"], "Read");
    assert_eq!(out["content"][1]["input"]["path"], "b.rs");

    // The history reached the backend in OpenAI shape, tool result included.
    let sent = upstream_body(&upstream).await;
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "Read");
    assert_eq!(sent["messages"][1]["tool_calls"][0]["id"], "toolu_a");
    assert_eq!(sent["messages"][2]["role"], "tool");
    assert_eq!(sent["messages"][2]["tool_call_id"], "toolu_a");
    assert_eq!(sent["messages"][2]["content"], "fn a() {}");
}

// ------------------------------------------------------------ streaming

/// Split an SSE body into `(event name, payload)` pairs.
fn sse_events(body: &str) -> Vec<(String, Value)> {
    body.split("\n\n")
        .filter(|f| !f.trim().is_empty())
        .map(|frame| {
            let mut name = String::new();
            let mut data = Value::Null;
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    name = rest.to_string();
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    data = serde_json::from_str(rest).unwrap_or(Value::Null);
                }
            }
            (name, data)
        })
        .collect()
}

async fn stream_events<S>(app: &S, bearer: &str, body: &Value) -> Vec<(String, Value)>
where
    S: common::Service<Request, Output = rama::http::Response, Error = std::convert::Infallible>,
{
    let resp = app.serve(messages_req(bearer, body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let bytes = common::read_body(resp).await;
    sse_events(&String::from_utf8_lossy(&bytes))
}

#[tokio::test]
async fn a_streamed_turn_emits_the_anthropic_event_sequence() {
    let upstream = MockServer::start().await;
    let sse = "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n\
               data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
               data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
               data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n\
               data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&upstream)
        .await;

    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let mut body = ask("model-a", "hi");
    body["stream"] = json!(true);
    let events = stream_events(&app, &bearer, &body).await;

    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ],
        "unexpected event sequence: {names:?}"
    );
    assert_eq!(events[0].1["message"]["type"], "message");
    assert_eq!(events[0].1["message"]["model"], "model-a");
    assert!(
        events[0].1["message"]["id"]
            .as_str()
            .unwrap()
            .starts_with("msg_"),
        "message id must be Anthropic-shaped"
    );
    assert_eq!(events[2].1["delta"]["text"], "Hel");
    assert_eq!(events[3].1["delta"]["text"], "lo");
    assert_eq!(events[5].1["delta"]["stop_reason"], "end_turn");
    // The usage frame the client never asked for still becomes its usage
    // report — the tool loop hides that frame, so this proves the sink sees
    // it anyway.
    assert_eq!(events[5].1["usage"]["input_tokens"], 7);
    assert_eq!(events[5].1["usage"]["output_tokens"], 2);
    // No `[DONE]` sentinel: that is the OpenAI terminator, not this one.
    assert!(events.iter().all(|(_, d)| !d.is_null()));
}

/// A tool the gateway doesn't own must come back to the client as a
/// `tool_use` block it can execute — this is the whole Claude Code loop.
#[tokio::test]
async fn a_streamed_client_tool_call_is_handed_back_as_a_tool_use_block() {
    let upstream = MockServer::start().await;
    let sse = "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"looking\"}}]}\n\n\
               data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"type\":\"function\",\"function\":{\"name\":\"Bash\",\"arguments\":\"{\\\"cmd\\\":\"}}]}}]}\n\n\
               data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]}}]}\n\n\
               data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
               data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&upstream)
        .await;

    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let body = json!({
        "model": "model-a",
        "max_tokens": 256,
        "stream": true,
        "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "list files"}],
    });
    let events = stream_events(&app, &bearer, &body).await;
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start", // text
            "content_block_delta",
            "content_block_stop",
            "content_block_start", // tool_use
            "content_block_delta", // input_json_delta
            "content_block_stop",
            "message_delta",
            "message_stop",
        ],
        "unexpected event sequence: {names:?}"
    );
    let start = &events[4].1;
    assert_eq!(start["index"], 1);
    assert_eq!(start["content_block"]["type"], "tool_use");
    assert_eq!(start["content_block"]["id"], "call_9");
    assert_eq!(start["content_block"]["name"], "Bash");
    // The arguments streamed in two fragments upstream; the client gets one
    // complete, parseable object.
    let partial = events[5].1["delta"]["partial_json"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(partial).expect("partial_json is complete JSON");
    assert_eq!(parsed["cmd"], "ls");
    assert_eq!(events[7].1["delta"]["stop_reason"], "tool_use");
}

/// Mid-stream the headers have already shipped, so an upstream failure can only
/// reach the client as an `error` event — and its *type* is all a client has to
/// decide whether retrying is worth it. Typing every failure `api_error` makes
/// a busy backend look like a hopeless request.
#[tokio::test]
async fn a_mid_stream_upstream_failure_keeps_its_error_type() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"message": "too many concurrent requests"}
        })))
        .mount(&upstream)
        .await;

    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let mut body = ask("model-a", "hi");
    body["stream"] = json!(true);
    let events = stream_events(&app, &bearer, &body).await;

    let (name, payload) = events.last().expect("an error event");
    assert_eq!(name, "error");
    assert_eq!(
        payload["error"]["type"], "rate_limit_error",
        "a 429 must not arrive as a generic api_error: {payload}"
    );
    assert!(
        payload["error"]["message"]
            .as_str()
            .unwrap()
            .contains("too many concurrent requests"),
        "the upstream's wording must survive: {payload}"
    );
}

// -------------------------------------------------------------- errors

/// The client's own recovery path matches on the upstream's wording, so it
/// has to survive the trip through our envelope.
#[tokio::test]
async fn an_upstream_error_keeps_its_wording() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "thinking is not supported for this model", "type": "BadRequest"}
        })))
        .mount(&upstream)
        .await;

    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let resp = app
        .serve(messages_req(&bearer, &ask("model-a", "hi")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp).await;
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(
        body["error"]["message"],
        "thinking is not supported for this model"
    );
}

#[tokio::test]
async fn an_unknown_model_is_a_404_error_envelope() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let resp = app
        .serve(messages_req(&bearer, &ask("no-such-model", "hi")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = json_body(resp).await;
    assert_eq!(body["error"]["type"], "not_found_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no-such-model")
    );
}

#[tokio::test]
async fn a_malformed_request_is_a_400_error_envelope() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    // No `model` field at all.
    let resp = app
        .serve(messages_req(&bearer, &json!({"messages": []})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"].as_str().unwrap().contains("model"));
}

/// The same enforcer as `/v1/chat/completions`, reported in this format.
#[tokio::test]
async fn a_quota_breach_is_429_with_retry_after() {
    use gateway_core::server::db::limits::{self, Dimension, SubjectType, Window};

    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    limits::upsert(
        &state.db,
        SubjectType::Global,
        "",
        None,
        Dimension::Requests,
        Window::Hour,
        0.0,
    )
    .await
    .unwrap();
    let app = common::app(state);

    let resp = app
        .serve(messages_req(&bearer, &ask("model-a", "hi")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(resp.headers().get("retry-after").is_some());
    let body = json_body(resp).await;
    assert_eq!(body["error"]["type"], "rate_limit_error");
}

// ------------------------------------------------------- routing + probe

/// Claude Code sends model ids no self-hosted backend serves. An operator
/// maps them with a backend alias — configuration, not code.
#[tokio::test]
async fn an_alias_routes_a_claude_model_name_to_the_backend_model() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream, completion("aliased")).await;

    let state = common::state_with_alias(&upstream.uri(), "claude-sonnet-4-6", "model-a").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let resp = app
        .serve(messages_req(&bearer, &ask("claude-sonnet-4-6", "hi")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // The response names the model the client asked for…
    let resolved = resp
        .headers()
        .get("x-gateway-resolved-model")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let out = json_body(resp).await;
    assert_eq!(out["model"], "claude-sonnet-4-6");
    // …while the header and the backend see the real one.
    assert_eq!(resolved.as_deref(), Some("model-a"));
    assert_eq!(upstream_body(&upstream).await["model"], "model-a");
}

/// Model discovery: the client asks `GET /v1/models`, keeps every entry whose
/// id contains "claude" or "anthropic", and offers them in its picker. The
/// alias an operator adds for routing is therefore also what makes the model
/// discoverable — one piece of configuration, both jobs.
#[tokio::test]
async fn an_alias_is_listed_for_model_discovery() {
    let state =
        common::state_with_alias("http://unused.invalid", "claude-sonnet-4-6", "model-a").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/models?limit=1000")
        .header("x-api-key", &bearer)
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ids: Vec<String> = json_body(resp).await["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        ids.contains(&"claude-sonnet-4-6".to_string()),
        "the alias must be discoverable, got {ids:?}"
    );
    assert!(ids.contains(&"model-a".to_string()));
}

// -------------------------------------------------------- token counting

/// vLLM serves `/tokenize` at the server root, which runs the model's own
/// chat template over messages *and* tool definitions — so the count is the
/// real one, not an estimate.
#[tokio::test]
async fn count_tokens_asks_the_backend_tokenizer() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"count": 296, "max_model_len": 262144})),
        )
        .mount(&upstream)
        .await;

    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let body = json!({
        "model": "model-a",
        "max_tokens": 1024,
        "system": [{"type": "text", "text": "You are Claude Code."}],
        "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "hi"}],
    });
    let req = count_req(&bearer, &body);
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json_body(resp).await["input_tokens"], 296);

    // The tokenizer saw the translated request: system hoisted, tools in
    // OpenAI shape, and none of the sampling knobs it has no use for.
    let sent = upstream_body(&upstream).await;
    assert_eq!(sent["model"], "model-a");
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["tools"][0]["function"]["name"], "Read");
    for gone in ["stream", "max_tokens", "temperature", "top_p"] {
        assert!(sent.get(gone).is_none(), "`{gone}` sent to /tokenize");
    }
}

/// A backend with no tokenizer gets a `404`, not a guess — the client then
/// counts context from real `usage` figures, which is also exact.
#[tokio::test]
async fn count_tokens_is_404_when_the_backend_has_no_tokenizer() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&upstream)
        .await;

    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    let req = count_req(&bearer, &ask("model-a", "hi"));
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = json_body(resp).await;
    assert_eq!(body["error"]["type"], "not_found_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("tokenizer"),
        "the message should say why: {body}"
    );
}

/// A busy backend is not a backend without a tokenizer. Caching a `503` would
/// disable token counting for the rest of the process over a momentary hiccup,
/// so the second call must go back out to the wire.
#[tokio::test]
async fn a_transient_tokenizer_failure_is_not_cached() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"count": 42})))
        .mount(&upstream)
        .await;

    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);
    let body = ask("model-a", "hi");

    let first = app.serve(count_req(&bearer, &body)).await.unwrap();
    assert_eq!(first.status(), StatusCode::NOT_FOUND);
    let second = app.serve(count_req(&bearer, &body)).await.unwrap();
    assert_eq!(
        second.status(),
        StatusCode::OK,
        "a 503 must not be remembered as a missing endpoint"
    );
    assert_eq!(json_body(second).await["input_tokens"], 42);
}

#[tokio::test]
async fn count_tokens_without_a_credential_is_401() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages/count_tokens")
        .header("content-type", "application/json")
        .body(Body::from(ask("model-a", "hi").to_string()))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_body(resp).await["error"]["type"],
        "authentication_error"
    );
}

#[tokio::test]
async fn count_tokens_for_an_unknown_model_is_a_404_route_error() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);
    let req = count_req(&bearer, &ask("no-such-model", "hi"));
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        json_body(resp).await["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no-such-model")
    );
}

#[tokio::test]
async fn the_startup_probe_is_answered() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::HEAD, "/api/hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
