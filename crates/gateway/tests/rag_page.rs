// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/rag` admin page — integration tests for the plait-rendered surface.
//!
//! Auth model: admin-gated. Anonymous → 302 redirect to /auth/login;
//! signed-in non-admin → 403; admin → 200 with the table.

mod common;

use common::Service as _;
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

/// Seed a session for `user_id` with the given `roles` (the RBAC
/// resolver in `state_with_admin_rbac` maps OIDC value `"admin"` →
/// internal role `"admin"`, so an admin user needs `roles: ["admin"]`).
async fn seed_session_with_roles(
    state: &gateway::rama_server::RamaState,
    user_id: &str,
    email: &str,
    roles: Vec<String>,
) -> String {
    use gateway::server::db::users;
    let now = Timestamp::now();
    users::upsert(
        &state.db,
        &users::User {
            id: user_id.into(),
            email: email.into(),
            name: None,
            roles,
            created_at: now,
            updated_at: now,
            timezone: None,
        },
    )
    .await
    .unwrap();
    let session = state.sessions.create(user_id).await.unwrap();
    state.sessions.sign(&session.id)
}

#[tokio::test]
async fn anonymous_get_redirects_to_login() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let app = common::app(state);
    let resp = app.serve(common::req(Method::GET, "/rag")).await.unwrap();
    // require_admin_or_403 calls require_session_or_redirect first, which
    // returns a 303 to /auth/login for anonymous callers.
    assert!(
        resp.status() == StatusCode::SEE_OTHER || resp.status() == StatusCode::FOUND,
        "got {}",
        resp.status()
    );
}

#[tokio::test]
async fn non_admin_get_is_forbidden() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_session_with_roles(&state, "u1", "u1@example.com", vec![]).await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/rag", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_get_renders_page_with_create_form() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie =
        seed_session_with_roles(&state, "boss", "boss@example.com", vec!["admin".into()]).await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/rag", &cookie, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("RAG collections"), "page title missing");
    assert!(body.contains("rag-create-form"), "create form missing");
    assert!(body.contains("No collections yet."), "empty state missing");
}

#[tokio::test]
async fn admin_post_creates_row_and_sse_patches_list() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie =
        seed_session_with_roles(&state, "boss", "boss@example.com", vec!["admin".into()]).await;
    let app = common::app(state);

    let form = "name=demo-repo\
        &git_url=https://example.invalid/demo.git\
        &git_ref=main\
        &embedding_model=embed-1\
        &include_globs=*.rs\
        &exclude_globs=target/\
        &chunk_size=800\
        &chunk_overlap=100";
    let resp = app
        .serve(req_with_cookie(Method::POST, "/rag", &cookie, Some(form)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    // datastar SSE: lines starting with `data: ` containing the
    // appended row html and a "Queued indexing" toast.
    assert!(
        body.contains("rag-row-"),
        "patched row missing\nbody = {body}"
    );
    assert!(body.contains("demo-repo"), "row label missing");
    assert!(body.contains("was queued"), "toast missing");

    // The next GET should now show the row instead of the empty state.
    let resp = app
        .serve(req_with_cookie(Method::GET, "/rag", &cookie, None))
        .await
        .unwrap();
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(!body.contains("No collections yet."));
    assert!(body.contains("demo-repo"));
}

#[tokio::test]
async fn create_rejects_invalid_chunk_overlap() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie =
        seed_session_with_roles(&state, "boss", "boss@example.com", vec!["admin".into()]).await;
    let app = common::app(state);

    let form = "name=bad\
        &git_url=u\
        &embedding_model=m\
        &chunk_size=100\
        &chunk_overlap=100";
    let resp = app
        .serve(req_with_cookie(Method::POST, "/rag", &cookie, Some(form)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("Chunk overlap"), "validation toast missing");
}

#[tokio::test]
async fn admin_reindex_flips_status_back_to_pending() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie =
        seed_session_with_roles(&state, "boss", "boss@example.com", vec!["admin".into()]).await;
    // Seed a collection directly through the DB so we don't depend on
    // the create path.
    use gateway::server::db::rag as rag_db;
    let c = rag_db::create_collection(
        &state.db,
        &rag_db::NewCollection {
            name: "errored-one".into(),
            description: None,
            git_url: "https://example.invalid".into(),
            git_ref: "main".into(),
            pat: None,
            embedding_model: "embed-1".into(),
            include_globs: vec![],
            exclude_globs: vec![],
            chunk_size: 800,
            chunk_overlap: 100,
            search_mode: rag_db::SearchMode::Versioned,
        },
    )
    .await
    .unwrap();
    rag_db::mark_failed(&state.db, c.id, "synthetic failure")
        .await
        .unwrap();

    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/{}/reindex", c.id),
            &cookie,
            Some(""),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    // SSE response should carry the freshly-rendered row with status badge.
    assert!(body.contains(&format!("rag-row-{}", c.id)));
    assert!(body.contains("Queued re-index"));
}

#[tokio::test]
async fn admin_delete_removes_row() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie =
        seed_session_with_roles(&state, "boss", "boss@example.com", vec!["admin".into()]).await;
    use gateway::server::db::rag as rag_db;
    let c = rag_db::create_collection(
        &state.db,
        &rag_db::NewCollection {
            name: "to-delete".into(),
            description: None,
            git_url: "u".into(),
            git_ref: "main".into(),
            pat: None,
            embedding_model: "m".into(),
            include_globs: vec![],
            exclude_globs: vec![],
            chunk_size: 800,
            chunk_overlap: 100,
            search_mode: rag_db::SearchMode::Versioned,
        },
    )
    .await
    .unwrap();
    let db = state.db.clone();
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/{}/delete", c.id),
            &cookie,
            Some(""),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("Collection removed."));
    // DB confirms.
    assert!(
        rag_db::find_collection_by_id(&db, c.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn create_form_renders_select_when_embedding_pool_is_configured() {
    // state_with_admin_rbac wires a Chat pool only; the RAG page falls back
    // to a text input there. Construct a state with an embedding pool seeded
    // so the dropdown branch is exercised end-to-end.
    use gateway::server::upstreams::{
        UpstreamRegistry,
        config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
    };
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    let mut pools = HashMap::new();
    pools.insert(
        "embed".to_string(),
        UpstreamPoolConfig {
            compliance: Default::default(),
            kind: PoolKind::Embedding,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                name: "mock".into(),
                base_url: "http://unused.invalid".into(),
                api_key_env: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    pools.insert(
        "chat".to_string(),
        UpstreamPoolConfig {
            compliance: Default::default(),
            kind: PoolKind::Chat,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                name: "chat".into(),
                base_url: "http://unused.invalid".into(),
                api_key_env: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    let registry = UpstreamRegistry::new(&pools).unwrap();
    // Seed two embedding models on the embedding pool.
    let embed = registry.pools().find(|p| p.name == "embed").unwrap();
    embed.backends[0].set_models(HashSet::from([
        "bge-small-en-v1.5".to_string(),
        "voyage-3".to_string(),
    ]));

    // Build a state around that registry, otherwise mirroring
    // state_with_admin_rbac.
    use gateway::rama_server::{RamaState, SessionStore};
    use gateway::server::db;
    use gateway::server::rbac::Resolver;
    use gateway::server::rbac::config::{RbacConfig, RoleConfig, RoleMapping};
    use gateway::server::tools::ToolRegistry;
    use gateway::server::{AppState, Config};
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let tools = Arc::new(ToolRegistry::new());
    let rbac = Arc::new(
        Resolver::build(
            RbacConfig {
                default_role: None,
                mappings: vec![RoleMapping {
                    oidc_claim: "groups".into(),
                    oidc_value: "admin".into(),
                    role: "admin".into(),
                }],
            },
            vec![RoleConfig {
                id: "admin".into(),
                models: vec!["*".into()],
                tools: vec!["*".into()],
            }],
        )
        .unwrap(),
    );
    let app = AppState::new(Config::default(), pool.clone(), registry, tools, rbac);
    let sessions = SessionStore::new(pool, common::TEST_SECRET);
    let state = RamaState::new(app, sessions);

    let cookie =
        seed_session_with_roles(&state, "boss", "boss@example.com", vec!["admin".into()]).await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(Method::GET, "/rag", &cookie, None))
        .await
        .unwrap();
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    // <select> appears + both model options + the chooser placeholder.
    assert!(
        body.contains("<select"),
        "create form should render a <select>"
    );
    assert!(body.contains("bge-small-en-v1.5"));
    assert!(body.contains("voyage-3"));
    assert!(body.contains("Choose an embedding model"));
}

#[tokio::test]
async fn admin_edit_form_then_update_round_trip() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie =
        seed_session_with_roles(&state, "boss", "boss@example.com", vec!["admin".into()]).await;
    use gateway::server::db::rag as rag_db;
    let c = rag_db::create_collection(
        &state.db,
        &rag_db::NewCollection {
            name: "fixture".into(),
            description: Some("first description".into()),
            git_url: "https://example.invalid/old.git".into(),
            git_ref: "main".into(),
            pat: Some("ghp_oldtoken".into()),
            embedding_model: "embed-1".into(),
            include_globs: vec!["*.rs".into()],
            exclude_globs: vec![],
            chunk_size: 400,
            chunk_overlap: 50,
            search_mode: rag_db::SearchMode::Versioned,
        },
    )
    .await
    .unwrap();
    let db = state.db.clone();
    let app = common::app(state);

    // edit-form returns an SSE patch carrying the edit-mode HTML
    // (a form with the existing description pre-filled).
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/{}/edit-form", c.id),
            &cookie,
            Some(""),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("Editing fixture"));
    assert!(body.contains("first description"));
    // PAT currently set → badge + clear_pat checkbox visible.
    assert!(body.contains("currently set"));
    assert!(body.contains("clear_pat"));

    // Submit an update — change description + clear the PAT.
    let form = "description=new+description\
        &git_url=https://example.invalid/new.git\
        &git_ref=main\
        &embedding_model=embed-2\
        &include_globs=*.rs,*.md\
        &exclude_globs=target/\
        &chunk_size=800\
        &chunk_overlap=100\
        &clear_pat=1";
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/{}/update", c.id),
            &cookie,
            Some(form),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("Saved `fixture`"));
    // Patched row reflects the new description + new git_url.
    assert!(body.contains("new description"));
    assert!(body.contains("example.invalid/new.git"));

    // DB confirms.
    let after = rag_db::find_collection_by_id(&db, c.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.description.as_deref(), Some("new description"));
    assert_eq!(after.git_url, "https://example.invalid/new.git");
    assert_eq!(after.embedding_model, "embed-2");
    assert!(after.pat.is_none(), "PAT should have been cleared");
    assert_eq!(after.chunk_size, 800);
}

#[tokio::test]
async fn update_with_empty_pat_keeps_existing_pat() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie =
        seed_session_with_roles(&state, "boss", "boss@example.com", vec!["admin".into()]).await;
    use gateway::server::db::rag as rag_db;
    let c = rag_db::create_collection(
        &state.db,
        &rag_db::NewCollection {
            name: "keep-pat".into(),
            description: None,
            git_url: "https://example.invalid/repo.git".into(),
            git_ref: "main".into(),
            pat: Some("ghp_keepme".into()),
            embedding_model: "embed-1".into(),
            include_globs: vec![],
            exclude_globs: vec![],
            chunk_size: 800,
            chunk_overlap: 100,
            search_mode: rag_db::SearchMode::Versioned,
        },
    )
    .await
    .unwrap();
    let db = state.db.clone();
    let app = common::app(state);
    // Don't include `pat` or `clear_pat` in the form — the PAT must
    // survive untouched.
    let form = "git_url=https://example.invalid/repo.git\
        &git_ref=main\
        &embedding_model=embed-1\
        &chunk_size=800\
        &chunk_overlap=100";
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/{}/update", c.id),
            &cookie,
            Some(form),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let after = rag_db::find_collection_by_id(&db, c.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.pat.as_deref(), Some("ghp_keepme"));
}

#[tokio::test]
async fn cancel_edit_returns_display_row_without_saving() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie =
        seed_session_with_roles(&state, "boss", "boss@example.com", vec!["admin".into()]).await;
    use gateway::server::db::rag as rag_db;
    let c = rag_db::create_collection(
        &state.db,
        &rag_db::NewCollection {
            name: "no-change".into(),
            description: Some("keep me".into()),
            git_url: "https://example.invalid".into(),
            git_ref: "main".into(),
            pat: None,
            embedding_model: "embed-1".into(),
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
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/{}/cancel-edit", c.id),
            &cookie,
            Some(""),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    // Patched row is the *display* row, not the edit form.
    assert!(body.contains("no-change"));
    assert!(body.contains("keep me"));
    assert!(!body.contains("Editing no-change"));
}

#[tokio::test]
async fn non_admin_post_is_forbidden() {
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie = seed_session_with_roles(&state, "u1", "u1@example.com", vec![]).await;
    let app = common::app(state);
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            "/rag",
            &cookie,
            Some("name=x&git_url=y&embedding_model=z"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_can_add_set_primary_reindex_and_delete_refs() {
    use gateway::server::db::rag as rag_db;
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie =
        seed_session_with_roles(&state, "boss", "boss@example.com", vec!["admin".into()]).await;
    let c = rag_db::create_collection(
        &state.db,
        &rag_db::NewCollection {
            name: "ceph".into(),
            description: None,
            git_url: "https://example.invalid".into(),
            git_ref: "main".into(),
            pat: None,
            embedding_model: "embed-1".into(),
            include_globs: vec![],
            exclude_globs: vec![],
            chunk_size: 800,
            chunk_overlap: 100,
            search_mode: rag_db::SearchMode::Versioned,
        },
    )
    .await
    .unwrap();
    let db = state.db.clone();
    let app = common::app(state);

    // Add the first ref → becomes primary; the row shows it.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/{}/refs", c.id),
            &cookie,
            Some("git_ref=reef"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(body.contains("reef"), "added ref missing\n{body}");
    assert!(body.contains("primary"), "first ref should be primary");

    // Add a second ref → not primary.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/{}/refs", c.id),
            &cookie,
            Some("git_ref=squid"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let refs = rag_db::list_refs(&db, c.id).await.unwrap();
    assert_eq!(refs.len(), 2);
    let reef = refs.iter().find(|r| r.git_ref == "reef").unwrap().clone();
    let squid = refs.iter().find(|r| r.git_ref == "squid").unwrap().clone();
    assert!(reef.is_primary && !squid.is_primary);

    // Promote squid to primary.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/refs/{}/primary", squid.id),
            &cookie,
            Some(""),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        rag_db::primary_ref(&db, c.id)
            .await
            .unwrap()
            .unwrap()
            .git_ref,
        "squid"
    );

    // Re-index reef (sitting in `error`) → flips back to `pending`.
    rag_db::set_ref_status(&db, reef.id, rag_db::CollectionStatus::Error)
        .await
        .unwrap();
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/refs/{}/reindex", reef.id),
            &cookie,
            Some(""),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        rag_db::find_ref_by_id(&db, reef.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        rag_db::CollectionStatus::Pending
    );

    // Delete reef → only squid remains.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/refs/{}/delete", reef.id),
            &cookie,
            Some(""),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let remaining = rag_db::list_refs(&db, c.id).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].git_ref, "squid");
}

#[tokio::test]
async fn admin_creates_aggregate_collection_and_bulk_adds_sources() {
    use gateway::server::db::rag as rag_db;
    let state = common::state_with_admin_rbac("http://unused.invalid").await;
    let cookie =
        seed_session_with_roles(&state, "boss", "boss@example.com", vec!["admin".into()]).await;
    let db = state.db.clone();
    let app = common::app(state);

    // Create an aggregate collection — empty Git URL is allowed here, the
    // `aggregate` checkbox is ticked, and `master` is the default ref.
    let form = "name=proxmox\
        &git_url=\
        &git_ref=master\
        &embedding_model=embed-1\
        &include_globs=*.pm\
        &aggregate=on";
    let resp = app
        .serve(req_with_cookie(Method::POST, "/rag", &cookie, Some(form)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("aggregate"),
        "aggregate badge missing\n{body}"
    );

    let c = rag_db::find_collection_by_name(&db, "proxmox")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.search_mode, rag_db::SearchMode::Aggregate);
    // Aggregate collections start with no refs — sources are added below.
    assert!(rag_db::list_refs(&db, c.id).await.unwrap().is_empty());

    // Bulk-add three sources; the third pins an explicit `@stable-8` ref.
    let bulk = "sources=https://github.com/proxmox/pve-manager.git%0A\
        https://github.com/proxmox/qemu-server.git%0A\
        https://github.com/proxmox/pve-docs.git%20%40stable-8";
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/{}/refs/bulk", c.id),
            &cookie,
            Some(bulk),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("Queued indexing of 3 source(s)."),
        "bulk toast missing\n{body}"
    );

    let refs = rag_db::list_refs(&db, c.id).await.unwrap();
    assert_eq!(refs.len(), 3);
    let by_label: std::collections::HashMap<String, &rag_db::CollectionRef> =
        refs.iter().map(|r| (r.source_label(&c), r)).collect();
    assert_eq!(by_label["pve-manager"].git_ref, "master");
    assert_eq!(by_label["qemu-server"].git_ref, "master");
    assert_eq!(by_label["pve-docs"].git_ref, "stable-8");
    assert_eq!(
        by_label["pve-manager"].git_url.as_deref(),
        Some("https://github.com/proxmox/pve-manager.git")
    );

    // Re-posting the same list is idempotent — duplicates are skipped.
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/{}/refs/bulk", c.id),
            &cookie,
            Some(bulk),
        ))
        .await
        .unwrap();
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        body.contains("skipped 3 duplicate"),
        "dup-skip toast missing\n{body}"
    );
    assert_eq!(rag_db::list_refs(&db, c.id).await.unwrap().len(), 3);

    // Editing an aggregate collection must NOT require a Git URL (it has
    // none — each source brings its own). Updating with an empty git_url
    // should succeed, not bounce with "Git URL is required".
    let update = "git_url=\
        &git_ref=master\
        &embedding_model=embed-1\
        &description=updated+desc\
        &chunk_size=800\
        &chunk_overlap=100";
    let resp = app
        .serve(req_with_cookie(
            Method::POST,
            &format!("/rag/{}/update", c.id),
            &cookie,
            Some(update),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(common::read_body(resp).await.to_vec()).unwrap();
    assert!(
        !body.contains("Git URL is required"),
        "aggregate edit was wrongly rejected for empty Git URL\n{body}"
    );
    assert_eq!(
        rag_db::find_collection_by_id(&db, c.id)
            .await
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("updated desc")
    );
}
