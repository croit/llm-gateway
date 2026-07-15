// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/api/v0/push/*` — Web Push opt-in endpoints.
//!
//! Pins the wiring the UI depends on: the endpoints are session-gated, they
//! report the feature's on/off state honestly, and a subscribe→unsubscribe
//! round-trip actually lands (and clears) a row. The VAPID/RFC-8291 crypto
//! itself is unit-tested in `server::push`; here we only exercise the HTTP
//! contract and the enabled-vs-disabled branches.

mod common;

use common::Service as _;
use gateway::rama_server::RamaState;
use gateway::server::crypto::Crypto;
use gateway::server::db::push_subscriptions;
use gateway::server::push::PushSender;
use rama::http::{Body, Method, Request, StatusCode};
use std::sync::Arc;

fn req_with_cookie(method: Method, uri: &str, cookie: &str, body: Option<&str>) -> Request {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("id={cookie}"));
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    b.body(Body::from(body.unwrap_or("").to_string())).unwrap()
}

/// A state with Web Push enabled (a real VAPID keypair generated + sealed in
/// its in-memory DB).
async fn state_with_push() -> RamaState {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let sender = PushSender::new(
        &state.db,
        &Crypto::from_key([1u8; 32]),
        "mailto:test@example.com".to_string(),
    )
    .await
    .expect("build PushSender");
    state.with_push(Arc::new(sender))
}

// ---- auth gating ---------------------------------------------------------

#[tokio::test]
async fn config_requires_session() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::GET, "/api/v0/push/config"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn subscribe_requires_session() {
    let state = state_with_push().await;
    let app = common::app(state);
    let resp = app
        .serve(common::req(Method::POST, "/api/v0/push/subscribe"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---- disabled behaviour --------------------------------------------------

#[tokio::test]
async fn config_reports_disabled_when_push_off() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            "/api/v0/push/config",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(json["enabled"], false);
    assert!(json["publicKey"].is_null());
}

#[tokio::test]
async fn subscribe_is_503_when_push_off() {
    let state = common::state_with_chat_pool("http://unused.invalid").await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);
    let body = r#"{"endpoint":"https://push.example.com/x","keys":{"p256dh":"BPk","auth":"AAA"}}"#;
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/push/subscribe",
            &cookie,
            Some(body),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

// ---- enabled behaviour ---------------------------------------------------

#[tokio::test]
async fn config_reports_enabled_with_key() {
    let state = state_with_push().await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            "/api/v0/push/config",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    assert_eq!(json["enabled"], true);
    // A base64url-no-pad 65-byte uncompressed P-256 point is 87 chars
    // (65 = 3·21 + 2 → 84 + 3).
    let key = json["publicKey"].as_str().expect("publicKey string");
    assert_eq!(key.len(), 87, "VAPID public key length");
}

#[tokio::test]
async fn subscribe_then_unsubscribe_round_trips() {
    let state = state_with_push().await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let endpoint = "https://push.example.com/abc123";
    // Valid RFC 8291 §5 key material (65-byte P-256 point + 16-byte auth) so it
    // passes subscribe-time validation.
    let p256dh =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    let auth = "BTBZMqHH6r4Tts7J_aSIgg";
    let body =
        format!(r#"{{"endpoint":"{endpoint}","keys":{{"p256dh":"{p256dh}","auth":"{auth}"}}}}"#);
    let app = common::app(state.clone());

    // Subscribe → row stored for the session user.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/push/subscribe",
            &cookie,
            Some(&body),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let subs = push_subscriptions::list_for_user(&state.db, "alice")
        .await
        .unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].endpoint, endpoint);
    assert_eq!(subs[0].p256dh, p256dh);

    // Unsubscribe → row gone.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/push/unsubscribe",
            &cookie,
            Some(&format!(r#"{{"endpoint":"{endpoint}"}}"#)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        push_subscriptions::list_for_user(&state.db, "alice")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn subscribe_rejects_ssrf_endpoint() {
    let state = state_with_push().await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state.clone());
    // Cloud metadata IP with otherwise-valid keys must be refused before storage.
    let p256dh =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    let body = format!(
        r#"{{"endpoint":"https://169.254.169.254/x","keys":{{"p256dh":"{p256dh}","auth":"BTBZMqHH6r4Tts7J_aSIgg"}}}}"#
    );
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/push/subscribe",
            &cookie,
            Some(&body),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        push_subscriptions::list_for_user(&state.db, "alice")
            .await
            .unwrap()
            .is_empty(),
        "SSRF endpoint must not be stored"
    );
}

#[tokio::test]
async fn subscribe_rejects_incomplete_body() {
    let state = state_with_push().await;
    let cookie = common::seed_session(&state, "alice", "alice@example.com").await;
    let app = common::app(state);
    // Missing `keys`.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/push/subscribe",
            &cookie,
            Some(r#"{"endpoint":"https://push.example.com/x"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
