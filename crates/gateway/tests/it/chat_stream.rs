// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/chat/{id}/messages` drives the persisted-conversation worker:
//! creates user + assistant turn rows in SQLite, spawns the upstream-
//! streaming worker, and SSE-tails the worker's broadcast back to the
//! browser. These tests cover the end-to-end happy path (deltas
//! produce DB content and outer-mode patches arrive on the wire) as
//! well as the empty / anonymous / no-[DONE] edge cases.

use crate::common;

use std::collections::HashMap;
use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::{RamaState, SessionStore, router::router};
use gateway_core::server::config::Config;
use gateway_core::server::db;
use gateway_core::server::rbac::Resolver;
use gateway_core::server::upstreams::{
    self,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use gateway_runtime::server::AppState;
use gateway_runtime::server::tools::ToolRegistry;
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
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                alias: None,
                probe_models: true,
                supports_edit: false,
                name: "mock".into(),
                base_url: upstream_uri.into(),
                api_key_env: None,
                api_key: None,
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
        gateway_core::server::usage::UsageHandle::disabled(),
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

    // The stream opens by arming the composer's Stop control. The submit
    // directive also sets it client-side, but the server asserting it is what
    // makes "a turn started" and "Stop is showing" the same event — the client
    // signal drifts (composer re-renders re-seed it, other handlers cleared
    // it) and a turn streaming behind a "ready" composer looks finished.
    let armed = body
        .find(r#"data: signals {"chatStreaming":true}"#)
        .unwrap_or_else(|| panic!("expected the stream to arm chatStreaming:\n{body}"));

    // Then the initial `mode append` of the user bubble + the assistant
    // skeleton onto `#conversation`.
    let first_patch = body.find("data: selector #conversation").unwrap();
    assert!(
        armed < first_patch,
        "the arming signal must lead the first patch:\n{body}"
    );
    let first_event_end = body[first_patch..].find("\n\n").unwrap() + first_patch;
    let event_start = body[..first_patch].rfind("event: ").unwrap();
    let first_event = &body[event_start..first_event_end];
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

    // The final signal-patch flips chatStreaming=false on every attached
    // client. Other signal patches, such as asset-panel state, may be
    // emitted during the stream.
    let finalized_signal_count = body
        .matches(r#"data: signals {"chatStreaming":false}"#)
        .count();
    assert_eq!(
        finalized_signal_count, 1,
        "expected one chatStreaming=false signal patch:\n{body}"
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
    use gateway_core::server::db::usage::{Filter, Period, aggregate, period_bounds};
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
    let metered = gateway_core::server::usage::spawn(state.db.clone(), 90);
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

/// A submit that lands while this user's turn is still streaming is rejected —
/// and the rejection must leave the Stop control **armed**.
///
/// The bug: the rejection went through the blanket `chatStreaming:false` reset
/// every other chat error used, so asking "are you still working?" during a
/// long turn disarmed Stop at the one moment it matters and left the composer
/// claiming the turn had finished. A turn genuinely is in flight here, so the
/// honest signal is the armed one.
#[tokio::test]
async fn a_submit_during_a_live_turn_leaves_stop_armed() {
    use session_core::workers::RegisterOutcome;

    let upstream = MockServer::start().await;
    let (state, cookie, session_id) = setup(&upstream.uri()).await;
    let app = router(state.clone());

    // Stand in for a live worker on this session — the registry is what the
    // handler consults, so no upstream traffic is needed.
    let RegisterOutcome::Registered { worker: _worker } =
        state.chats.register("alice", "busy-turn", &session_id)
    else {
        panic!("registry was not empty");
    };

    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "are you still working?")]);
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
    let body = String::from_utf8_lossy(&body);

    assert!(
        !body.contains(r#""chatStreaming":false"#),
        "rejecting a mid-turn submit must not disarm Stop:\n{body}"
    );
    assert!(
        body.contains(r#"data: signals {"chatStreaming":true}"#),
        "rejecting a mid-turn submit must (re-)arm Stop:\n{body}"
    );
    // The user still gets told why nothing was sent.
    assert!(
        body.contains("event: datastar-patch-elements"),
        "expected an error toast:\n{body}"
    );
    // And no second turn was created.
    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    assert!(
        turns.is_empty(),
        "a rejected submit must persist nothing: {turns:?}"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_timer_is_client_driven_and_finalizes_nonzero() {
    // Regression (structural). The live "Thinking… (Xs)" timer used to be
    // server-driven: `reasoning_elapsed_ms` was rewritten per reasoning
    // chunk and the bubble re-rendered each tick. On a backend that flushes
    // its reasoning in a single burst the elapsed was ≈0 at every write and
    // then frozen on finalize → a permanent 0.0s.
    //
    // The timer now ticks client-side. The server's job is only to (a) emit
    // a `<thinking-timer>` element carrying the `data-elapsed-ms` anchor and
    // a `{secs}`-placeholder `data-label-template` while reasoning is in
    // progress, and (b) stamp one authoritative `reasoning_elapsed_ms` at
    // finalize — measured start→finalize, so it is non-zero even when the
    // whole reasoning arrives in ONE chunk.
    //
    // This upstream reproduces exactly that pathological shape: a single
    // reasoning burst, a real wall-clock gap, then [DONE] with no content.
    // wiremock can't (it delivers the whole body at once), so we hand-roll
    // a raw upstream.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        // Signal that the server is ready to accept connections
        let _ = ready_tx.send(());

        // Serve every connection the client opens (pool warm-ups /
        // retries may make more than one); each gets the same delayed
        // reasoning stream.
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut rd, mut wr) = sock.into_split();
                // Read the ENTIRE request (headers + Content-Length body)
                // before writing a byte of the response. Responding while
                // the client is still uploading its POST body — then having
                // the writer finish and drop the socket — is what surfaced as
                // a flaky "error sending request" (a non-idempotent POST hyper
                // won't retry). Draining the read half concurrently wasn't
                // enough; the server must not "answer early".
                {
                    let mut buf = Vec::with_capacity(2048);
                    let mut b = [0u8; 1024];
                    let mut content_len: usize = 0;
                    let mut header_end: Option<usize> = None;
                    loop {
                        // Headers complete yet?
                        if header_end.is_none()
                            && let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                        {
                            header_end = Some(p + 4);
                            let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                            content_len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(0);
                        }
                        // Once headers are in, stop as soon as the whole body
                        // has arrived.
                        if let Some(end) = header_end
                            && buf.len() >= end + content_len
                        {
                            break;
                        }
                        match rd.read(&mut b).await {
                            Ok(0) | Err(_) => break, // client hung up / socket error
                            Ok(n) => buf.extend_from_slice(&b[..n]),
                        }
                    }
                }
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
                // Single-burst reasoning-only stream: ALL reasoning in one
                // delta (the shape that used to freeze the old server-driven
                // timer at 0.0s), then a real wall-clock gap, then [DONE]
                // with no content ever landing — so the turn finalizes while
                // still "thinking" and the freeze happens at finalize.
                let chunks = [
                    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"let me think hard\"}}]}\n\n",
                    "data: [DONE]\n\n",
                ];
                for (i, c) in chunks.iter().enumerate() {
                    if i > 0 {
                        // Gap between the reasoning burst and [DONE] so the
                        // start→finalize duration is unambiguously non-zero.
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

    // Wait for the server to signal it's ready to accept connections.
    // This prevents a race condition where the test client tries to connect
    // before the server is ready, causing "error sending request" failures.
    let _ = ready_rx.await;

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

    // (a) While reasoning was in progress the server emitted the
    // client-driven timer element — a `<thinking-timer>` carrying the
    // elapsed-so-far anchor and a localized template with the `{secs}`
    // placeholder the browser fills each frame. This is the wiring that
    // makes the live count independent of upstream chunk cadence; before
    // the fix the server rendered a frozen number instead.
    assert!(
        body.contains("<thinking-timer"),
        "expected a <thinking-timer> element while reasoning streamed; body was:\n{body}"
    );
    assert!(
        body.contains("data-elapsed-ms="),
        "thinking-timer must carry the data-elapsed-ms anchor; body was:\n{body}"
    );
    assert!(
        body.contains("data-label-template=") && body.contains("{secs}"),
        "thinking-timer must carry a {{secs}}-placeholder label template; body was:\n{body}"
    );

    // (b) The finalized bubble shows the settled "Thought for Xs" label,
    // and the persisted duration is non-zero — measured start→finalize, so
    // a single-burst reasoning stream (the old freeze case) still records
    // real think-time rather than 0.0s.
    assert!(
        body.contains("Thought for"),
        "expected the finalized 'Thought for Xs' label; body was:\n{body}"
    );
    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    let asst = turns
        .iter()
        .find(|t| t.turn.role == chat::TurnRole::Assistant)
        .unwrap();
    assert_eq!(asst.turn.status, chat::TurnStatus::Completed);
    assert!(
        asst.turn.reasoning_started_at.is_some(),
        "reasoning_started_at should be stamped on the first reasoning chunk"
    );
    assert!(
        asst.turn.reasoning_elapsed_ms.unwrap_or(0) > 0,
        "reasoning_elapsed_ms should be stamped non-zero at finalize even for a single burst"
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

// ---------------------------------------------------------------------------
// Delta-protocol tests. The original design re-rendered the entire
// assistant bubble (envelope + full accumulated content + live
// reasoning trace) into every `datastar-patch-elements` event, per
// upstream chunk — quadratic wire cost, measured at 225 MB for one
// long reply on mobile. These pin the replacement properties: sealed
// blocks travel once, live reasoning never rides the main stream,
// and the on-demand /thinking sub-stream carries it instead.

/// A raw TCP "upstream" the test scripts at runtime: each `send(...)`
/// writes one SSE line chunk to every open connection; `None` writes
/// the terminating chunk. Used where wiremock's write-everything-at-
/// once model can't reproduce mid-stream states. The watch receiver
/// reports how many connections are open so tests can wait for the
/// worker to dial in before pushing the first chunk.
async fn scripted_sse_upstream() -> (
    String,
    tokio::sync::mpsc::Sender<Option<String>>,
    tokio::sync::watch::Receiver<usize>,
    tokio::sync::oneshot::Receiver<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Option<String>>(16);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let (conn_tx, conn_rx) = tokio::sync::watch::channel(0usize);

    tokio::spawn(async move {
        let _ = ready_tx.send(());
        let mut writers: Vec<tokio::net::tcp::OwnedWriteHalf> = Vec::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    if let Ok((sock, _)) = accepted {
                        let (mut rd, mut wr) = sock.into_split();
                        // Drain the request half so client POSTs never
                        // stall on unread bodies.
                        tokio::spawn(async move {
                            let mut sink = [0u8; 4096];
                            loop {
                                match rd.read(&mut sink).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {}
                                }
                            }
                        });
                        let _ = wr.write_all(
                            b"HTTP/1.1 200 OK\r\n\
                              Content-Type: text/event-stream\r\n\
                              Transfer-Encoding: chunked\r\n\
                              Connection: close\r\n\r\n",
                        ).await;
                        let _ = wr.flush().await;
                        writers.push(wr);
                        let _ = conn_tx.send(writers.len());
                    }
                }
                line = rx.recv() => {
                    match line {
                        Some(Some(line)) => {
                            let frame = format!("{:x}\r\n{line}\r\n", line.len());
                            for w in writers.iter_mut() {
                                let _ = w.write_all(frame.as_bytes()).await;
                                let _ = w.flush().await;
                            }
                        }
                        Some(None) | None => {
                            for w in writers.iter_mut() {
                                let _ = w.write_all(b"0\r\n\r\n").await;
                                let _ = w.flush().await;
                            }
                            break;
                        }
                    }
                }
            }
        }
    });
    (format!("http://{addr}"), tx, conn_rx, ready_rx)
}

/// Block until the scripted upstream has at least one connection.
async fn await_upstream_connection(mut conns: tokio::sync::watch::Receiver<usize>) {
    loop {
        if *conns.borrow() >= 1 {
            return;
        }
        if conns.changed().await.is_err() {
            panic!("scripted upstream hung up before any connection");
        }
    }
}

/// Incrementally read an SSE response body, buffering bytes. Returns
/// (buffered_so_far, body) — timeout-bounded so assertions can run
/// against "everything sent within N ms".
async fn drain_sse_for(
    mut body: rama::http::Body,
    window: std::time::Duration,
) -> (String, rama::http::Body, bool) {
    use rama::http::body::util::BodyExt;
    let mut out = String::new();
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return (out, body, false);
        }
        match tokio::time::timeout(remaining, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    out.push_str(&String::from_utf8_lossy(data));
                }
            }
            Ok(None) => return (out, body, true),
            _ => return (out, body, true),
        }
    }
}

/// Poll until the assistant turn's reasoning matches `want_prefix`,
/// returning the turn id. Bounded at 5s so a broken stream fails
/// instead of hanging.
async fn await_reasoning(state: &Arc<RamaState>, session_id: &str, want_prefix: &str) -> String {
    for _ in 0..100 {
        if let Ok(turns) = chat::list_turns(&state.db, session_id).await
            && let Some(t) = turns
                .iter()
                .find(|t| t.turn.role == chat::TurnRole::Assistant)
            && t.turn
                .reasoning
                .as_deref()
                .is_some_and(|r| r.starts_with(want_prefix))
        {
            return t.turn.id.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("reasoning starting with {want_prefix:?} never landed in the DB");
}

#[tokio::test]
async fn live_reasoning_never_rides_the_main_stream() {
    let (base, script, conns, ready) = scripted_sse_upstream().await;
    let _ = ready.await;
    let (state, cookie, session_id) = setup(&base).await;
    // Pre-title the session so the auto-title generator's own upstream
    // call can't consume the scripted stream before the worker does.
    chat::set_session_title(&state.db, &session_id, "titled")
        .await
        .unwrap();
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
    let sse_body = resp.into_body();
    await_upstream_connection(conns.clone()).await;

    // Reasoning phase: one chunk lands, the shell (with the live
    // timer) flushes — and the reasoning TEXT itself must not be on
    // the wire. That text is only available through the on-demand
    // /thinking sub-stream the user opts into by expanding the
    // trace.
    script
        .send(Some(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"the hidden plan unfolds\"}}]}\n\n"
                .into(),
        ))
        .await
        .unwrap();
    let (seen, sse_body, _) = drain_sse_for(sse_body, std::time::Duration::from_millis(700)).await;
    assert!(
        seen.contains("<thinking-timer"),
        "the shell with the live timer must flush during reasoning:\n{seen}"
    );
    assert!(
        !seen.contains("the hidden plan unfolds"),
        "live reasoning must not ship on the main stream:\n{seen}"
    );
    assert!(
        seen.contains("data-on:toggle"),
        "the collapsed trace must carry the opt-in sub-stream trigger:\n{seen}"
    );

    // Content + finish: the reasoning is final now, so the settled
    // render carries it — exactly once.
    script
        .send(Some(
            "data: {\"choices\":[{\"delta\":{\"content\":\"The visible answer.\"}}]}\n\n".into(),
        ))
        .await
        .unwrap();
    script.send(Some("data: [DONE]\n\n".into())).await.unwrap();
    script.send(None).await.unwrap();
    let (rest, _, _) = drain_sse_for(sse_body, std::time::Duration::from_secs(5)).await;
    let full = format!("{seen}{rest}");
    assert_eq!(
        full.matches("the hidden plan unfolds").count(),
        1,
        "finalized reasoning ships exactly once (never per-tick):\n{full}"
    );
    assert!(
        full.contains("The visible answer."),
        "the answer itself must stream:\n{full}"
    );
    assert!(
        full.contains(r#"data: signals {"chatStreaming":false}"#),
        "the stream must close with the idle signal:\n{full}"
    );
}

#[tokio::test]
async fn thinking_substream_streams_the_live_trace_and_closes() {
    let (base, script, conns, ready) = scripted_sse_upstream().await;
    let _ = ready.await;
    let (state, cookie, session_id) = setup(&base).await;
    chat::set_session_title(&state.db, &session_id, "titled")
        .await
        .unwrap();
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
    let _main_body = resp.into_body();
    await_upstream_connection(conns.clone()).await;

    script
        .send(Some(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"trace part one\"}}]}\n\n"
                .into(),
        ))
        .await
        .unwrap();
    let turn_id = await_reasoning(&state, &session_id, "trace part one").await;

    // Opt-in: expanding the trace opens the sub-stream, which ships
    // only the thinking-body interior.
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/chat/{session_id}/turns/{turn_id}/thinking"))
        .header("cookie", format!("id={cookie}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let selector = format!("data: selector #turn-{turn_id}-thinking-body");
    let thinking_body_stream = resp.into_body();
    let (first, thinking_body_stream, _) =
        drain_sse_for(thinking_body_stream, std::time::Duration::from_millis(700)).await;
    assert!(
        first.contains(&selector),
        "sub-stream patches the thinking body interior:\n{first}"
    );
    assert!(
        first.contains("trace part one"),
        "the live snapshot arrives immediately:\n{first}"
    );

    // Live updates flow while the trace grows.
    script
        .send(Some(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\" and part two\"}}]}\n\n"
                .into(),
        ))
        .await
        .unwrap();
    let (more, thinking_body_stream, _) =
        drain_sse_for(thinking_body_stream, std::time::Duration::from_millis(700)).await;
    assert!(
        more.contains("and part two"),
        "growing reasoning streams while the trace is open:\n{more}"
    );

    // Finalize closes the sub-stream after one last patch.
    script
        .send(Some(
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n".into(),
        ))
        .await
        .unwrap();
    script.send(Some("data: [DONE]\n\n".into())).await.unwrap();
    script.send(None).await.unwrap();
    // The sub-stream closes cleanly at finalize — with no redundant
    // re-patch when the last emit already carried the final trace.
    let (_, _, ended) =
        drain_sse_for(thinking_body_stream, std::time::Duration::from_secs(5)).await;
    assert!(ended, "the sub-stream must end when the turn finalizes");

    // Anonymous access is redirected like every other authed route.
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/chat/{session_id}/turns/{turn_id}/thinking"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn main_stream_wire_stays_linear_in_content_size() {
    // 60 paragraphs x ~200 chars streamed one paragraph per delta.
    // The pre-delta protocol re-rendered the whole bubble per delta
    // (≈60 x avg-half-content ≈ 30x the content itself); the delta
    // protocol must stay within a small constant factor.
    let upstream = MockServer::start().await;
    let paragraphs: Vec<String> = (0..60)
        .map(|i| {
            format!(
                "paragraph {i:02} {} — and some more words to pad it out nicely\n\n",
                "lorem ipsum dolor sit amet".repeat(6)
            )
        })
        .collect();
    let content_len: usize = paragraphs.iter().map(|p| p.len()).sum::<usize>();
    let sse_body = paragraphs
        .iter()
        .map(|p| {
            format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"delta":{"content":p}}]})
            )
        })
        .collect::<String>()
        + "data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&upstream)
        .await;

    let (state, cookie, session_id) = setup(&upstream.uri()).await;
    let app = router(state.clone());
    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "write lots")]);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/chat/{session_id}/messages"))
        .header("cookie", format!("id={cookie}"))
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();

    // Sanity: the full answer landed in the DB.
    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    let asst = turns
        .iter()
        .find(|t| t.turn.role == chat::TurnRole::Assistant)
        .unwrap();
    assert_eq!(asst.turn.status, chat::TurnStatus::Completed);
    assert_eq!(
        asst.turn.content.as_deref().map(str::len),
        Some(content_len),
        "the whole answer must be persisted"
    );

    let wire: usize = body
        .lines()
        .filter(|l| l.starts_with("data: elements "))
        .map(|l| l.len())
        .sum();
    assert!(
        wire < content_len * 8,
        "element-patch bytes must stay linear in content size: {wire} bytes of patches for {content_len} bytes of content"
    );
    // Shell patches are phase-gated, not per-delta: with no reasoning
    // and no tool calls the whole turn needs a handful at most.
    let shell_patches = body.matches("data: selector #turn-").count();
    assert!(
        shell_patches <= 6,
        "full-shell patches must be phase-gated, found {shell_patches}:\n{body}"
    );
}

#[tokio::test]
async fn a_reply_cut_off_at_the_token_ceiling_is_marked_and_stays_replayable() {
    // Regression for "the turn looks finished but isn't": the driver used to
    // ignore `finish_reason` entirely, so an upstream that stopped at
    // `max_tokens` mid-sentence produced a turn with a timestamp, no spinner
    // and nothing else — indistinguishable from a real answer.
    //
    // The turn must say it was cut off. It must ALSO stay `Completed`: the
    // partial text is real output, and a non-completed assistant turn is
    // skipped by `message_for_history`, so erroring it would delete the answer
    // from the model's own context — the notice tells the user to ask it to
    // continue, and it has to have something to continue from.
    let upstream = MockServer::start().await;
    let truncated = "data: {\"choices\":[{\"delta\":{\"content\":\"Here is the plan: first I will\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
         data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(truncated, "text/event-stream"))
        .mount(&upstream)
        .await;

    let (state, cookie, session_id) = setup(&upstream.uri()).await;
    let app = router(state.clone());

    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "write me a report")]);
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
        .expect("assistant turn");
    let notice = asst.turn.error_message.as_deref().unwrap_or_default();
    assert!(
        notice.contains("maximum output length"),
        "the turn must carry a notice saying the reply was cut off at the \
         ceiling, got: {notice:?}"
    );
    assert!(
        notice.contains("max_tokens"),
        "…and name the knob an operator raises, got: {notice:?}"
    );
    assert_eq!(
        asst.turn.status,
        chat::TurnStatus::Completed,
        "a truncated turn produced usable output: erroring it would drop the \
         partial answer from the replayed history, break `continue`, 502 the \
         webhook path and fail the scheduled-run path"
    );
    // Everything the model did manage to say stays on screen.
    assert_eq!(
        asst.turn.content.as_deref(),
        Some("Here is the plan: first I will"),
        "the partial reply must be kept, not discarded"
    );

    // …and the model can actually see it on the next turn. This is the half
    // the status choice exists for.
    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "continue")]);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/chat/{session_id}/messages"))
        .header("cookie", format!("id={cookie}"))
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    let _ = resp.into_body().collect().await.unwrap().to_bytes();

    let sent = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    let last = sent
        .iter()
        .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
        .rfind(|b| b["stream"] == serde_json::json!(true))
        .expect("the second streamed turn");
    let replayed = last["messages"].to_string();
    assert!(
        replayed.contains("Here is the plan: first I will"),
        "the truncated reply must be replayed to the model, or \"continue\" \
         starts from nothing: {replayed}"
    );
}

#[tokio::test]
async fn a_turn_that_writes_nothing_says_so_instead_of_looking_answered() {
    // The other half of "did it finish or is it still working?": the model
    // ends the turn having emitted no content at all. The bubble settles, the
    // spinner clears and a timestamp appears — the exact shape of a finished
    // reply — except there is no reply in it. The user is left unable to tell
    // an empty answer from one still on its way.
    let upstream = MockServer::start().await;
    // A well-formed stream that simply carries no content delta.
    let empty = "data: {\"choices\":[{\"delta\":{}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(empty, "text/event-stream"))
        .mount(&upstream)
        .await;

    let (state, cookie, session_id) = setup(&upstream.uri()).await;
    let app = router(state.clone());

    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "say something")]);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/chat/{session_id}/messages"))
        .header("cookie", format!("id={cookie}"))
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.into_body().collect().await.unwrap().to_bytes();

    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    let asst = turns
        .iter()
        .find(|t| t.turn.role == chat::TurnRole::Assistant)
        .expect("assistant turn");
    let notice = asst.turn.error_message.as_deref().unwrap_or_default();
    assert!(
        notice.contains("without writing a reply"),
        "an empty turn must say it is empty rather than presenting as answered, \
         got: {notice:?}"
    );
    assert_eq!(
        asst.turn.status,
        chat::TurnStatus::Completed,
        "nothing failed — the turn ran to completion, it just said nothing"
    );
}

#[tokio::test]
async fn a_normal_stop_still_completes() {
    // The guard above must never fire on a healthy turn: `finish_reason: stop`
    // is the overwhelmingly common case and a false positive turns a good
    // answer into an error alert.
    let upstream = MockServer::start().await;
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"all done\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
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
    let _ = resp.into_body().collect().await.unwrap().to_bytes();

    let turns = chat::list_turns(&state.db, &session_id).await.unwrap();
    let asst = turns
        .iter()
        .find(|t| t.turn.role == chat::TurnRole::Assistant)
        .expect("assistant turn");
    assert_eq!(asst.turn.status, chat::TurnStatus::Completed);
    assert_eq!(asst.turn.content.as_deref(), Some("all done"));
}

#[tokio::test]
async fn the_turn_discipline_rule_actually_reaches_the_upstream_request() {
    // Wiring guard for the "turn ends on a promise" fix. The unit test proves
    // `leading_system_message` composes the rule; this proves it survives the
    // whole path and lands on the wire as the FIRST message of the request the
    // backend receives — a single leading `system` turn (several would be
    // rejected outright by the Qwen3 vLLM chat template).
    //
    // Deliberately checked against a bare conversation: no name, no IP, no
    // skills, no compaction. That case used to send no system message at all,
    // which is exactly the case this rule has to cover.
    let upstream = MockServer::start().await;
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
         data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&upstream)
        .await;

    let (state, cookie, session_id) = setup(&upstream.uri()).await;
    let app = router(state.clone());

    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "make me a report")]);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/chat/{session_id}/messages"))
        .header("cookie", format!("id={cookie}"))
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    let _ = resp.into_body().collect().await.unwrap().to_bytes();

    // The title-generation pass hits the same mock, so pick the streaming
    // request — the turn itself — rather than whichever landed first.
    let sent = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    let body = sent
        .iter()
        .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
        .find(|b| b["stream"] == serde_json::json!(true))
        .expect("the streamed turn request");
    let messages = body["messages"].as_array().expect("messages array");

    assert_eq!(
        messages[0]["role"], "system",
        "the rule must lead the request: {messages:#?}"
    );
    let system = messages[0]["content"].as_str().unwrap();
    assert!(
        system.contains("no background execution"),
        "the upstream must be told nothing of the model's runs on after the \
         message ends; got: {system}"
    );
    assert!(
        system.contains("never end a message on an announcement"),
        "…and that an announcement is only allowed when the call follows it in \
         the same turn; got: {system}"
    );
    assert_eq!(
        messages.iter().filter(|m| m["role"] == "system").count(),
        1,
        "exactly ONE leading system turn — backends reject more: {messages:#?}"
    );
    // The user's own message still travels, unmodified, right after it.
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "make me a report");
}

// ---- tool-loop wiring on the chat driver ----------------------------------
//
// The two failures reported against the chat UI, both of which only appear
// after several tool rounds and so are invisible to any single-round test.

/// Chat state whose registry holds `company_echo`, granted to role `engineer`.
/// Echo returns its argument verbatim, which is the cheapest way to make a
/// tool result of an arbitrary, controlled size.
async fn state_with_echo_tool(upstream_uri: &str) -> RamaState {
    use gateway_core::server::rbac::config::{RbacConfig, RoleConfig, RoleMapping};
    use gateway_runtime::server::tools::echo::Echo;

    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mut pools = HashMap::new();
    pools.insert(
        "pool".to_string(),
        UpstreamPoolConfig {
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                alias: None,
                probe_models: true,
                supports_edit: false,
                name: "mock".into(),
                base_url: upstream_uri.into(),
                api_key_env: None,
                api_key: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    common::seed_pool_models(&registry, "pool", 0, &["model-a"]);
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
            admin: false,
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
        Arc::new(ToolRegistry::new().with(Echo)),
        Arc::new(rbac),
    );
    let sessions = SessionStore::new(pool, common::TEST_SECRET);
    RamaState::new(
        app,
        sessions,
        gateway_core::server::usage::UsageHandle::disabled(),
    )
}

/// A chat session belonging to an engineer, with `company_echo` switched on
/// for the conversation (tools are per-conversation opt-in) and `effort` set
/// so the round cap is known.
async fn echo_session(state: &RamaState, effort: &str) -> (String, String) {
    use gateway_core::server::db::users;
    use jiff::Timestamp;
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
            speech_voice: None,
        },
    )
    .await
    .unwrap();
    let sess = state.sessions.create("alice").await.unwrap();
    let cookie = state.sessions.sign(&sess.id);
    let chat_session = chat::create_session(&state.db, "alice").await.unwrap();
    gateway_core::server::db::chat_session_tools::set(
        &state.db,
        &chat_session.id,
        gateway_runtime::server::tools::catalog::entry_key_for("company_echo"),
        true,
        "manual",
    )
    .await
    .unwrap();
    gateway_core::server::db::chat_session_settings::set_effort(
        &state.db,
        &chat_session.id,
        effort,
    )
    .await
    .unwrap();
    (cookie, chat_session.id)
}

/// An upstream that asks for `rounds` tool calls, each echoing `payload`, and
/// then answers. `stream: true` requests only — the title pass shares the mock.
async fn echoing_upstream(rounds: usize, payload: String) -> MockServer {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let upstream = MockServer::start().await;
    let seen = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            let streaming = serde_json::from_slice::<serde_json::Value>(&req.body)
                .map(|b| b["stream"] == serde_json::json!(true))
                .unwrap_or(false);
            if !streaming {
                // The title-generation pass.
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": "t"}}]
                }));
            }
            let n = seen.fetch_add(1, Ordering::SeqCst);
            let body = if n < rounds {
                let args =
                    serde_json::to_string(&serde_json::json!({"message": payload})).expect("args");
                format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices": [{"delta": {"role": "assistant",
                        "content": null,
                        "tool_calls": [{"index": 0, "id": format!("call_{n}"),
                            "type": "function",
                            "function": {"name": "company_echo", "arguments": args}}]},
                        "finish_reason": null}]}),
                    serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
                )
            } else {
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices": [{"delta": {"content": "done"}}]}),
                )
            };
            ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    upstream
}

fn send_message(session_id: &str, cookie: &str) -> Request {
    let (ct, body) = multipart_text(&[("model", "model-a"), ("message", "read the whole file")]);
    Request::builder()
        .method(Method::POST)
        .uri(format!("/chat/{session_id}/messages"))
        .header("cookie", format!("id={cookie}"))
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap()
}

/// Every streamed (turn) request the upstream saw, oldest first.
async fn streamed_requests(upstream: &MockServer) -> Vec<serde_json::Value> {
    upstream
        .received_requests()
        .await
        .expect("recorded requests")
        .iter()
        .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
        .filter(|b| b["stream"] == serde_json::json!(true))
        .collect()
}

/// The reported RAG failure, at the level it actually happens: a turn that
/// keeps calling tools runs the prompt past the turn's tool-output allowance,
/// and from then on every result — including the one that would finally read
/// the document — arrives clamped to the 2 KB floor with a note saying not to
/// ask again.
///
/// The chat driver was the one tool loop that never reclaimed anything: it
/// accumulated every result verbatim *and* charged the turn for all of them.
/// So the guard is two-sided — older results must be traded for a re-callable
/// stub, and a late result must still come back at a useful size.
#[tokio::test]
async fn a_long_tool_loop_reclaims_context_instead_of_starving_later_results() {
    // Comfortably under the per-result share (19_660 bytes at the 32k default
    // window) but four of them blow the turn's 45_875-byte allowance.
    let payload = "y".repeat(15_000);
    let upstream = echoing_upstream(6, payload).await;
    let state = Arc::new(state_with_echo_tool(&upstream.uri()).await);
    let (cookie, session_id) = echo_session(&state, "standard").await;
    let app = router(state.clone());

    let resp = app.serve(send_message(&session_id, &cookie)).await.unwrap();
    let _ = resp.into_body().collect().await.unwrap().to_bytes();

    let sent = streamed_requests(&upstream).await;
    assert!(
        sent.len() >= 6,
        "expected a multi-round turn, got {}",
        sent.len()
    );
    let last = sent.last().expect("a final round");
    let tool_results: Vec<&str> = last["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert!(tool_results.len() >= 5, "{tool_results:?}");

    assert!(
        tool_results
            .iter()
            .any(|c| c.contains("earlier tool output cleared to save context")),
        "older results must be evicted to make room; sizes: {:?}",
        tool_results.iter().map(|c| c.len()).collect::<Vec<_>>()
    );
    // The point of evicting: the room comes back. Before the fix the last
    // result was clamped to the 2 KB floor no matter how much had been freed.
    let last_result = tool_results.last().expect("a most recent result");
    assert!(
        last_result.len() > 8_192,
        "a late result was starved to {} bytes: {}",
        last_result.len(),
        &last_result[..last_result.len().min(200)]
    );
}

/// The other half of the same root cause: the gateway knew the real context
/// window all along and threw it away. The `/models` probe reads
/// `max_model_len`; nothing carried it to the budget, so every model fell back
/// to the global 32768 default. A model actually serving 262144 got an eighth
/// of the tool-output allowance it should have — which is what made a
/// six-search turn overflow in the first place.
///
/// With the discovered window in place the same turn fits comfortably: every
/// result arrives whole and nothing has to be evicted at all.
#[tokio::test]
async fn a_discovered_context_window_sizes_the_tool_budget() {
    let payload = "y".repeat(15_000);
    let upstream = echoing_upstream(6, payload).await;
    let state = Arc::new(state_with_echo_tool(&upstream.uri()).await);
    // What the probe learns from a real vLLM `/models` response.
    for pool in state.upstreams.pools() {
        for backend in &pool.backends {
            backend.set_context_windows(HashMap::from([("model-a".to_string(), 262_144i64)]));
        }
    }
    let (cookie, session_id) = echo_session(&state, "standard").await;
    let app = router(state.clone());

    let resp = app.serve(send_message(&session_id, &cookie)).await.unwrap();
    let _ = resp.into_body().collect().await.unwrap().to_bytes();

    let sent = streamed_requests(&upstream).await;
    let tool_results: Vec<&str> = sent.last().expect("a final round")["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert_eq!(tool_results.len(), 6, "{tool_results:?}");
    assert!(
        tool_results.iter().all(|c| c.len() > 14_000),
        "nothing should be cut at the real window; sizes: {:?}",
        tool_results.iter().map(|c| c.len()).collect::<Vec<_>>()
    );
    assert!(
        !tool_results
            .iter()
            .any(|c| c.contains("earlier tool output cleared")),
        "and nothing should need evicting"
    );
}

/// The reported 400, at the level it happens. On its final round the driver
/// tells the model, in words, that no further tool call can run. Appending
/// that as a second `system` message is rejected outright by the Qwen3 vLLM
/// chat template ("System message must be at the beginning"), which killed
/// every round-budget-exhausting turn *after* all its tool work was done.
///
/// `effort=fast` caps the loop at 8 rounds so the final one is reached cheaply.
#[tokio::test]
async fn the_final_round_reaches_the_upstream_with_one_leading_system_message() {
    // More tool calls than the round budget, so the loop runs out.
    let upstream = echoing_upstream(50, "small".into()).await;
    let state = Arc::new(state_with_echo_tool(&upstream.uri()).await);
    let (cookie, session_id) = echo_session(&state, "fast").await;
    let app = router(state.clone());

    let resp = app.serve(send_message(&session_id, &cookie)).await.unwrap();
    let _ = resp.into_body().collect().await.unwrap().to_bytes();

    let sent = streamed_requests(&upstream).await;
    let last = sent.last().expect("a final round");
    let messages = last["messages"].as_array().expect("messages");
    let system_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m["role"] == "system")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        system_idxs,
        vec![0],
        "exactly one system message, at the front — anything else is a 400 on \
         Qwen3 vLLM: {messages:#?}"
    );
    let system = messages[0]["content"].as_str().expect("string content");
    assert!(
        system.contains("FINAL round"),
        "the final-round notice must still reach the model: {system}"
    );
    assert_eq!(
        last["tool_choice"],
        serde_json::json!("none"),
        "and the tools must be withheld on that round"
    );
}
