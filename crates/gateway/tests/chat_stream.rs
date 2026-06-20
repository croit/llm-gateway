// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/chat/{id}/messages` drives the persisted-conversation worker:
//! creates user + assistant turn rows in SQLite, spawns the upstream-
//! streaming worker, and SSE-tails the worker's broadcast back to the
//! browser. These tests cover the end-to-end happy path (deltas
//! produce DB content and outer-mode patches arrive on the wire) as
//! well as the empty / anonymous / no-[DONE] edge cases.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::{RamaState, SessionStore, router::router};
use gateway::server::AppState;
use gateway::server::config::Config;
use gateway::server::db;
use gateway::server::rbac::Resolver;
use gateway::server::tools::ToolRegistry;
use gateway::server::upstreams::{
    self,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use rama::http::body::util::BodyExt;
use rama::http::{Body, Method, Request, StatusCode};
use session_core::db as chat;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a multipart/form-data body with simple text fields. Mirrors
/// the composer's submit shape (model + message, no attachments).
/// Returns (content_type_value, body_bytes).
fn multipart_text(fields: &[(&str, &str)]) -> (String, Vec<u8>) {
    let boundary = "----testboundaryX";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let ct = format!("multipart/form-data; boundary={boundary}");
    (ct, body)
}

async fn state_with_streaming_chat(upstream_uri: &str) -> RamaState {
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
    let app = AppState::new(
        Config::default(),
        pool.clone(),
        registry,
        Arc::new(ToolRegistry::new()),
        Arc::new(Resolver::empty()),
    );
    let sessions = SessionStore::new(pool, common::TEST_SECRET);
    RamaState::new(
        app,
        sessions,
        gateway::server::usage::UsageHandle::disabled(),
    )
}

/// Helper: spin up a fresh state + session cookie + chat session.
async fn setup(upstream_uri: &str) -> (Arc<RamaState>, String, String) {
    let state = state_with_streaming_chat(upstream_uri).await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session = chat::create_session(&state.db, "alice").await.unwrap();
    (Arc::new(state), cookie, session.id)
}

#[tokio::test]
async fn message_send_emits_initial_bubbles_and_finalizes_signal() {
    let upstream = MockServer::start().await;
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
         data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&upstream)
        .await;

    let (state, cookie, session_id) = setup(&upstream.uri()).await;
    let app = router(state.clone());

    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "hi")]);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/chat/{session_id}/messages"))
        .header("cookie", format!("id={cookie}"))
        .header("content-type", ct)
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
    // Anti-buffering header so proxies stream rather than buffer the
    // response — the regression you'd see without it is "reply lands
    // as one block."
    assert_eq!(
        resp.headers()
            .get("x-accel-buffering")
            .and_then(|v| v.to_str().ok()),
        Some("no")
    );

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&body).unwrap();

    // First element-patch event is the initial `mode append` of the
    // user bubble + the assistant skeleton onto `#conversation`.
    let first_event_end = body.find("\n\n").unwrap();
    let first_event = &body[..first_event_end];
    assert!(first_event.contains("data: selector #conversation"));
    assert!(first_event.contains("data: mode append"));
    assert!(first_event.contains(r#"class="chat-msg--user""#));
    assert!(first_event.contains(r#"class="chat-msg--assistant""#));
    assert!(first_event.contains(">hi<"));

    // The remaining patches are `mode outer` re-renders of the
    // assistant bubble keyed to `#turn-<uuid>`. We don't assert the
    // exact count (worker timing makes it nondeterministic — could be
    // 1 per content delta + 1 finalize, or a single coalesced one
    // depending on broadcast queueing), but the per-uuid selector
    // must show up at least once.
    let assistant_id = chat::list_turns(&state.db, &session_id)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.turn.role == chat::TurnRole::Assistant)
        .expect("assistant turn was created")
        .turn
        .id;
    let outer_selector = format!("data: selector #turn-{assistant_id}");
    assert!(
        body.matches(&outer_selector).count() >= 1,
        "expected at least one outer-mode patch on #turn-{assistant_id}, body was:\n{body}"
    );

    // Final signal-patch flips chatStreaming=false on every attached
    // client.
    let signal_event_count = body.matches("event: datastar-patch-signals").count();
    assert_eq!(
        signal_event_count, 1,
        "expected one datastar-patch-signals event (end-of-stream flag flip):\n{body}"
    );
    assert!(
        body.contains(r#"data: signals {"chatStreaming":false}"#),
        "expected the signal patch to set chatStreaming=false:\n{body}"
    );

    // DB-side: the assistant turn is now status=completed with the
    // full accumulated content.
    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    let asst = turns
        .iter()
        .find(|t| t.turn.role == chat::TurnRole::Assistant)
        .unwrap();
    assert_eq!(asst.turn.status, chat::TurnStatus::Completed);
    assert_eq!(asst.turn.content.as_deref(), Some("Hello"));
}

#[tokio::test]
async fn chat_turn_records_a_usage_row_with_source_chat() {
    use gateway::server::db::usage::{Filter, Period, aggregate, period_bounds};
    use jiff::Timestamp;

    // Content deltas, then the trailing `usage` frame the driver asks for via
    // `stream_options.include_usage`, then [DONE].
    let upstream = MockServer::start().await;
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
         data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8}}\n\n\
         data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&upstream)
        .await;

    // Opt into a live metered sink before wrapping the state in an Arc.
    let state = state_with_streaming_chat(&upstream.uri()).await;
    let metered = gateway::server::usage::spawn(state.db.clone(), 90);
    let state = state.with_usage(metered);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let session = chat::create_session(&state.db, "alice").await.unwrap();
    let db = state.db.clone();
    let state = Arc::new(state);
    let app = router(state.clone());

    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "hi")]);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/chat/{}/messages", session.id))
        .header("cookie", format!("id={cookie}"))
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Draining the SSE tail blocks until the worker finalizes the turn — so
    // by here the per-round usage record has been emitted onto the channel.
    let _ = resp.into_body().collect().await.unwrap().to_bytes();

    // Let the batched writer flush (≤ ~500ms).
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;

    let now = Timestamp::now();
    let bounds = period_bounds(Period::Today, "UTC", now);
    let agg = aggregate(&db, bounds, &Filter::default(), 90, now, true)
        .await
        .unwrap();
    assert_eq!(
        agg.summary.requests, 1,
        "chat turn recorded one backend call"
    );
    assert_eq!(
        agg.summary.total_tokens, 8,
        "usage frame parsed from the stream"
    );
    assert_eq!(agg.by_source[0].key, "chat", "source is the chat UI");
    assert_eq!(agg.by_model[0].key, "model-a");
}

#[tokio::test]
async fn message_send_renders_markdown_even_when_upstream_omits_done() {
    // Upstream emits two deltas (which together form valid markdown)
    // and then closes the stream *without* the OpenAI `[DONE]`
    // terminator. The gateway should still persist the full content
    // and emit a final outer-mode patch — otherwise the user sees
    // partial text and the turn row stays in_progress forever.
    let upstream = MockServer::start().await;
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"# Hi\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{\"content\":\"\\n\\nbody\"}}]}\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&upstream)
        .await;

    let (state, cookie, session_id) = setup(&upstream.uri()).await;
    let app = router(state.clone());

    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "hi")]);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/chat/{session_id}/messages"))
        .header("cookie", format!("id={cookie}"))
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&body).unwrap();

    // The final assistant-bubble outer-patch carries the
    // markdown-rendered HTML.
    assert!(
        body.contains("<h1>Hi</h1>"),
        "expected markdown-rendered <h1> from `# Hi`, body was:\n{body}"
    );
    assert!(
        body.contains("<p>body</p>"),
        "expected markdown-rendered <p>, body was:\n{body}"
    );

    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    let asst = turns
        .iter()
        .find(|t| t.turn.role == chat::TurnRole::Assistant)
        .unwrap();
    assert_eq!(asst.turn.status, chat::TurnStatus::Completed);
    assert_eq!(asst.turn.content.as_deref(), Some("# Hi\n\nbody"));
}

#[tokio::test]
async fn message_send_rejects_anonymous() {
    let state = state_with_streaming_chat("http://unused.invalid").await;
    let app = router(Arc::new(state));
    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "hi")]);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/chat/any-id/messages")
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    // Anonymous on a page route gets the 303 → /login redirect.
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers()
            .get(rama::http::header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/login")
    );
}

#[tokio::test]
async fn message_send_rejects_empty_message() {
    let (state, cookie, session_id) = setup("http://unused.invalid").await;
    let app = router(state.clone());
    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "")]);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/chat/{session_id}/messages"))
        .header("cookie", format!("id={cookie}"))
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    // Empty-submit feedback used to be 400+text but datastar 1.0
    // ignores non-SSE bodies on `@post` responses — the user got no
    // toast and no console message. We now emit a 200 SSE event
    // stream with a single error-toast patch so the red bubble
    // shows up just like every other validation failure.
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(rama::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ct.starts_with("text/event-stream"),
        "expected event-stream, got `{ct}`"
    );
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("event: datastar-patch-elements"),
        "expected a toast patch, got:\n{body}"
    );
    assert!(
        body.contains("border-l-error"),
        "expected the error-toast variant, got:\n{body}"
    );
    assert!(
        body.contains("message can"),
        "expected the empty-message copy in the toast, got:\n{body}"
    );
    // No turns should have been created for an empty submit.
    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    assert!(turns.is_empty());
}
