// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-token scope and quota, end to end through the real `/v1` stack.
//!
//! The unit tests in `gateway-core` pin the registry and the enforcer in
//! isolation. These pin the *wiring*: that a token's allowlist reaches the
//! handlers at all (it rides on `PoolAccess`, built once per request in
//! `pool_access_for_token`), and that the token's own quota is checked
//! alongside its owner's rather than instead of it.

use rama::Service;
use rama::http::{Body, Method, Request, StatusCode};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common;

use gateway_core::server::db::limits::{self, Dimension, ManagedBy, SubjectType, Window};
use gateway_core::server::db::token_models;

/// A chat pool serving two models, so an allowlist has something to exclude.
async fn state_with_two_models(
    upstream: &MockServer,
) -> gateway_runtime::rama_server::state::RamaState {
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    common::seed_pool_models(&state.upstreams, "pool", 0, &["model-a", "model-b"]);
    state
}

fn chat_req(bearer: &str, model: &str) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model": model, "messages": []}).to_string(),
        ))
        .unwrap()
}

fn models_req(bearer: &str) -> Request {
    Request::builder()
        .method(Method::GET)
        .uri("/v1/models")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap()
}

async fn mount_chat(upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })))
        .mount(upstream)
        .await;
}

/// The default — every token issued before this feature existed, and every
/// one issued without touching the picker.
#[tokio::test]
async fn a_token_without_an_allowlist_reaches_every_model() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream).await;
    let state = state_with_two_models(&upstream).await;
    let bearer = common::seed_user_with_token(&state, "alice").await;
    let app = common::app(state);

    for model in ["model-a", "model-b"] {
        let resp = app.serve(chat_req(&bearer, model)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{model} should route");
    }

    let resp = app.serve(models_req(&bearer)).await.unwrap();
    let body = common::read_body(resp).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["data"].as_array().unwrap().len(), 2, "{parsed}");
}

/// A restricted token: the allowed model routes, the excluded one is refused
/// with a 403 that names itself — and, critically, the refusal happens for
/// the *listing* too, so the token cannot discover what it may not use.
#[tokio::test]
async fn a_restricted_token_is_refused_the_models_it_does_not_list() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream).await;
    let state = state_with_two_models(&upstream).await;
    let (bearer, token_id) = common::seed_user_with_token_id(&state, "alice").await;
    token_models::set_for_token(&state.db, &token_id, &["model-a".into()], ManagedBy::Owner)
        .await
        .unwrap();
    let app = common::app(state);

    let resp = app.serve(chat_req(&bearer, "model-a")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app.serve(chat_req(&bearer, "model-b")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = common::read_body(resp).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["error"]["code"], "model_not_allowed", "{parsed}");
    assert_eq!(parsed["error"]["param"], "model");

    // The listing agrees with what routing will do.
    let resp = app.serve(models_req(&bearer)).await.unwrap();
    let body = common::read_body(resp).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let ids: Vec<&str> = parsed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["model-a"], "{parsed}");

    // …as does the single-model lookup.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/models/model-b")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Clearing the allowlist restores the default. The write path spells
/// "unrestricted" as *no rows*, and reading it back as an empty allowlist —
/// deny everything — would lock the owner out of their own token.
#[tokio::test]
async fn clearing_the_allowlist_restores_full_access() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream).await;
    let state = state_with_two_models(&upstream).await;
    let (bearer, token_id) = common::seed_user_with_token_id(&state, "alice").await;
    token_models::set_for_token(&state.db, &token_id, &["model-a".into()], ManagedBy::Owner)
        .await
        .unwrap();
    token_models::set_for_token(&state.db, &token_id, &[], ManagedBy::Owner)
        .await
        .unwrap();
    let app = common::app(state);

    let resp = app.serve(chat_req(&bearer, "model-b")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// A per-token quota refuses the call with a 429 that says it was the token's
/// ceiling — the owner is not over budget, so a message blaming their quota
/// would send them looking in the wrong place.
#[tokio::test]
async fn a_per_token_quota_returns_429_naming_the_token() {
    let upstream = MockServer::start().await;
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let (bearer, token_id) = common::seed_user_with_token_id(&state, "alice").await;
    // Zero requests per hour on this token alone; no rule on the user.
    limits::upsert(
        &state.db,
        SubjectType::Token,
        &token_id,
        None,
        Dimension::Requests,
        Window::Hour,
        0.0,
    )
    .await
    .unwrap();
    let app = common::app(state);

    let resp = app.serve(chat_req(&bearer, "model-a")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(resp.headers().contains_key("retry-after"));
    let body = common::read_body(resp).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let msg = parsed["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("API token"),
        "the 429 must say the token's own ceiling tripped: {msg}"
    );
}

/// A quota on one token must not spend another's, even for the same owner.
#[tokio::test]
async fn one_tokens_quota_does_not_bind_another() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream).await;
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let (_capped, capped_id) = common::seed_user_with_token_id(&state, "alice").await;
    let free = common::seed_user_with_token(&state, "alice").await;
    limits::upsert(
        &state.db,
        SubjectType::Token,
        &capped_id,
        None,
        Dimension::Requests,
        Window::Hour,
        0.0,
    )
    .await
    .unwrap();
    let app = common::app(state);

    let resp = app.serve(chat_req(&free, "model-a")).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the uncapped token of the same user still works"
    );
}

// ---------------------------------------------------------------------------
// The /tokens save path. What a token *ends up* restricted to is decided here,
// so the mapping from form to stored rows is worth pinning on its own.

/// A session cookie for `user`, for the form-posting tests below.
async fn session_for(state: &gateway_runtime::rama_server::state::RamaState, user: &str) -> String {
    common::seed_session(state, user, &format!("{user}@example.com")).await
}

fn models_post(cookie: &str, token_id: &str, body: &str) -> Request {
    common::post_form(&format!("/tokens/{token_id}/models"), cookie, body)
}

/// The trap: a token restricted to exactly the models the deployment happens
/// to serve renders with every box ticked. Inferring "all ticked means
/// unrestricted" would drop the restriction the moment its owner opened the
/// panel and pressed Save — and hand the token every model added later.
#[tokio::test]
async fn saving_an_all_ticked_picker_keeps_an_existing_restriction() {
    let upstream = MockServer::start().await;
    let state = state_with_two_models(&upstream).await;
    let (_, token_id) = common::seed_user_with_token_id(&state, "alice").await;
    // Restricted to every model that currently exists.
    token_models::set_for_token(
        &state.db,
        &token_id,
        &["model-a".into(), "model-b".into()],
        ManagedBy::Owner,
    )
    .await
    .unwrap();
    let cookie = session_for(&state, "alice").await;
    let db = state.db.clone();
    let app = common::app(state);

    // The panel posts the restrict flag plus both ticks — an untouched save.
    let resp = app
        .serve(models_post(
            &cookie,
            &token_id,
            "restrict=on&models=model-a&models=model-b",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let still = token_models::for_token(&db, &token_id).await.unwrap();
    assert!(
        still.is_some(),
        "an untouched save must not silently unrestrict the token"
    );
    assert_eq!(still.unwrap().len(), 2);
}

/// Turning the limit off is the only way to clear it, and it must actually
/// clear it — the restriction is stored as "no rows".
#[tokio::test]
async fn unchecking_the_limit_clears_the_allowlist() {
    let upstream = MockServer::start().await;
    let state = state_with_two_models(&upstream).await;
    let (_, token_id) = common::seed_user_with_token_id(&state, "alice").await;
    token_models::set_for_token(&state.db, &token_id, &["model-a".into()], ManagedBy::Owner)
        .await
        .unwrap();
    let cookie = session_for(&state, "alice").await;
    let db = state.db.clone();
    let app = common::app(state);

    // No `restrict` field at all — an unchecked checkbox submits nothing.
    let resp = app
        .serve(models_post(&cookie, &token_id, "models=model-a"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(token_models::for_token(&db, &token_id).await.unwrap(), None);
}

/// "Restrict, but tick nothing" says allow-nothing, which the storage cannot
/// represent — no rows is how unrestricted is spelled. Saving it would grant
/// the exact opposite of what was asked, so the save is refused instead.
#[tokio::test]
async fn restricting_to_nothing_is_refused_rather_than_inverted() {
    let upstream = MockServer::start().await;
    let state = state_with_two_models(&upstream).await;
    let (_, token_id) = common::seed_user_with_token_id(&state, "alice").await;
    token_models::set_for_token(&state.db, &token_id, &["model-a".into()], ManagedBy::Owner)
        .await
        .unwrap();
    let cookie = session_for(&state, "alice").await;
    let db = state.db.clone();
    let app = common::app(state);

    let resp = app
        .serve(models_post(&cookie, &token_id, "restrict=on"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an SSE toast, not an error page"
    );

    let still = token_models::for_token(&db, &token_id).await.unwrap();
    assert_eq!(
        still.map(|s| s.len()),
        Some(1),
        "the previous allowlist stands; nothing was widened"
    );
}

/// A stale entry — a model the allowlist names that no pool serves any more —
/// renders ticked and must survive a save, or the token silently widens as
/// pools come and go.
#[tokio::test]
async fn a_stale_allowlist_entry_survives_a_save() {
    let upstream = MockServer::start().await;
    let state = state_with_two_models(&upstream).await;
    let (_, token_id) = common::seed_user_with_token_id(&state, "alice").await;
    token_models::set_for_token(
        &state.db,
        &token_id,
        &["model-a".into(), "retired".into()],
        ManagedBy::Owner,
    )
    .await
    .unwrap();
    let cookie = session_for(&state, "alice").await;
    let db = state.db.clone();
    let app = common::app(state);

    let resp = app
        .serve(models_post(
            &cookie,
            &token_id,
            "restrict=on&models=model-a&models=retired",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let still = token_models::for_token(&db, &token_id)
        .await
        .unwrap()
        .unwrap();
    assert!(still.contains("retired"), "{still:?}");
    assert!(still.contains("model-a"), "{still:?}");
}

/// One user must not be able to configure another's token.
#[tokio::test]
async fn a_stranger_cannot_restrict_someone_elses_token() {
    let upstream = MockServer::start().await;
    let state = state_with_two_models(&upstream).await;
    let (_, token_id) = common::seed_user_with_token_id(&state, "alice").await;
    // Mallory has a session of their own, but no claim on alice's token.
    let cookie = session_for(&state, "mallory").await;
    let db = state.db.clone();
    let app = common::app(state);

    let resp = app
        .serve(models_post(
            &cookie,
            &token_id,
            "restrict=on&models=model-a",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "SSE toast");
    assert_eq!(
        token_models::for_token(&db, &token_id).await.unwrap(),
        None,
        "alice's token is untouched"
    );
}

/// A per-token quota set by an admin is not the owner's to raise or remove —
/// otherwise capping a token would be a suggestion.
#[tokio::test]
async fn an_owner_cannot_raise_or_delete_an_admin_set_quota() {
    let upstream = MockServer::start().await;
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let (_, token_id) = common::seed_user_with_token_id(&state, "alice").await;
    limits::upsert(
        &state.db,
        SubjectType::Token,
        &token_id,
        None,
        Dimension::Requests,
        Window::Day,
        10.0,
    )
    .await
    .unwrap();
    let cookie = session_for(&state, "alice").await;
    let db = state.db.clone();
    let app = common::app(state);

    // Raise it via the self-service form.
    let resp = app
        .serve(common::post_form(
            &format!("/tokens/{token_id}/limits"),
            &cookie,
            "dimension=requests&window=day&value=99999",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "SSE toast");
    let rules = limits::applicable_for_token(&db, &token_id).await.unwrap();
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0].value, 10.0, "the admin's cap stands");

    // …and delete it.
    let resp = app
        .serve(common::post_form(
            &format!("/tokens/{token_id}/limits/delete"),
            &cookie,
            &format!("id={}", rules[0].id),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "SSE toast");
    assert_eq!(
        limits::applicable_for_token(&db, &token_id)
            .await
            .unwrap()
            .len(),
        1,
        "an admin's cap survives the owner pressing Remove"
    );
}

/// …but the owner's own rules stay theirs to manage.
#[tokio::test]
async fn an_owner_can_still_manage_their_own_quota() {
    let upstream = MockServer::start().await;
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let (_, token_id) = common::seed_user_with_token_id(&state, "alice").await;
    let cookie = session_for(&state, "alice").await;
    let db = state.db.clone();
    let app = common::app(state);

    let resp = app
        .serve(common::post_form(
            &format!("/tokens/{token_id}/limits"),
            &cookie,
            "dimension=tokens&window=day&value=500",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rules = limits::applicable_for_token(&db, &token_id).await.unwrap();
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0].value, 500.0);
    assert_eq!(rules[0].managed_by, ManagedBy::Owner);

    let resp = app
        .serve(common::post_form(
            &format!("/tokens/{token_id}/limits/delete"),
            &cookie,
            &format!("id={}", rules[0].id),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        limits::applicable_for_token(&db, &token_id)
            .await
            .unwrap()
            .is_empty()
    );
}

/// A non-finite quota parses, survives `value.max(0.0)`, and stores a limit
/// that can never be reached — and renders as `inf`.
#[tokio::test]
async fn a_non_finite_quota_is_rejected() {
    let upstream = MockServer::start().await;
    let state = common::state_with_chat_pool(&upstream.uri()).await;
    let (_, token_id) = common::seed_user_with_token_id(&state, "alice").await;
    let cookie = session_for(&state, "alice").await;
    let db = state.db.clone();
    let app = common::app(state);

    for value in ["inf", "NaN", "-1"] {
        let resp = app
            .serve(common::post_form(
                &format!("/tokens/{token_id}/limits"),
                &cookie,
                &format!("dimension=requests&window=day&value={value}"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "SSE toast");
        assert!(
            limits::applicable_for_token(&db, &token_id)
                .await
                .unwrap()
                .is_empty(),
            "`{value}` must not be stored as a quota"
        );
    }
}

/// The operator's list and the owner's are separate, and routing enforces the
/// intersection — the asymmetry this closes is that an admin could cap a
/// token's spend but had no say at all over its reach.
#[tokio::test]
async fn an_operator_restriction_narrows_a_token_the_owner_cannot_widen() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream).await;
    let state = state_with_two_models(&upstream).await;
    let (bearer, token_id) = common::seed_user_with_token_id(&state, "alice").await;

    // The operator allows only model-a.
    token_models::set_for_token(&state.db, &token_id, &["model-a".into()], ManagedBy::Admin)
        .await
        .unwrap();
    // The owner tries to grant themselves both.
    token_models::set_for_token(
        &state.db,
        &token_id,
        &["model-a".into(), "model-b".into()],
        ManagedBy::Owner,
    )
    .await
    .unwrap();

    let app = common::app(state);
    let resp = app.serve(chat_req(&bearer, "model-a")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "both lists allow model-a");

    let resp = app.serve(chat_req(&bearer, "model-b")).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "the owner cannot grant past the operator's list"
    );

    // And the listing agrees.
    let resp = app.serve(models_req(&bearer)).await.unwrap();
    let body = common::read_body(resp).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let ids: Vec<&str> = parsed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["model-a"], "{parsed}");
}

/// Clearing the owner's list must not clear the operator's.
#[tokio::test]
async fn an_owner_clearing_their_list_does_not_lift_the_operator_restriction() {
    let upstream = MockServer::start().await;
    mount_chat(&upstream).await;
    let state = state_with_two_models(&upstream).await;
    let (bearer, token_id) = common::seed_user_with_token_id(&state, "alice").await;
    token_models::set_for_token(&state.db, &token_id, &["model-a".into()], ManagedBy::Admin)
        .await
        .unwrap();
    let cookie = session_for(&state, "alice").await;
    let app = common::app(state);

    // Owner turns their own restriction off entirely.
    let resp = app
        .serve(models_post(
            &cookie,
            &token_id,
            "models=model-a&models=model-b",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app.serve(chat_req(&bearer, "model-b")).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "the operator's restriction survives the owner clearing theirs"
    );
}
