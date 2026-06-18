// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! /theme/toggle + flash cookie roundtrip + the session-authed
//! transcription mirror. All three are part of the chrome the chat
//! page expects.

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
use rama::http::{Body, Method, Request, StatusCode};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn set_cookie_values(resp: &rama::http::Response) -> Vec<String> {
    resp.headers()
        .get_all(rama::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(String::from))
        .collect()
}

#[tokio::test]
async fn theme_toggle_returns_sse_patch_and_sets_cookie() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = router(Arc::new(state));

    // Anonymous + no cookie → server treats current as Dark, flips to Light.
    // Response is now SSE (was 303 before the datastar refactor); the
    // Set-Cookie still rides along so a full reload picks up the new theme.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/theme/toggle")
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cookies = set_cookie_values(&resp);
    let theme_cookie = cookies
        .iter()
        .find(|c| c.starts_with("theme="))
        .expect("theme cookie set");
    assert!(
        theme_cookie.starts_with("theme=light;"),
        "expected theme=light, got `{theme_cookie}`"
    );
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("event: datastar-patch-elements"),
        "expected at least one datastar patch event, got:\n{body}"
    );
    assert!(
        body.contains("data: selector #theme-toggle-form"),
        "expected the form-swap patch, got:\n{body}"
    );
    assert!(
        body.contains("data-theme") && body.contains("light"),
        "expected the inline script to set data-theme=light, got:\n{body}"
    );

    // With theme=light set, toggle flips back to dark.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/theme/toggle")
        .header("cookie", "theme=light")
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    let cookies = set_cookie_values(&resp);
    let theme_cookie = cookies
        .iter()
        .find(|c| c.starts_with("theme="))
        .expect("theme cookie set");
    assert!(
        theme_cookie.starts_with("theme=dark;"),
        "expected theme=dark, got `{theme_cookie}`"
    );
}

#[tokio::test]
async fn app_js_loads_before_datastar_so_window_globals_exist() {
    // Regression: datastar processes `data-init` (e.g. window.chatScroll.init
    // on #conversation) during its own module execution, so app.js — which
    // defines the window.chat* globals — MUST run first. Both <script>s are
    // deferred, so document order decides: app.js must appear before
    // datastar.js in the page, or the init throws "chatScroll is undefined".
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = router(Arc::new(state));

    let req = Request::builder()
        .method(Method::GET)
        .uri("/tokens")
        .header("cookie", format!("id={cookie}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    let app_pos = body.find("app.js").expect("app.js <script> present");
    let datastar_pos = body
        .find("datastar.js")
        .expect("datastar.js <script> present");
    assert!(
        app_pos < datastar_pos,
        "app.js (defines window.chatScroll) must load before datastar.js; \
         got app@{app_pos}, datastar@{datastar_pos}"
    );
}

#[tokio::test]
async fn revoke_returns_sse_with_row_swap_and_toast() {
    use gateway::server::auth::token;
    use gateway::server::db::tokens;
    use jiff::{SignedDuration, Timestamp};
    use uuid::Uuid;

    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;

    let now = Timestamp::now();
    let (_, hash) = token::mint();
    let token_id = Uuid::new_v4().to_string();
    tokens::insert(
        &state.db,
        &tokens::Token {
            id: token_id.clone(),
            user_id: "alice".into(),
            name: "test".into(),
            hash,
            created_at: now,
            last_used_at: None,
            expires_at: now + SignedDuration::from_hours(1),
            revoked_at: None,
        },
    )
    .await
    .unwrap();

    let app = router(Arc::new(state));

    // POST /tokens/{id}/revoke now returns 200 + text/event-stream
    // with two datastar-patch-elements events:
    //   1. `mode outer` on `#token-row-<id>` swapping in the revoked
    //      variant of the row,
    //   2. `mode append` on `#toasts` inserting the success toast.
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/tokens/{token_id}/revoke"))
        .header("cookie", format!("id={cookie}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Pull what we need off the response before `read_body` consumes
    // it: content-type for the stream-vs-html check and Set-Cookies
    // for the "no flash cookie anymore" assertion at the bottom.
    let ct = resp
        .headers()
        .get(rama::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_default();
    let set_cookies = set_cookie_values(&resp);
    assert!(
        ct.starts_with("text/event-stream"),
        "expected text/event-stream, got `{ct}`"
    );

    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    let patches = body.matches("event: datastar-patch-elements").count();
    assert_eq!(patches, 2, "expected two patch events, body was:\n{body}");
    let row_selector = format!("data: selector #token-row-{token_id}");
    assert!(
        body.contains(&row_selector),
        "expected row-targeted patch, got:\n{body}"
    );
    assert!(
        body.contains("data: mode outer"),
        "expected row swap via `mode outer`, got:\n{body}"
    );
    assert!(
        body.contains("data: selector #toasts"),
        "expected toast appended to #toasts, got:\n{body}"
    );
    assert!(
        body.contains("Token revoked."),
        "expected the revoke toast text, got:\n{body}"
    );
    assert!(
        body.contains("badge-error") && body.contains("revoked"),
        "expected the swapped-in row to be the revoked variant, got:\n{body}"
    );

    // Nothing should be set as a cookie — the entire interaction is
    // SSE-driven; the old flash-cookie roundtrip is gone.
    assert!(
        set_cookies.is_empty(),
        "expected no Set-Cookie headers, got: {set_cookies:?}"
    );
}

#[tokio::test]
async fn transcription_models_lists_discovered_models() {
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mut pools = HashMap::new();
    pools.insert(
        "voice".to_string(),
        UpstreamPoolConfig {
            kind: PoolKind::Transcription,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                name: "whisper".into(),
                base_url: "http://unused.invalid".into(),
                api_key_env: None,
                weight: 1,
                max_inflight: 4,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    // No static routes: `seed_pool_models` mimics what the health probe
    // would have written after parsing the upstream's `/models`
    // response. The dropdown is populated from that set.
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    common::seed_pool_models(&registry, "voice", 0, &["whisper-large-v3"]);
    let app = AppState::new(
        Config::default(),
        pool.clone(),
        registry,
        Arc::new(ToolRegistry::new()),
        Arc::new(Resolver::empty()),
    );
    let sessions = SessionStore::new(pool, common::TEST_SECRET);
    let state = RamaState::new(app, sessions);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = router(Arc::new(state));

    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/v0/transcription_models")
        .header("cookie", format!("id={cookie}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    let data = parsed["data"].as_array().expect("data array");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0], "whisper-large-v3");
}

#[tokio::test]
async fn transcription_models_rejects_anonymous() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = router(Arc::new(state));
    let resp = app
        .serve(common::req(Method::GET, "/api/v0/transcription_models"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn session_transcribe_forwards_multipart_to_upstream() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "text": "hello world" })),
        )
        .mount(&upstream)
        .await;

    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mut pools = HashMap::new();
    pools.insert(
        "voice".to_string(),
        UpstreamPoolConfig {
            kind: PoolKind::Transcription,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                name: "whisper".into(),
                base_url: upstream.uri(),
                api_key_env: None,
                weight: 1,
                max_inflight: 4,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    common::seed_pool_models(&registry, "voice", 0, &["whisper-large-v3"]);
    let app = AppState::new(
        Config::default(),
        pool.clone(),
        registry,
        Arc::new(ToolRegistry::new()),
        Arc::new(Resolver::empty()),
    );
    let sessions = SessionStore::new(pool, common::TEST_SECRET);
    let state = RamaState::new(app, sessions);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = router(Arc::new(state));

    // Minimal multipart body: `model=whisper-large-v3` + `file=<audio>`.
    let boundary = "----test-boundary";
    let body = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"model\"\r\n\r\n\
         whisper-large-v3\r\n\
         --{boundary}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"audio.webm\"\r\n\
         Content-Type: audio/webm\r\n\r\n\
         FAKE_AUDIO_BYTES\r\n\
         --{boundary}--\r\n"
    );
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v0/transcriptions")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("cookie", format!("id={cookie}"))
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["text"], "hello world");
}

#[tokio::test]
async fn session_transcribe_rejects_anonymous() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = router(Arc::new(state));
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v0/transcriptions")
        .header("content-type", "multipart/form-data; boundary=x")
        .body(Body::from("--x--"))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// 16 kHz mono PCM-16 WAV with `n_samples` zero-valued samples — the
/// exact format the browser worklet emits, so the gateway's
/// duration gate (which only measures WAVs in that format) actually
/// sees the sample count.
fn synth_pcm16_mono_16k_wav(n_samples: usize) -> Vec<u8> {
    let bytes_per_sample: u16 = 2;
    let channels: u16 = 1;
    let sample_rate: u32 = 16_000;
    let byte_rate = sample_rate * u32::from(channels) * u32::from(bytes_per_sample);
    let block_align = channels * bytes_per_sample;
    let data_size = (n_samples * usize::from(bytes_per_sample)) as u32;
    let mut out = Vec::with_capacity(44 + data_size as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_size).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    for _ in 0..n_samples {
        out.extend_from_slice(&0i16.to_le_bytes());
    }
    out
}

#[tokio::test]
async fn session_transcribe_rejects_too_short_audio() {
    // 1600 samples at 16 kHz = 100 ms — well below the 400 ms
    // floor `handle_transcription` enforces to keep voxtral's
    // multimodal embedder from wedging on near-empty input.
    //
    // Upstream is intentionally NOT mocked: a request reaching the
    // proxy would fall over with an upstream-unreachable error, but
    // the duration gate should short-circuit before that point and
    // return a clear 400. If this test starts seeing 502s instead
    // of 400s, the gate has regressed.
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let mut pools = HashMap::new();
    pools.insert(
        "voice".to_string(),
        UpstreamPoolConfig {
            kind: PoolKind::Transcription,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                name: "voxtral".into(),
                base_url: "http://unused.invalid".into(),
                api_key_env: None,
                weight: 1,
                max_inflight: 4,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    let registry = upstreams::UpstreamRegistry::new(&pools).unwrap();
    common::seed_pool_models(&registry, "voice", 0, &["voxtral"]);
    let app = AppState::new(
        Config::default(),
        pool.clone(),
        registry,
        Arc::new(ToolRegistry::new()),
        Arc::new(Resolver::empty()),
    );
    let sessions = SessionStore::new(pool, common::TEST_SECRET);
    let state = RamaState::new(app, sessions);
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = router(Arc::new(state));

    let wav = synth_pcm16_mono_16k_wav(1600);
    let boundary = "----test-boundary";
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"model\"\r\n\r\n\
             voxtral\r\n\
             --{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"recording.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v0/transcriptions")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("cookie", format!("id={cookie}"))
        .body(Body::from(body))
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(parsed["error"]["code"], "audio_too_short");
}
