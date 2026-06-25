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

/// The largest in-progress "Thinking… (X.Ys)" value the stream ever
/// rendered. Returns -1.0 if no in-progress thinking label appeared.
/// Deliberately ignores the finalized "Thought for …" label — the bug
/// was that the *live* timer never moved off 0.0s while reasoning
/// streamed, even though the final stamped value was correct.
fn max_in_progress_thinking_secs(body: &str) -> f64 {
    let marker = "Thinking… (";
    let mut max = -1.0_f64;
    let mut rest = body;
    while let Some(i) = rest.find(marker) {
        rest = &rest[i + marker.len()..];
        if let Some(end) = rest.find("s)")
            && let Ok(v) = rest[..end].parse::<f64>()
        {
            max = max.max(v);
        }
    }
    max
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_timer_advances_while_reasoning_streams() {
    // Regression: while the model streams reasoning (and before any
    // content arrives), the "Thinking… (Xs)" timer must tick up. It
    // used to freeze at 0.0s because `reasoning_elapsed_ms` was only
    // stamped on the first *content* delta / at finalization, so every
    // in-progress re-render rendered the NULL → 0.0s default.
    //
    // wiremock delivers the whole body in one shot (elapsed ≈ 0 for
    // every chunk), so we hand-roll a raw upstream that puts real
    // wall-clock gaps between reasoning chunks.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Serve every connection the client opens (pool warm-ups /
        // retries may make more than one); each gets the same delayed
        // reasoning stream.
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut rd, mut wr) = sock.into_split();
                // Keep draining the request side for the whole
                // connection so the client's body write never hits a
                // half-closed socket ("error sending request").
                tokio::spawn(async move {
                    let mut b = [0u8; 1024];
                    while let Ok(n) = rd.read(&mut b).await {
                        if n == 0 {
                            break;
                        }
                    }
                });
                if wr
                    .write_all(
                        b"HTTP/1.1 200 OK\r\n\
                          Content-Type: text/event-stream\r\n\
                          Transfer-Encoding: chunked\r\n\
                          Connection: close\r\n\r\n",
                    )
                    .await
                    .is_err()
                {
                    return;
                }
                // Reasoning-only stream: no content delta ever lands, so
                // the bubble stays "Thinking…" the whole way and the
                // timer is driven purely by the reasoning path under test.
                let chunks = [
                    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"let me\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\" think\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\" hard\"}}]}\n\n",
                    "data: [DONE]\n\n",
                ];
                for (i, c) in chunks.iter().enumerate() {
                    if i > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    let frame = format!("{:x}\r\n{c}\r\n", c.len());
                    if wr.write_all(frame.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = wr.flush().await;
                }
                let _ = wr.write_all(b"0\r\n\r\n").await;
                let _ = wr.flush().await;
            });
        }
    });

    let base = format!("http://{addr}");
    let (state, cookie, session_id) = setup(&base).await;
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

    // The live timer must have advanced past zero *while reasoning was
    // still streaming* (two 200ms gaps before [DONE] → ≥ 0.2s). Before
    // the fix this was always 0.0s.
    let max_live = max_in_progress_thinking_secs(body);
    assert!(
        max_live >= 0.1,
        "expected an in-progress 'Thinking… (Xs)' label > 0.0s while reasoning streamed, \
         got max {max_live}; body was:\n{body}"
    );

    // And the persisted value is the reasoning duration, not NULL.
    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    let asst = turns
        .iter()
        .find(|t| t.turn.role == chat::TurnRole::Assistant)
        .unwrap();
    assert_eq!(asst.turn.status, chat::TurnStatus::Completed);
    assert!(
        asst.turn.reasoning_elapsed_ms.unwrap_or(0) > 0,
        "reasoning_elapsed_ms should be stamped from the reasoning stream"
    );
}

/// Round 0 streams a `tool_call` to a name the gateway doesn't own — an MCP
/// capability id the model invented instead of going through
/// `invoke_capability` (the croit-ERP failure that left a call stuck
/// "Calling" for 24h). Round 1 streams a normal reply.
#[derive(Default)]
struct UnknownToolResponder {
    counter: std::sync::atomic::AtomicU32,
}

impl wiremock::Respond for UnknownToolResponder {
    fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
        // The auto-title generator also POSTs here (non-streaming); answer it
        // trivially so it doesn't consume one of the turn's streamed rounds.
        let body = std::str::from_utf8(&req.body).unwrap_or("");
        if !body.contains("\"stream\":true") {
            return ResponseTemplate::new(200).set_body_raw(
                r#"{"choices":[{"message":{"role":"assistant","content":"Erp"}}]}"#,
                "application/json",
            );
        }
        let round = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let sse = if round == 0 {
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"mcp__croit_erp__taskBoards.list\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n"
        } else {
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n\
             data: [DONE]\n\n"
        };
        ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream")
    }
}

#[tokio::test]
async fn unknown_tool_call_is_errored_not_left_calling() {
    // Regression for the "Calling forever" bug: when the model emits a
    // tool_call for a tool the gateway doesn't own, the inserted row must be
    // completed as errored (never left 'running', which renders as a
    // permanent spinner) AND answered so the model can recover — here it
    // produces a final reply on round 2.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(UnknownToolResponder::default())
        .mount(&upstream)
        .await;

    let (state, cookie, session_id) = setup(&upstream.uri()).await;
    let app = router(state.clone());

    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "use the erp")]);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/chat/{session_id}/messages"))
        .header("cookie", format!("id={cookie}"))
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Draining the SSE tail blocks until the worker finalizes the turn.
    let _ = resp.into_body().collect().await.unwrap().to_bytes();

    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    let asst = turns
        .iter()
        .find(|t| t.turn.role == chat::TurnRole::Assistant)
        .unwrap();
    let call = asst
        .tool_calls
        .iter()
        .find(|c| c.name == "mcp__croit_erp__taskBoards.list")
        .expect("the unknown tool call was recorded");
    assert_eq!(
        call.status,
        chat::ToolCallStatus::Errored,
        "an unknown tool call must be errored, not left 'running' (the stuck-Calling bug)"
    );
    assert!(
        call.output_json
            .as_deref()
            .unwrap_or_default()
            .contains("invoke_capability"),
        "the error should steer the model toward invoke_capability, got: {:?}",
        call.output_json
    );
    // The call was answered, so the model recovered and the turn completed.
    assert_eq!(asst.turn.status, chat::TurnStatus::Completed);
    assert_eq!(asst.turn.content.as_deref(), Some("done"));
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
