// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The deployment setup wizard, end to end against a mock IdP.
//!
//! Three things are worth pinning here, in ascending order of how expensive
//! they would be to get wrong:
//!
//! 1. **First-run routing** — an unconfigured gateway must send every page to
//!    `/setup`, including `/login`, which otherwise shows a sign-in button that
//!    can only fail.
//! 2. **Recovery is not first run** — when `restore-setup` reopens the wizard
//!    on a live gateway, the gateway must keep serving. If reopening setup ever
//!    starts redirecting real users to a wizard, one locked-out admin takes the
//!    whole deployment down while asking for help.
//! 3. **The wizard actually configures the gateway** — the full round trip:
//!    provider form → real authorization-code login → pick an admin group →
//!    finish, leaving a live OIDC client, a mapped admin group, and no restart
//!    required.

use crate::common;
use crate::oidc_integration::{EMAIL, SUBJECT, jwk_for, sign_id_token};

use std::sync::Arc;

use common::Service as _;
use gateway::rama_server::{RamaState, SessionStore, router::service};
use gateway_core::server::config::Config;
use gateway_core::server::db::{self, app_settings, gateway_groups, users};
use gateway_core::server::oidc_settings;
use gateway_core::server::rbac::Resolver;
use gateway_core::server::setup;
use gateway_core::server::upstreams;
use gateway_runtime::server::AppState;
use gateway_runtime::server::state::RuntimeSettings;
use gateway_runtime::server::tools::ToolRegistry;
use jiff::{SignedDuration, Timestamp};
use rama::http::{Body, Method, Request, StatusCode};
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CLIENT_ID: &str = "gateway-setup-client";
const CLIENT_SECRET: &str = "setup-client-secret";
const PUBLIC_URL: &str = "http://gateway.test";

/// A gateway that has never been configured: no OIDC, no completed setup.
async fn unconfigured_state() -> RamaState {
    let mut config = Config::default();
    config.gateway.public_url_import_only = PUBLIC_URL.into();
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let registry = upstreams::UpstreamRegistry::new(&Default::default()).unwrap();
    let app = AppState::new(
        config,
        pool.clone(),
        registry,
        Arc::new(ToolRegistry::new()),
        Arc::new(Resolver::empty()),
    );
    app.set_runtime(RuntimeSettings {
        public_url: PUBLIC_URL.into(),
        oidc: None,
        setup_completed: false,
    });
    let sessions = SessionStore::new(pool, common::TEST_SECRET);
    RamaState::new(
        app,
        sessions,
        gateway_core::server::usage::UsageHandle::disabled(),
    )
}

/// Flip the same state into "already configured and serving".
async fn mark_configured(state: &RamaState) {
    setup::mark_completed(&state.db).await.unwrap();
    state.set_runtime(RuntimeSettings {
        public_url: PUBLIC_URL.into(),
        oidc: None,
        setup_completed: true,
    });
}

fn post_form(uri: &str, body: &str, cookie: Option<&str>) -> Request {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded");
    if let Some(c) = cookie {
        b = b.header("cookie", c);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn location(resp: &rama::http::Response) -> String {
    resp.headers()
        .get(rama::http::header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

/// A plausible provider, for the tests that plant a draft directly rather than
/// walking screen 1.
fn test_params() -> gateway_core::server::auth::oidc::OidcParams {
    gateway_core::server::auth::oidc::OidcParams {
        issuer: "https://id.example.com".into(),
        client_id: CLIENT_ID.into(),
        client_secret: CLIENT_SECRET.into(),
        scopes: vec!["email".into()],
        roles_claim: Some("groups".into()),
    }
}

async fn body_text(resp: rama::http::Response) -> String {
    String::from_utf8_lossy(&common::read_body(resp).await).into_owned()
}

// ---------------------------------------------------------------------------
// 1. First-run routing

#[tokio::test]
async fn a_fresh_gateway_sends_every_page_to_the_wizard() {
    let state = unconfigured_state().await;
    let app = service(Arc::new(state));

    for path in [
        "/",
        "/tokens",
        "/chat",
        "/usage",
        // `/login` in particular: without this the first thing a new
        // deployment shows is a sign-in button that cannot work.
        "/login",
        // Routes nobody ever hand-checked. They pass because the gate is a
        // layer with an allowlist (`rama_server::first_run`) rather than a
        // check each handler has to remember — which is what let `/auth/login`
        // slip through and answer 500 on a fresh box.
        "/admin/users",
        "/webhooks",
        "/scheduled",
        "/integrations",
        "/memory",
        // Not a route at all. A 404 telling an operator nothing is a worse
        // answer than the wizard they are looking for.
        "/whatever-this-is",
    ] {
        let resp = app.serve(common::req(Method::GET, path)).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SEE_OTHER,
            "{path} should redirect on a fresh install"
        );
        assert_eq!(
            location(&resp),
            "/setup",
            "{path} redirected somewhere else"
        );
    }

    // The other side of the gate: the wizard cannot work if the callback its
    // own test login lands on is redirected away. It answers for itself (here:
    // a 400, since this request carries no in-flight login) — anything but a
    // 303 to `/setup`.
    let resp = app
        .serve(common::req(Method::GET, "/auth/callback"))
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "the probe callback must reach its handler, or setup can never finish"
    );

    // Static chrome keeps serving, or the wizard renders unstyled.
    let resp = app
        .serve(common::req(Method::GET, "/assets/app.css"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn an_unconfigured_gateway_reports_itself_not_ready() {
    // `/healthz` is liveness, `/readyz` is readiness — and a gateway that
    // cannot serve one authenticated request is not ready. Reporting ready
    // here makes a load balancer send production traffic to a box the operator
    // is still setting up.
    let state = unconfigured_state().await;
    let app = service(Arc::new(state.clone()));

    let resp = app
        .serve(common::req(Method::GET, "/healthz"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "liveness is unaffected");

    let resp = app
        .serve(common::req(Method::GET, "/readyz"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    mark_configured(&state).await;
    let resp = app
        .serve(common::req(Method::GET, "/readyz"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "ready once setup is done");
}

#[tokio::test]
async fn auth_login_points_at_the_wizard_rather_than_erroring() {
    // The route a fresh deployment is most likely to be linked to. Without the
    // check it answers 500 "OIDC is not configured", which tells the operator
    // nothing about what to do next.
    let state = unconfigured_state().await;
    let app = service(Arc::new(state));

    let resp = app
        .serve(common::req(Method::GET, "/auth/login"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp), "/setup");
}

#[tokio::test]
async fn the_wizard_is_open_and_prefilled_on_a_first_run() {
    let state = unconfigured_state().await;
    let app = service(Arc::new(state));

    let req = Request::builder()
        .method(Method::GET)
        .uri("/setup")
        .header("host", "gw.example.com")
        .header("x-forwarded-proto", "https")
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "first run must be open");

    let html = body_text(resp).await;
    // The public URL is guessed from the request, and the redirect URI the
    // operator has to whitelist is shown built from it — the single most
    // commonly mis-typed value in an OIDC setup.
    assert!(html.contains("https://gw.example.com"), "{html}");
    assert!(
        html.contains("https://gw.example.com/auth/callback"),
        "the redirect URI to whitelist must be shown"
    );
    assert!(html.contains("name=\"issuer\""), "provider form missing");
}

#[tokio::test]
async fn a_configured_gateway_has_no_setup_page() {
    let state = unconfigured_state().await;
    mark_configured(&state).await;
    let app = service(Arc::new(state));

    let resp = app.serve(common::req(Method::GET, "/setup")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // And a POST cannot sneak past the GET gate.
    let resp = app
        .serve(post_form("/setup/finish", "pair=groups%1Fadmin", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// 2. Recovery must not take the gateway down

#[tokio::test]
async fn recovery_needs_the_one_time_token() {
    let state = unconfigured_state().await;
    mark_configured(&state).await;
    setup::open_recovery(&state.db, "correct-horse")
        .await
        .unwrap();
    let app = service(Arc::new(state));

    // No token at all.
    let resp = app.serve(common::req(Method::GET, "/setup")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Wrong token.
    let resp = app
        .serve(common::req(Method::GET, "/setup?claim=battery-staple"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Right token: in, and handed a cookie so the POSTs that follow work.
    let resp = app
        .serve(common::req(Method::GET, "/setup?claim=correct-horse"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = resp
        .headers()
        .get_all(rama::http::header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|v| v.starts_with("gw_setup="))
        .expect("recovery claim cookie");
    assert!(cookie.contains("Path=/setup"), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
}

#[tokio::test]
async fn an_open_recovery_window_does_not_disturb_anyone() {
    // THE regression this file exists for. Reopening setup on a live gateway
    // must not put it back into first-run mode: everyone else keeps working
    // while the locked-out admin fixes their access.
    let state = unconfigured_state().await;
    mark_configured(&state).await;
    setup::open_recovery(&state.db, "token").await.unwrap();
    let app = service(Arc::new(state.clone()));

    // A signed-in user's page still renders rather than bouncing to /setup.
    let cookie = common::seed_session(&state, "bob", "bob@example.com").await;
    let req = Request::builder()
        .method(Method::GET)
        .uri("/tokens")
        .header("cookie", format!("id={cookie}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an open recovery window must not interrupt signed-in users"
    );

    // And an anonymous visitor is still sent to sign in, not to the wizard.
    let resp = app.serve(common::req(Method::GET, "/")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert!(
        location(&resp).starts_with("/login"),
        "anonymous users belong at /login, not the wizard: {}",
        location(&resp)
    );
}

#[tokio::test]
async fn an_expired_recovery_window_closes_the_wizard_again() {
    let state = unconfigured_state().await;
    mark_configured(&state).await;
    setup::open_recovery(&state.db, "token").await.unwrap();
    // Wind the deadline back past now, as the passage of 30 minutes would.
    app_settings::set(
        &state.db,
        setup::RECOVERY_UNTIL_KEY,
        &(Timestamp::now() - SignedDuration::from_mins(1)).to_string(),
    )
    .await
    .unwrap();
    let app = service(Arc::new(state));

    let resp = app
        .serve(common::req(Method::GET, "/setup?claim=token"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "an expired window must not still accept its token"
    );
}

// ---------------------------------------------------------------------------
// 3. The whole wizard, against a real (mock) provider

/// Stand up a wiremock IdP that answers discovery + JWKS. The token endpoint
/// is mounted later, once the nonce the gateway generated is known — the ID
/// token it returns has to echo that nonce back.
///
/// Shared with `oidc_integration`, so the wizard's probe and the real sign-in
/// are tested against the same provider shape.
pub(crate) async fn mock_idp(public_key: &RsaPublicKey) -> MockServer {
    let idp = MockServer::start().await;
    let issuer = idp.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/auth"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
        })))
        .mount(&idp)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": [jwk_for(public_key)]
        })))
        .mount(&idp)
        .await;
    idp
}

#[tokio::test]
async fn the_wizard_configures_the_gateway_without_a_restart() {
    let private_key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let public_key = RsaPublicKey::from(&private_key);
    let idp = mock_idp(&public_key).await;
    let issuer = idp.uri();

    let state = unconfigured_state().await;
    let app = service(Arc::new(state.clone()));

    // --- Screen 1: enter the provider and start the test login. ------------
    let form = serde_urlencoded::to_string([
        ("public_url", PUBLIC_URL),
        ("issuer", issuer.as_str()),
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("scopes", "email profile groups"),
        ("roles_claim", "groups"),
    ])
    .unwrap();
    let resp = app
        .serve(post_form("/setup/test", &form, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert!(
        location(&resp).starts_with(&format!("{issuer}/auth?")),
        "should redirect to the provider: {}",
        location(&resp)
    );

    // The probe is recorded as a probe, not as a sign-in.
    let (csrf, nonce, purpose): (String, String, String) =
        sqlx::query_as("SELECT state, nonce, purpose FROM pending_logins LIMIT 1")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(
        purpose, "setup",
        "the callback tells a probe from a login by this column"
    );

    // The authorization request must use the PRODUCTION redirect URI, so the
    // operator whitelists exactly one URI and the test proves the real path.
    assert!(
        location(&resp).contains(&urlencoding_of(&format!("{PUBLIC_URL}/auth/callback"))),
        "probe must reuse the production redirect_uri: {}",
        location(&resp)
    );

    // --- The provider signs the operator in. -------------------------------
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

    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/auth/callback?code=test-code&state={csrf}"))
        .header("cookie", format!("gw_oidc={csrf}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp), "/setup");

    // A probe authorises nobody: no user row, no session cookie.
    assert!(
        users::find_by_id(&state.db, SUBJECT)
            .await
            .unwrap()
            .is_none(),
        "the setup probe must not create a user"
    );
    assert!(
        resp.headers().get(rama::http::header::SET_COOKIE).is_none(),
        "the setup probe must not mint a session"
    );

    // --- Screen 2: the operator's own claims are offered. ------------------
    let resp = app.serve(common::req(Method::GET, "/setup")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains(EMAIL), "should show who signed in: {html}");
    // Both group values from the token are offered as pickable pairs — the
    // whole reason the wizard insists on a real login first.
    assert!(html.contains("engineering"), "{html}");
    assert!(html.contains("admin"), "{html}");

    // --- Finish. -----------------------------------------------------------
    let finish = serde_urlencoded::to_string([("pair", "groups\u{1f}admin")]).unwrap();
    let resp = app
        .serve(post_form("/setup/finish", &finish, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    // Sign in, then continue to the settings the wizard deliberately did not
    // ask about — carried as an ordinary `return_to` so the OIDC round trip
    // delivers it, rather than as a second landing mechanism.
    assert_eq!(
        location(&resp),
        "/login?return_to=%2Fadmin%2Fsettings",
        "finishing lands on sign-in, then continues to settings"
    );

    // Live, with no restart: the running state now has an OIDC client and is
    // out of first-run mode.
    assert!(
        state.oidc().is_some(),
        "the OIDC client must be swapped in live"
    );
    assert!(state.setup_completed());
    assert_eq!(state.public_url(), PUBLIC_URL);

    // Persisted, so a restart agrees with the running process.
    assert!(setup::is_completed(&state.db).await.unwrap());
    let params = oidc_settings::params(&state.db, &state.crypto)
        .await
        .unwrap()
        .expect("provider settings stored");
    assert_eq!(params.issuer, issuer);
    assert_eq!(params.client_id, CLIENT_ID);
    assert_eq!(
        params.client_secret, CLIENT_SECRET,
        "the sealed secret must round-trip"
    );
    assert_eq!(
        params.roles_claim.as_deref(),
        Some("groups"),
        "the claim the admin value was picked from IS the roles claim"
    );

    // The admin group exists, is flagged admin, and maps the chosen value.
    let groups = gateway_groups::list_groups(&state.db).await.unwrap();
    let admins = groups
        .iter()
        .find(|g| g.name == "admins")
        .expect("admin group created");
    assert!(admins.is_admin);
    let mapped = gateway_groups::mapped_values_for_group(&state.db, "admins")
        .await
        .unwrap();
    assert_eq!(mapped, vec!["admin".to_string()]);

    // In-flight wizard state is gone — a client secret must not linger in a
    // settings row after the run that needed it.
    assert!(
        setup::load_draft(&state.db, &state.crypto)
            .await
            .unwrap()
            .is_none(),
        "the draft (which holds the client secret) must be cleared"
    );
    assert!(
        setup::load_proof(&state.db, &state.crypto)
            .await
            .unwrap()
            .is_none(),
        "the operator's claims must not be kept after setup"
    );

    // And the wizard is closed behind us.
    let resp = app.serve(common::req(Method::GET, "/setup")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn starting_a_new_probe_discards_the_previous_ones_claims() {
    // Two tabs, or a back button: tab 1 proves provider A, tab 2 then submits
    // provider B. If A's proof survived, `/setup` would show A's claims beside
    // B's draft, and `setup_finish` would persist provider B with an admin
    // value taken from a token A issued. Proof and draft move together.
    let state = unconfigured_state().await;
    setup::save_proof(
        &state.db,
        &state.crypto,
        &setup::Proof {
            subject: SUBJECT.into(),
            email: EMAIL.into(),
            name: None,
            claims: json!({"sub": SUBJECT, "groups": ["from-provider-a"]}),
        },
    )
    .await
    .unwrap();
    let app = service(Arc::new(state.clone()));

    // Provider B is unreachable, so the probe never gets far enough to replace
    // the proof itself — which is exactly the case that used to leave A's.
    let form = serde_urlencoded::to_string([
        ("public_url", PUBLIC_URL),
        ("issuer", "http://127.0.0.1:1"),
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("scopes", "email"),
        ("roles_claim", "groups"),
    ])
    .unwrap();
    let resp = app
        .serve(post_form("/setup/test", &form, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    assert!(
        setup::load_proof(&state.db, &state.crypto)
            .await
            .unwrap()
            .is_none(),
        "the previous provider's claims must not survive a new attempt"
    );
    // And the wizard is back on screen 1 rather than showing stale claims.
    let resp = app.serve(common::req(Method::GET, "/setup")).await.unwrap();
    let html = body_text(resp).await;
    assert!(!html.contains("from-provider-a"), "{html}");
    assert!(
        html.contains("name=\"issuer\""),
        "should be back on screen 1"
    );
}

#[tokio::test]
async fn a_probe_that_lands_after_setup_closed_is_discarded() {
    // A `pending_logins` row lives 15 minutes; a recovery window is 30 and can
    // be finished (or expire) inside that span. A probe arriving afterwards
    // must not still write a real person's ID-token claims into a settings row
    // on a gateway that is no longer accepting setup.
    let state = unconfigured_state().await;
    setup::save_draft(
        &state.db,
        &state.crypto,
        &setup::Draft {
            public_url: PUBLIC_URL.into(),
            params: test_params(),
        },
    )
    .await
    .unwrap();
    // Plant an in-flight probe, then close setup behind it.
    let start = gateway_core::server::auth::oidc::AuthorizationStart {
        url: "https://id.example.com/auth".into(),
        csrf: "csrf-value".into(),
        nonce: "nonce-value".into(),
        pkce_verifier: "verifier".into(),
    };
    gateway_core::server::auth::pending::insert(
        &state.db,
        &start,
        Some("/setup"),
        gateway_core::server::auth::pending::Purpose::Setup,
    )
    .await
    .unwrap();
    mark_configured(&state).await;

    let app = service(Arc::new(state.clone()));
    let req = Request::builder()
        .method(Method::GET)
        .uri("/auth/callback?code=c&state=csrf-value")
        .header("cookie", "gw_oidc=csrf-value")
        .body(Body::empty())
        .unwrap();
    let resp = app.serve(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        setup::load_proof(&state.db, &state.crypto)
            .await
            .unwrap()
            .is_none(),
        "a discarded probe must not record anyone's claims"
    );
}

#[tokio::test]
async fn finishing_without_picking_a_group_is_rejected() {
    let state = unconfigured_state().await;
    // Plant a draft + proof directly: this test is about the finish step's
    // validation, not about repeating the whole round trip.
    setup::save_draft(
        &state.db,
        &state.crypto,
        &setup::Draft {
            public_url: PUBLIC_URL.into(),
            params: test_params(),
        },
    )
    .await
    .unwrap();
    setup::save_proof(
        &state.db,
        &state.crypto,
        &setup::Proof {
            subject: SUBJECT.into(),
            email: EMAIL.into(),
            name: None,
            claims: json!({"sub": SUBJECT, "groups": ["engineering"]}),
        },
    )
    .await
    .unwrap();
    let app = service(Arc::new(state.clone()));

    let resp = app
        .serve(post_form("/setup/finish", "", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        !setup::is_completed(&state.db).await.unwrap(),
        "a rejected finish must leave the gateway unconfigured"
    );
}

#[tokio::test]
async fn an_unreachable_provider_does_not_lose_what_was_typed() {
    let state = unconfigured_state().await;
    let app = service(Arc::new(state.clone()));

    // Port 1 on loopback: nothing listens, so discovery fails fast.
    let form = serde_urlencoded::to_string([
        ("public_url", PUBLIC_URL),
        ("issuer", "http://127.0.0.1:1"),
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("scopes", "email"),
        ("roles_claim", "groups"),
    ])
    .unwrap();
    let resp = app
        .serve(post_form("/setup/test", &form, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    // Coming back, the form is filled in again — retyping a client secret
    // because a URL had a typo is exactly the friction this wizard exists to
    // remove.
    let resp = app.serve(common::req(Method::GET, "/setup")).await.unwrap();
    let html = body_text(resp).await;
    assert!(html.contains("http://127.0.0.1:1"), "{html}");
    assert!(
        !setup::is_completed(&state.db).await.unwrap(),
        "a failed test must not configure anything"
    );
}

/// Percent-encode the way `Url::query_pairs_mut` does, so the assertion above
/// compares like with like.
fn urlencoding_of(value: &str) -> String {
    serde_urlencoded::to_string([("v", value)])
        .unwrap()
        .trim_start_matches("v=")
        .to_string()
}
