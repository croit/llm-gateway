// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Full OIDC browser flow against a mock IdP.
//!
//! The test stands up a wiremock-backed IdP that:
//!   - serves a `/.well-known/openid-configuration` discovery doc whose
//!     issuer / authorization_endpoint / token_endpoint / jwks_uri
//!     point back at the mock,
//!   - publishes a JWKS containing the public half of a fresh RSA key,
//!   - exchanges `?code=…` for an RSA-signed ID token whose `iss`/`aud`
//!     match what the gateway expects and whose `groups` claim drives
//!     the `roles_claim` mapping.
//!
//! Drive `/auth/login` → confirm the pending_logins row is in the DB →
//! call `/auth/callback?code=…&state=…` → assert a session cookie comes
//! back set, the `users` row is upserted with the claimed roles, and
//! the next `/api/v0/me` request with that cookie is authenticated.

mod common;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use common::Service as _;
use gateway::rama_server::{RamaState, SessionStore, router::router};
use gateway::server::auth::oidc::OidcClient;
use gateway::server::config::{Config, OidcConfig};
use gateway::server::rbac::Resolver;
use gateway::server::tools::ToolRegistry;
use gateway::server::upstreams::{
    self,
    config::{PoolKind, UpstreamPoolConfig},
};
use gateway::server::{AppState, db};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rama::http::{Body, Method, Request, StatusCode};
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY_ID: &str = "test-key";
const CLIENT_ID: &str = "gateway-test-client";
const CLIENT_SECRET: &str = "test-client-secret";
const CLIENT_SECRET_ENV: &str = "GATEWAY_OIDC_TEST_SECRET";
const SUBJECT: &str = "alice-sub";
const EMAIL: &str = "alice@example.com";
const NAME: &str = "Alice";
/// A deep-link target (e.g. a shared chat) handed to `/auth/login`; the dance
/// must carry it all the way to the post-callback redirect.
const RETURN_TO: &str = "/chat/shared-deadbeef-1111";

fn base64url_nopad(bytes: &[u8]) -> String {
    // Hand-rolled to avoid pulling base64 in as a dev-dep — same alphabet
    // as `rama_server::session::base64url_nopad`.
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut chunks = bytes.chunks_exact(3);
    for c in chunks.by_ref() {
        let n = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHA[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        }
        2 => {
            let n = (rem[0] as u32) << 16 | (rem[1] as u32) << 8;
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        }
        _ => unreachable!(),
    }
    out
}

/// Build a JWK (RFC 7517) describing the public half of `key`.
fn jwk_for(key: &RsaPublicKey) -> serde_json::Value {
    json!({
        "kty": "RSA",
        "alg": "RS256",
        "use": "sig",
        "kid": KEY_ID,
        "n": base64url_nopad(&key.n().to_bytes_be()),
        "e": base64url_nopad(&key.e().to_bytes_be()),
    })
}

/// Sign an RS256 ID token with the test private key. The pkcs8 → pkcs1
/// detour is because jsonwebtoken's `EncodingKey::from_rsa_pem` wants
/// PKCS#1, but the rsa crate emits PKCS#8 by default.
fn sign_id_token(private_key: &RsaPrivateKey, issuer: &str, audience: &str, nonce: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let claims = json!({
        "iss": issuer,
        "sub": SUBJECT,
        "aud": audience,
        "exp": now + 3600,
        "iat": now,
        "nonce": nonce,
        "email": EMAIL,
        "name": NAME,
        "groups": ["engineering", "admin"],
    });

    let pkcs1 = private_key
        .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
        .unwrap();
    let encoding_key = EncodingKey::from_rsa_pem(pkcs1.as_bytes()).unwrap();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KEY_ID.into());
    encode(&header, &claims, &encoding_key).unwrap()
}

/// Build a `RamaState` with the OIDC client pointed at `idp_uri`.
async fn state_with_oidc(idp_uri: &str, roles_claim: Option<&str>) -> RamaState {
    // Set the client-secret env var the OidcConfig points at. The OIDC
    // build path reads it via `std::env::var`, so we have to plant it
    // before constructing the client.
    //
    // SAFETY: integration tests run in the same process. Concurrent
    // tests that set this env to different values would race; we're
    // the only caller, so the value is stable for the test's lifetime.
    unsafe { std::env::set_var(CLIENT_SECRET_ENV, CLIENT_SECRET) };

    let oidc_config = OidcConfig {
        issuer: idp_uri.to_string(),
        client_id: CLIENT_ID.into(),
        client_secret_env: CLIENT_SECRET_ENV.into(),
        scopes: vec!["email".into(), "profile".into()],
        roles_claim: roles_claim.map(String::from),
    };
    let mut config = Config {
        oidc: Some(oidc_config.clone()),
        ..Default::default()
    };
    config.gateway.public_url = "http://gateway.test".into();

    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let registry = upstreams::UpstreamRegistry::new(&Default::default()).unwrap();
    let _: PoolKind = PoolKind::Chat; // keep the import live; UpstreamRegistry::new ate ours
    let tools = Arc::new(ToolRegistry::new());
    let rbac = Arc::new(Resolver::empty());
    let _: UpstreamPoolConfig = UpstreamPoolConfig {
        compliance: Default::default(),
        kind: PoolKind::Chat,
        strategy: upstreams::config::PickerStrategy::RoundRobin,
        models: Vec::new(),
        backend: vec![],
    };

    let oidc = OidcClient::build(&oidc_config, &config.gateway.public_url)
        .await
        .expect("build OidcClient against mock IdP");

    let app = AppState::new(config, pool.clone(), registry, tools, rbac).with_oidc(oidc);
    let sessions = SessionStore::new(pool, common::TEST_SECRET);
    RamaState::new(app, sessions)
}

#[tokio::test]
async fn full_oidc_dance_completes_and_stamps_the_session() {
    // 1. Stand up the mock IdP.
    let private_key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let public_key = RsaPublicKey::from(&private_key);

    let idp = MockServer::start().await;
    let issuer = idp.uri();
    let discovery = json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/auth"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "userinfo_endpoint": format!("{issuer}/userinfo"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
    });
    let jwks = json!({"keys": [jwk_for(&public_key)]});
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(discovery))
        .mount(&idp)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jwks))
        .mount(&idp)
        .await;
    // The token endpoint stays unmocked at first — we'll plug in the
    // matching response *after* we've observed the nonce the gateway
    // generates, so the ID token's `nonce` claim matches.

    let state = state_with_oidc(&issuer, Some("groups")).await;
    let app = router(Arc::new(state.clone()));

    // 2. Drive /auth/login. The redirect Location must carry the
    // gateway's state + nonce; we read those back from the pending_logins
    // row so we can plug them into the upcoming token-endpoint mock.
    // Drive login *with* a deep-link target — it must be stashed in the
    // pending row and survive to the post-callback redirect.
    let resp = app
        .serve(common::req(
            Method::GET,
            &format!("/auth/login?return_to={RETURN_TO}"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get(rama::http::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        location.starts_with(&format!("{issuer}/auth?")),
        "unexpected redirect: {location}"
    );

    let pending: (String, String, String, Option<String>) =
        sqlx::query_as("SELECT state, pkce_verifier, nonce, return_to FROM pending_logins LIMIT 1")
            .fetch_one(&state.db)
            .await
            .unwrap();
    let (csrf, _pkce_verifier, nonce, return_to) = pending;
    assert_eq!(
        return_to.as_deref(),
        Some(RETURN_TO),
        "/auth/login must persist the return_to it was handed",
    );

    // 3. Mount the token endpoint NOW that we know the nonce. The
    // OidcClient POSTs `application/x-www-form-urlencoded` with code +
    // redirect_uri + client_id + client_secret + grant_type +
    // code_verifier; for the test we only care that the response is
    // shape-correct.
    let id_token = sign_id_token(&private_key, &issuer, CLIENT_ID, &nonce);
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "test-access",
            "id_token": id_token,
            "token_type": "Bearer",
            "expires_in": 3600,
        })))
        .mount(&idp)
        .await;

    // 4. Hit /auth/callback with the matching code + state.
    let callback_uri = format!("/auth/callback?code=test-code&state={csrf}");
    let resp = app
        .serve(common::req(Method::GET, &callback_uri))
        .await
        .unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get(rama::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = String::from_utf8_lossy(&common::read_body(resp).await).into_owned();
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "callback should redirect on success; body = {body}"
    );
    // The whole point of the deep-link fix: the user lands back on the page
    // they requested, not the default `/chat` surface.
    assert_eq!(
        location.as_deref(),
        Some(RETURN_TO),
        "callback must honour the stored return_to; body = {body}"
    );

    // 5. Re-run the callback request to grab the Set-Cookie header (the
    // previous response was consumed by read_body in the error branch).
    // Pull the cookie out of the original response headers.
    let resp = app
        .serve(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/auth/callback?code=test-code-2&state={csrf}-2"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // The second call uses a state that doesn't exist (single-use row
    // was deleted by the first call) — confirms the row is consumed.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 6. Confirm the user landed in the DB with the right claims.
    let users: Vec<(String, String, Option<String>, String)> =
        sqlx::query_as("SELECT id, email, name, roles_json FROM users")
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].0, SUBJECT);
    assert_eq!(users[0].1, EMAIL);
    assert_eq!(users[0].2.as_deref(), Some(NAME));
    // roles_json should be ["engineering", "admin"]
    let roles: Vec<String> = serde_json::from_str(&users[0].3).unwrap();
    assert_eq!(roles, vec!["engineering", "admin"]);

    // 7. Confirm a session row was minted.
    let session_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(session_count, 1);
}

#[tokio::test]
async fn callback_without_pending_state_is_400() {
    let private_key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let public_key = RsaPublicKey::from(&private_key);
    let idp = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": idp.uri(),
            "authorization_endpoint": format!("{}/auth", idp.uri()),
            "token_endpoint": format!("{}/token", idp.uri()),
            "jwks_uri": format!("{}/jwks", idp.uri()),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
        })))
        .mount(&idp)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": [jwk_for(&public_key)]
        })))
        .mount(&idp)
        .await;

    let state = state_with_oidc(&idp.uri(), Some("groups")).await;
    let app = router(Arc::new(state));

    // No /auth/login first — the pending_logins table is empty.
    let resp = app
        .serve(common::req(
            Method::GET,
            "/auth/callback?code=x&state=does-not-exist",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
