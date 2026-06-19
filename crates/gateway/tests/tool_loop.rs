// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Integration coverage for the tool-call loop on the rama proxy.
//!
//! Drives a two-round chat completion: the wiremock upstream returns a
//! `tool_calls` array on round 1 (calling `company_echo`), then a
//! normal assistant reply on round 2. The gateway should run the tool
//! between rounds, append the result to the messages, and relay the
//! final response with `x-gateway-tool-rounds: 1`.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::{RamaState, SessionStore, router::router};
use gateway::server::config::Config;
use gateway::server::db::{tokens, users};
use gateway::server::rbac::Resolver;
use gateway::server::rbac::config::{RbacConfig, RoleConfig, RoleMapping};
use gateway::server::tools::{ToolRegistry, echo::Echo, time::CurrentTimestamp};
use gateway::server::upstreams::{
    self,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use gateway::server::{AppState, db};
use jiff::{SignedDuration, Timestamp};
use rama::http::{Body, Method, Request, StatusCode};
use serde_json::json;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a state where role "engineer" grants `company_echo`, OIDC
/// role "engineering" maps to "engineer", and the chat pool points at
/// `upstream_uri`.
async fn state_with_tools(upstream_uri: &str) -> RamaState {
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mut pools = HashMap::new();
    pools.insert(
        "pool".to_string(),
        UpstreamPoolConfig {
            compliance: Default::default(),
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                name: "mock".into(),
                base_url: upstream_uri.into(),
                api_key_env: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    common::seed_pool_models(&registry, "pool", 0, &["model-a"]);

    // Real tool registry (Echo + CurrentTimestamp) so company_echo is
    // dispatchable.
    let tools = Arc::new(ToolRegistry::new().with(Echo).with(CurrentTimestamp));

    let rbac = Resolver::build(
        RbacConfig {
            default_role: None,
            mappings: vec![RoleMapping {
                oidc_claim: "groups".into(),
                oidc_value: "engineering".into(),
                role: "engineer".into(),
            }],
        },
        vec![RoleConfig {
            id: "engineer".into(),
            models: vec!["*".into()],
            tools: vec!["company_echo".into()],
            skills: vec![],
        }],
    )
    .unwrap();

    let app = AppState::new(
        Config::default(),
        pool.clone(),
        registry,
        tools,
        Arc::new(rbac),
    );
    let sessions = SessionStore::new(pool, common::TEST_SECRET);
    RamaState::new(app, sessions)
}

/// Seed a user with the OIDC role + a bearer token.
async fn seed_engineer_with_bearer(state: &RamaState) -> String {
    use gateway::server::auth::token;
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: "alice".into(),
            email: "alice@example.com".into(),
            name: None,
            roles: vec!["engineering".into()],
            created_at: now,
            updated_at: now,
            timezone: None,
        },
    )
    .await
    .unwrap();
    let (plaintext, hash) = token::mint();
    tokens::insert(
        &state.db,
        &tokens::Token {
            id: Uuid::new_v4().to_string(),
            user_id: "alice".into(),
            name: "test".into(),
            hash,
            created_at: now,
            last_used_at: None,
            expires_at: now + SignedDuration::from_hours(1),
            revoked_at: None,
            tools_enabled: true,
        },
    )
    .await
    .unwrap();
    plaintext
}

#[tokio::test]
async fn tool_call_loop_runs_one_round_and_relays_final_response() {
    let upstream = MockServer::start().await;

    // The chat-completions mock keeps a counter of how many times it's
    // been called and returns different bodies per call:
    //   round 1 → assistant with a tool_call to company_echo
    //   round 2 → normal assistant reply
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ToolLoopResponder::default())
        .mount(&upstream)
        .await;

    let state = state_with_tools(&upstream.uri()).await;
    let bearer = seed_engineer_with_bearer(&state).await;
    let app = router(Arc::new(state));

    let body = json!({
        "model": "model-a",
        "messages": [{"role": "user", "content": "say hello"}]
    })
    .to_string();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The response should carry the round-2 body (no tool_calls) and the
    // gateway header reporting 1 completed round.
    let rounds = resp
        .headers()
        .get("x-gateway-tool-rounds")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(rounds, "1", "expected one tool-loop round, got `{rounds}`");

    let body = common::read_body(resp).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let content = parsed["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(
        content.contains("hello echoed"),
        "expected round-2 reply to mention the echo result, got `{content}`"
    );
}

#[tokio::test]
async fn no_grants_and_no_client_tools_skips_the_loop() {
    // Same wiremock but the user has no tool grants AND the request
    // body doesn't carry a client-supplied tools array — the gateway
    // should fast-path stream through with no rounds (no
    // x-gateway-tool-rounds header).
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "plain reply"}}]
        })))
        .mount(&upstream)
        .await;

    // State without an RBAC mapping for the user's OIDC role → empty
    // allowed-tools.
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = router(Arc::new(state));

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
    assert!(
        resp.headers().get("x-gateway-tool-rounds").is_none(),
        "fast path should not emit x-gateway-tool-rounds"
    );
}

#[tokio::test]
async fn client_supplied_tools_still_get_gateway_tools_merged_in() {
    // The regression guard for the proxy merge: a client that brings its
    // OWN `tools` array must still have the gateway's tools unioned in and
    // the tool-loop run server-side. Previously the proxy bailed to a
    // byte-dumb passthrough whenever the client sent tools, so gateway
    // tools (e.g. search_web) were unreachable via the API.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ToolLoopResponder::default())
        .mount(&upstream)
        .await;

    let state = state_with_tools(&upstream.uri()).await;
    let bearer = seed_engineer_with_bearer(&state).await;
    let app = router(Arc::new(state));

    // Client drives its own tool ("client_tool") AND we expect the gateway
    // to add "company_echo" alongside it.
    let body = json!({
        "model": "model-a",
        "messages": [{"role": "user", "content": "echo hello"}],
        "tools": [{
            "type": "function",
            "function": {"name": "client_tool", "description": "client's own", "parameters": {"type": "object"}}
        }]
    })
    .to_string();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The loop ran (gateway tool executed) despite the client bringing
    // tools — the old byte-dumb path would emit no rounds header at all.
    let rounds = resp
        .headers()
        .get("x-gateway-tool-rounds")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        rounds, "1",
        "expected the gateway tool-loop to run, got `{rounds}`"
    );

    // Prove the union on the wire: the FIRST body the upstream received
    // carries both the client's tool and the injected gateway tool.
    let requests = upstream.received_requests().await.unwrap();
    let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let tool_names: Vec<&str> = first["tools"]
        .as_array()
        .expect("request carries a tools array")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(
        tool_names.contains(&"client_tool"),
        "client's own tool must survive the merge, got {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"company_echo"),
        "gateway tool must be injected alongside the client's, got {tool_names:?}"
    );
}

/// Assemble an OpenAI-style SSE body from a list of chunk JSON values,
/// terminated with `[DONE]`. Keeps the streaming tests readable instead
/// of hand-escaping `data:` frames.
fn sse_body(chunks: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&format!("data: {chunk}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

#[tokio::test]
async fn streaming_gateway_tool_is_hidden_executed_and_final_streamed() {
    // stream:true + a gateway-owned tool_call: the gateway must suppress
    // the tool_call deltas (the client has no implementation and must not
    // see them), run the tool, loop, and stream the final round's text
    // through. The client sees the answer, never the company_echo call.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(StreamingToolResponder::default())
        .mount(&upstream)
        .await;

    let state = state_with_tools(&upstream.uri()).await;
    let bearer = seed_engineer_with_bearer(&state).await;
    let app = router(Arc::new(state));

    let body = json!({
        "model": "model-a",
        "stream": true,
        "messages": [{"role": "user", "content": "echo hello"}]
    })
    .to_string();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(rama::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();

    // Final answer streamed through.
    assert!(
        body.contains("hello echoed"),
        "expected the final streamed text, body was:\n{body}"
    );
    // The gateway-owned tool_call was suppressed — never leaked to client.
    assert!(
        !body.contains("company_echo"),
        "gateway tool_call must not reach the client, body was:\n{body}"
    );
    assert!(
        !body.contains("\"tool_calls\""),
        "no tool_calls deltas should survive to the client, body was:\n{body}"
    );
    assert!(
        body.contains("[DONE]"),
        "stream must terminate, body was:\n{body}"
    );
}

#[tokio::test]
async fn streaming_client_tool_is_reemitted_to_client() {
    // stream:true + a CLIENT-owned tool_call (the client brought its own
    // tools, unioned with ours): the gateway suppresses the live deltas
    // while accumulating, then — because the turn calls a tool it doesn't
    // own — re-materialises the full tool_call as a synthesized assistant
    // delta + finish chunk so the client can run it and re-submit.
    let upstream = MockServer::start().await;
    let sse = sse_body(&[
        json!({
            "id": "cc-1", "object": "chat.completion.chunk", "created": 1, "model": "model-a",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": null, "tool_calls": [
                {"index": 0, "id": "call_x", "type": "function", "function": {"name": "client_tool", "arguments": ""}}
            ]}, "finish_reason": null}]
        }),
        json!({
            "id": "cc-1", "object": "chat.completion.chunk", "created": 1, "model": "model-a",
            "choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "{\"q\":1}"}}
            ]}, "finish_reason": null}]
        }),
        json!({
            "id": "cc-1", "object": "chat.completion.chunk", "created": 1, "model": "model-a",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
        }),
    ]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&upstream)
        .await;

    let state = state_with_tools(&upstream.uri()).await;
    let bearer = seed_engineer_with_bearer(&state).await;
    let app = router(Arc::new(state));

    let body = json!({
        "model": "model-a",
        "stream": true,
        "messages": [{"role": "user", "content": "call my tool"}],
        "tools": [{
            "type": "function",
            "function": {"name": "client_tool", "description": "client's own", "parameters": {"type": "object"}}
        }]
    })
    .to_string();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();

    // Find the synthesized assistant tool_calls delta and verify it
    // carries the client's complete call (name + accumulated arguments).
    let synthesized = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|p| *p != "[DONE]")
        .filter_map(|p| serde_json::from_str::<serde_json::Value>(p).ok())
        .find(|v| v.pointer("/choices/0/delta/tool_calls").is_some())
        .expect("a tool_calls delta must reach the client");
    let tc = &synthesized["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc["id"], "call_x");
    assert_eq!(tc["function"]["name"], "client_tool");
    assert_eq!(tc["function"]["arguments"], "{\"q\":1}");

    // Followed by a finish_reason terminator and [DONE].
    assert!(
        body.contains("\"finish_reason\":\"tool_calls\""),
        "expected a tool_calls finish terminator, body was:\n{body}"
    );
    assert!(
        body.contains("[DONE]"),
        "stream must terminate, body was:\n{body}"
    );
}

/// Stateful wiremock responder that returns different bodies on each
/// call. wiremock's stock `ResponseTemplate` only supports static
/// replies; rolling our own gives us the per-round shape control.
#[derive(Default)]
struct ToolLoopResponder {
    counter: std::sync::atomic::AtomicU32,
}

/// Streaming sibling of `ToolLoopResponder`: round 0 streams a
/// gateway-owned tool_call as SSE; round 1 streams a final text reply.
/// Exercises `drive_streaming_tool_loop` end to end.
#[derive(Default)]
struct StreamingToolResponder {
    counter: std::sync::atomic::AtomicU32,
}

impl wiremock::Respond for StreamingToolResponder {
    fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
        let round = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let sse = match round {
            0 => sse_body(&[
                json!({
                    "id": "s-1", "object": "chat.completion.chunk", "created": 1, "model": "model-a",
                    "choices": [{"index": 0, "delta": {"role": "assistant", "content": null, "tool_calls": [
                        {"index": 0, "id": "call_1", "type": "function", "function": {"name": "company_echo", "arguments": "{\"message\":\"hello\"}"}}
                    ]}, "finish_reason": null}]
                }),
                json!({
                    "id": "s-1", "object": "chat.completion.chunk", "created": 1, "model": "model-a",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
                }),
            ]),
            _ => sse_body(&[
                json!({
                    "id": "s-2", "object": "chat.completion.chunk", "created": 2, "model": "model-a",
                    "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hello echoed"}, "finish_reason": null}]
                }),
                json!({
                    "id": "s-2", "object": "chat.completion.chunk", "created": 2, "model": "model-a",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
                }),
            ]),
        };
        ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream")
    }
}

impl wiremock::Respond for ToolLoopResponder {
    fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
        let round = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match round {
            0 => {
                // Round 1: assistant calls company_echo("hello"). The
                // gateway runs the tool, appends `role: "tool"` with
                // the echo result, and re-POSTs.
                ResponseTemplate::new(200).set_body_json(json!({
                    "id": "round-1",
                    "choices": [{
                        "index": 0,
                        "finish_reason": "tool_calls",
                        "message": {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "company_echo",
                                    "arguments": "{\"message\":\"hello\"}"
                                }
                            }]
                        }
                    }]
                }))
            }
            _ => {
                // Round 2+: pretend we consumed the tool result and
                // can answer normally. We can also peek at the round-2
                // request to confirm the tool result was appended,
                // but for the spike we just need a final reply that
                // says "hello echoed" so the assertion can fire.
                let body_str = std::str::from_utf8(&req.body).unwrap_or("");
                let saw_tool_msg =
                    body_str.contains("\"role\":\"tool\"") && body_str.contains("hello");
                let content = if saw_tool_msg {
                    "the tool said: hello echoed"
                } else {
                    "round 2 reached without tool message — bug in runner"
                };
                ResponseTemplate::new(200).set_body_json(json!({
                    "id": "round-2",
                    "choices": [{
                        "index": 0,
                        "finish_reason": "stop",
                        "message": {"role": "assistant", "content": content}
                    }]
                }))
            }
        }
    }
}
