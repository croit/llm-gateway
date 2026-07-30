// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The per-user spoken-reply voice: `POST /api/v0/me/speech_voice`, the
//! resolution the speech path performs, and the header picker that drives it.
//!
//! The contract under test, end to end:
//!
//!   * the menu is the operator's declared voice set — never free text;
//!   * a stored pick beats the pool's language→voice default;
//!   * a pick the operator has since retired falls back to that default
//!     instead of reaching the upstream as an unknown id;
//!   * the picker renders the *stored* selection on load (not the first
//!     option), and stays away entirely when there is nothing to choose.

use std::sync::Arc;

use gateway_core::server::db::users;
use rama::http::{Body, Method, Request, StatusCode};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gateway::rama_server::{RamaState, router::service};

use crate::common::{self, Service as _};

/// The voices the fixture's operator declares: a catch-all default plus two
/// language-specific ones. `speech_voices_for` therefore offers three.
const VOICES: &[(&str, &str)] = &[("", "alloy"), ("de", "onyx"), ("en", "nova")];

const MODEL: &str = "tts-1";

async fn post_voice(state: &Arc<RamaState>, cookie: &str, body: serde_json::Value) -> StatusCode {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v0/me/speech_voice")
        .header("content-type", "application/json")
        .header("cookie", format!("id={cookie}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    service(state.clone()).serve(req).await.unwrap().status()
}

async fn stored_voice(state: &Arc<RamaState>, user_id: &str) -> Option<String> {
    users::find_by_id(&state.db, user_id)
        .await
        .unwrap()
        .unwrap()
        .speech_voice
}

#[tokio::test]
async fn stores_a_declared_voice_and_clears_it_again() {
    let mock = MockServer::start().await;
    let state = Arc::new(common::state_with_speech_voices(&mock.uri(), MODEL, VOICES, &[]).await);
    let cookie = common::seed_session(&state, "u1", "u1@example.com").await;

    assert_eq!(
        post_voice(&state, &cookie, json!({"voice": "onyx"})).await,
        StatusCode::OK
    );
    assert_eq!(stored_voice(&state, "u1").await.as_deref(), Some("onyx"));

    // The picker's "default voice" option posts an empty string, which is the
    // same thing as null: hand the choice back to the pool.
    assert_eq!(
        post_voice(&state, &cookie, json!({"voice": ""})).await,
        StatusCode::OK
    );
    assert_eq!(stored_voice(&state, "u1").await, None);
}

#[tokio::test]
async fn the_menu_is_the_operators_offer_list() {
    // The point of the separate offer list: several voices for ONE language,
    // which `pool_voices` (one row per language) cannot express. Here German
    // resolves to `onyx` by default while the user may pick any of three.
    let mock = MockServer::start().await;
    let state = Arc::new(
        common::state_with_speech_voices(
            &mock.uri(),
            MODEL,
            &[("", "alloy"), ("de", "onyx")],
            &["marin", "cedar", "onyx"],
        )
        .await,
    );
    let cookie = common::seed_session(&state, "u1", "u1@example.com").await;

    // Every offered voice is accepted…
    for v in ["marin", "cedar", "onyx"] {
        assert_eq!(
            post_voice(&state, &cookie, json!({"voice": v})).await,
            StatusCode::OK,
            "offered voice {v} was rejected"
        );
    }
    // …and so is one that only the language map mentions (an operator who
    // never fills the menu still gets a working picker)…
    assert_eq!(
        post_voice(&state, &cookie, json!({"voice": "alloy"})).await,
        StatusCode::OK
    );
    // …but nothing else.
    assert_eq!(
        post_voice(&state, &cookie, json!({"voice": "verse"})).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn rejects_a_voice_the_operator_never_declared() {
    let mock = MockServer::start().await;
    let state = Arc::new(common::state_with_speech_voices(&mock.uri(), MODEL, VOICES, &[]).await);
    let cookie = common::seed_session(&state, "u1", "u1@example.com").await;

    // `shimmer` is a real OpenAI voice — but not one this deployment offers, so
    // the gateway must not store it (and must not pass it upstream later).
    assert_eq!(
        post_voice(&state, &cookie, json!({"voice": "shimmer"})).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(stored_voice(&state, "u1").await, None);
}

#[tokio::test]
async fn requires_a_session() {
    let mock = MockServer::start().await;
    let state = Arc::new(common::state_with_speech_voices(&mock.uri(), MODEL, VOICES, &[]).await);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v0/me/speech_voice")
        .header("content-type", "application/json")
        .body(Body::from(json!({"voice": "onyx"}).to_string()))
        .unwrap();
    let status = service(state.clone()).serve(req).await.unwrap().status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Drive `POST /api/v0/speech` once. What the gateway forwarded is then read
/// off the mock with [`voices_sent`] — asserting on the *upstream* request is
/// the only way to see which voice won, since the response is opaque audio.
async fn speak_once(state: &Arc<RamaState>, cookie: &str, language: &str, text: &str) {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v0/speech")
        .header("content-type", "application/json")
        .header("cookie", format!("id={cookie}"))
        .body(Body::from(
            json!({"text": text, "language": language}).to_string(),
        ))
        .unwrap();
    let resp = service(state.clone()).serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "speech call failed");
}

/// The `voice` field of every request the mock upstream received, in order.
async fn voices_sent(mock: &MockServer) -> Vec<Option<String>> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            v.get("voice").and_then(|x| x.as_str()).map(str::to_string)
        })
        .collect()
}

#[tokio::test]
async fn stored_voice_beats_the_pool_default() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/speech"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ID3fake".to_vec()))
        .mount(&mock)
        .await;
    let state = Arc::new(common::state_with_speech_voices(&mock.uri(), MODEL, VOICES, &[]).await);
    let cookie = common::seed_session(&state, "u1", "u1@example.com").await;

    // No preference yet: the pool's `en` entry decides.
    speak_once(&state, &cookie, "en", "one").await;
    // Now pick a different one; the same language must follow the user.
    assert_eq!(
        post_voice(&state, &cookie, json!({"voice": "onyx"})).await,
        StatusCode::OK
    );
    speak_once(&state, &cookie, "en", "two").await;

    let sent = voices_sent(&mock).await;
    assert_eq!(
        sent,
        vec![Some("nova".to_string()), Some("onyx".to_string())],
        "first call takes the pool's en→nova default, second the user's onyx"
    );
}

#[tokio::test]
async fn a_retired_voice_falls_back_to_the_pool_default() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/speech"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ID3fake".to_vec()))
        .mount(&mock)
        .await;
    // This deployment offers only the catch-all voice…
    let state =
        Arc::new(common::state_with_speech_voices(&mock.uri(), MODEL, &[("", "alloy")], &[]).await);
    let cookie = common::seed_session(&state, "u1", "u1@example.com").await;
    // …while the user row still holds a voice from before the operator
    // removed it (written directly: the endpoint would refuse it now).
    users::set_speech_voice(&state.db, "u1", Some("onyx"))
        .await
        .unwrap();

    speak_once(&state, &cookie, "de", "hallo").await;

    let sent = voices_sent(&mock).await;
    assert_eq!(
        sent,
        vec![Some("alloy".to_string())],
        "a voice no longer on offer must not reach the upstream"
    );
}
