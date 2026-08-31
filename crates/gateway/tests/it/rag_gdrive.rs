// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Connecting a RAG collection to Google Drive.
//!
//! Drive is the first source that cannot be authorised by a credential an
//! operator types, so what is pinned here is the consent flow's contract
//! rather than the Drive API itself (that is unit-tested in the provider):
//!
//!   * an admin is sent to Google with PKCE and the read-only scope,
//!   * the `state` is single-use, so a replayed callback cannot mint a
//!     second token,
//!   * a non-admin cannot start a flow,
//!   * a collection stays saveable *before* consent, which is the ordering
//!     the whole flow depends on — the client id has to be stored before
//!     there is anything to consent with.

use crate::common;

use common::Service as _;
use gateway_core::server::db::{rag as rag_db, rag_oauth as oauth_db};
use jiff::Timestamp;
use rama::http::{Body, Method, Request, StatusCode};

fn req_with_cookie(method: Method, uri: &str, cookie: &str, body: Option<&str>) -> Request {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("id={cookie}"));
    if body.is_some() {
        b = b.header("content-type", "application/x-www-form-urlencoded");
    }
    b.body(Body::from(body.unwrap_or("").to_string())).unwrap()
}

async fn seed_user(
    state: &gateway::rama_server::RamaState,
    user_id: &str,
    roles: Vec<String>,
) -> String {
    use gateway_core::server::db::users;
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: user_id.into(),
            email: format!("{user_id}@example.com"),
            name: None,
            roles,
            created_at: now,
            updated_at: now,
            timezone: None,
            speech_voice: None,
        },
    )
    .await
    .unwrap();
    let session = state.sessions.create(user_id).await.unwrap();
    state.sessions.sign(&session.id)
}

/// A Drive collection whose OAuth client is stored but which nobody has
/// consented to yet — the state an operator is in between "Save" and
/// "Connect".
async fn drive_collection(state: &gateway::rama_server::RamaState) -> i64 {
    let secrets = serde_json::to_string(&serde_json::json!({"client_secret": "s3cret"})).unwrap();
    let sealed = state.crypto.seal_str(&secrets).unwrap();
    let c = rag_db::create_collection(
        &state.db,
        &rag_db::NewCollection {
            name: "drive".into(),
            description: None,
            git_url: String::new(),
            git_ref: "main".into(),
            pat: None,
            source: rag_db::SourceSpec {
                kind: "gdrive".into(),
                config: [(
                    "client_id".to_string(),
                    "cid.apps.googleusercontent.com".to_string(),
                )]
                .into_iter()
                .collect(),
                secrets: Some(sealed),
            },
            profile_id: None,
            extraction_model: None,
            embedding_model: "embed".into(),
            include_globs: vec![],
            exclude_globs: vec![],
            chunk_size: 800,
            chunk_overlap: 100,
            search_mode: rag_db::SearchMode::Versioned,
        },
    )
    .await
    .unwrap();
    c.id
}

#[tokio::test]
async fn drive_is_offered_as_a_source_kind() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let registry = state.provider_registry();
    let f = registry
        .get("gdrive")
        .expect("Google Drive is a registered provider");
    assert_eq!(f.label(), "Google Drive");
    let keys: Vec<&str> = f.config_fields().iter().map(|c| c.key).collect();
    assert!(keys.contains(&"client_id"));
    assert!(
        !keys.contains(&"refresh_token"),
        "the refresh token is minted by consent, never typed into the form"
    );
}

#[tokio::test]
async fn connect_sends_an_admin_to_google_with_pkce_and_read_only_scope() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_user(&state, "alice", vec!["admin".into()]).await;
    let id = drive_collection(&state).await;
    let db = state.db.clone();
    let app = common::app(state);

    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            &format!("/rag/{id}/connect"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get("location")
        .expect("the browser is sent somewhere")
        .to_str()
        .unwrap()
        .to_string();

    assert!(
        location.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"),
        "the operator lands on Google's consent screen: {location}"
    );
    let url = reqwest::Url::parse(&location).unwrap();
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        q.get("client_id").map(String::as_str),
        Some("cid.apps.googleusercontent.com")
    );
    assert_eq!(
        q.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert!(q.contains_key("code_challenge"), "PKCE is not optional");
    assert_eq!(
        q.get("scope").map(String::as_str),
        Some("https://www.googleapis.com/auth/drive.readonly"),
        "read-only: an indexer never needs to write to the customer's Drive"
    );
    assert_eq!(
        q.get("access_type").map(String::as_str),
        Some("offline"),
        "without offline access Google issues no refresh token and unattended \
         indexing would stop after an hour"
    );
    assert!(
        !q.contains_key("resource"),
        "RFC 8707 audience binding is an MCP concern; Google has no audience \
         to name and may reject the parameter"
    );

    // The flow is recorded so the callback can finish it.
    let st = q.get("state").expect("a CSRF state was issued");
    let pending = oauth_db::take_pending(&db, st).await.unwrap().unwrap();
    assert_eq!(pending.collection_id, id);
    assert_eq!(pending.source_kind, "gdrive");
    assert_eq!(pending.admin_user_id, "alice");
}

#[tokio::test]
async fn a_non_admin_cannot_start_a_consent_flow() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_user(&state, "bob", vec![]).await;
    let id = drive_collection(&state).await;
    let app = common::app(state);

    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            &format!("/rag/{id}/connect"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "a non-admin must not be handed a consent URL for a shared corpus"
    );
}

#[tokio::test]
async fn anonymous_connect_redirects_to_login_rather_than_starting_a_flow() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let id = drive_collection(&state).await;
    let app = common::app(state);

    let resp = app
        .serve(common::req(Method::GET, &format!("/rag/{id}/connect")))
        .await
        .unwrap();
    let location = resp
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    assert!(
        !location.contains("accounts.google.com"),
        "an anonymous request must never be handed a consent URL: {location}"
    );
}

/// The `state` is consumed on read. A callback replayed with the same code —
/// from a back button, a shared link, or a captured redirect — must not be
/// able to complete a second time.
#[tokio::test]
async fn a_replayed_callback_is_refused() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_user(&state, "alice", vec!["admin".into()]).await;
    let id = drive_collection(&state).await;
    let db = state.db.clone();
    let app = common::app(state);

    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            &format!("/rag/{id}/connect"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    let url = reqwest::Url::parse(location).unwrap();
    let st = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.to_string())
        .unwrap();

    // First use consumes it (standing in for the real callback, which would
    // go on to talk to Google).
    assert!(oauth_db::take_pending(&db, &st).await.unwrap().is_some());

    // The replay reaches the handler and finds nothing to finish.
    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            &format!("/rag/oauth/callback?code=abc&state={st}"),
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "a replayed state must not complete a second authorization"
    );
}

#[tokio::test]
async fn a_callback_carrying_a_provider_error_does_not_store_anything() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_user(&state, "alice", vec!["admin".into()]).await;
    let id = drive_collection(&state).await;
    let db = state.db.clone();
    // The state's own key, so the blob genuinely opens — decrypting with the
    // wrong one would make this assertion pass no matter what was stored.
    let crypto = state.crypto.clone();
    let app = common::app(state);

    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            "/rag/oauth/callback?error=access_denied",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::SEE_OTHER);

    let c = rag_db::find_collection_by_id(&db, id)
        .await
        .unwrap()
        .unwrap();
    let sealed = c
        .source
        .secrets
        .as_ref()
        .expect("the client secret is stored");
    let plain = crypto
        .open_str(&sealed.nonce, &sealed.ciphertext)
        .expect("the stored secrets open with the gateway's key");
    assert!(
        plain.contains("client_secret"),
        "sanity: this is really the secrets blob, so the check below can fail"
    );
    assert!(
        !plain.contains("refresh_token"),
        "a refused consent must leave no credential behind"
    );
}

/// The ordering the whole flow rests on: a Drive collection has to be
/// storable before anyone can consent, because consent needs the stored
/// client id. If saving demanded a working provider, the two would deadlock.
#[tokio::test]
async fn a_drive_collection_saves_before_it_is_connected() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let db = state.db.clone();
    let id = drive_collection(&state).await;

    let c = rag_db::find_collection_by_id(&db, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.source.kind, "gdrive");
    assert!(
        state
            .provider_registry()
            .get("gdrive")
            .unwrap()
            .build(
                &gateway_features::server::rag::source::ProviderConfig::new(
                    c.source.config.clone(),
                    [("client_secret".to_string(), "s3cret".to_string())]
                        .into_iter()
                        .collect(),
                ),
                reqwest::Client::new(),
            )
            .is_err(),
        "...and it is still not usable until consent has happened"
    );
}

/// Creating a Drive collection over the JSON API has to work, for the same
/// reason it works in the form: the client credentials must be storable
/// before anyone can consent with them. `build_source` used to run an
/// unconditional dry-run build, so this always 400'd with "use Connect to
/// grant access" — advice a JSON client cannot act on.
#[tokio::test]
async fn the_api_can_create_a_drive_collection_before_consent() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_user(&state, "alice", vec!["admin".into()]).await;
    let app = common::app(state);

    let body = r#"{
        "name": "drive-api",
        "embedding_model": "embed",
        "source_kind": "gdrive",
        "source_config": {
            "client_id": "cid.apps.googleusercontent.com",
            "client_secret": "s3cret",
            "root_folder_id": "root"
        }
    }"#;
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/rag/collections",
            &cookie,
            Some(body),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let raw = String::from_utf8_lossy(&common::read_body(resp).await).to_string();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a not-yet-connected Drive source is storable; body={raw}"
    );
    let resp_body = raw;
    let created: serde_json::Value = serde_json::from_str(&resp_body).unwrap();
    assert_eq!(created["source_kind"], "gdrive");
}

/// The refresh token is a secret no API caller ever sends. Rebuilding the
/// sealed blob from the request alone would drop it, so editing an unrelated
/// field would silently disconnect the corpus.
#[tokio::test]
async fn patching_an_unrelated_field_keeps_the_stored_refresh_token() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_user(&state, "alice", vec!["admin".into()]).await;
    let db = state.db.clone();
    let crypto = state.crypto.clone();

    // A collection that has already been through consent.
    let secrets = serde_json::to_string(
        &serde_json::json!({"client_secret": "s3cret", "refresh_token": "rt-abc"}),
    )
    .unwrap();
    let sealed = state.crypto.seal_str(&secrets).unwrap();
    let c = rag_db::create_collection(
        &db,
        &rag_db::NewCollection {
            name: "connected".into(),
            description: None,
            git_url: String::new(),
            git_ref: "main".into(),
            pat: None,
            source: rag_db::SourceSpec {
                kind: "gdrive".into(),
                config: [
                    ("client_id".to_string(), "cid".to_string()),
                    ("root_folder_id".to_string(), "root".to_string()),
                ]
                .into_iter()
                .collect(),
                secrets: Some(sealed),
            },
            profile_id: None,
            extraction_model: None,
            embedding_model: "embed".into(),
            include_globs: vec![],
            exclude_globs: vec![],
            chunk_size: 800,
            chunk_overlap: 100,
            search_mode: rag_db::SearchMode::Versioned,
        },
    )
    .await
    .unwrap();
    let app = common::app(state);

    // Re-point it at a different folder, sending no secrets at all.
    let patch = r#"{
        "source_kind": "gdrive",
        "source_config": { "client_id": "cid", "root_folder_id": "OtherFolder" }
    }"#;
    let resp = app
        .serve(req_with_cookie(
            Method::PATCH,
            &format!("/api/v0/rag/collections/{}", c.id),
            &cookie,
            Some(patch),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let after = rag_db::find_collection_by_id(&db, c.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after
            .source
            .config
            .get("root_folder_id")
            .map(String::as_str),
        Some("OtherFolder"),
        "the edit landed"
    );
    let kept = after.source.open_secrets(&crypto);
    assert_eq!(
        kept.get("refresh_token").map(String::as_str),
        Some("rt-abc"),
        "editing a folder id must not disconnect the collection"
    );
    assert_eq!(
        kept.get("client_secret").map(String::as_str),
        Some("s3cret"),
        "a secret the caller did not resend keeps its stored value"
    );
}

/// A client with no compiled-in knowledge of a provider has to be able to
/// discover that it needs a browser consent, not just a set of fields.
#[tokio::test]
async fn the_providers_endpoint_says_which_sources_need_consent() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_user(&state, "alice", vec!["admin".into()]).await;
    let app = common::app(state);

    let resp = app
        .serve(req_with_cookie(
            Method::GET,
            "/api/v0/rag/providers",
            &cookie,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(&common::read_body(resp).await).unwrap();
    let providers = body["data"].as_array().expect("a provider list");

    let drive = providers
        .iter()
        .find(|p| p["kind"] == "gdrive")
        .expect("Drive is published");
    assert_eq!(drive["auth"]["kind"], "oauth2");
    assert!(
        drive["auth"]["scopes"]
            .as_array()
            .is_some_and(|s| !s.is_empty()),
        "the scopes the consent will ask for are visible up front"
    );

    let webdav = providers
        .iter()
        .find(|p| p["kind"] == "webdav")
        .expect("WebDAV is published");
    assert_eq!(
        webdav["auth"]["kind"], "fields",
        "a typed-credential source is not confused for a consent one"
    );
}

/// A non-git source must not have to send a git URL.
///
/// `git_url` was a required JSON field even though it is only validated for
/// `source_kind: "git"`, so every Drive or WebDAV caller had to send
/// `"git_url": ""` to get past the parser.
#[tokio::test]
async fn a_remote_collection_needs_no_git_url() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_user(&state, "alice", vec!["admin".into()]).await;
    let app = common::app(state);

    let body = r#"{
        "name": "no-git-url",
        "embedding_model": "embed",
        "source_kind": "gdrive",
        "source_config": { "client_id": "cid", "client_secret": "sec" }
    }"#;
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/rag/collections",
            &cookie,
            Some(body),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // A git collection still has to name a repository.
    let body = r#"{ "name": "needs-url", "embedding_model": "embed" }"#;
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/api/v0/rag/collections",
            &cookie,
            Some(body),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
