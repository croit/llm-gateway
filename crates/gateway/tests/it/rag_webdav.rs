// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! End-to-end test for indexing a *remote* RAG source.
//!
//! Stands up a wiremock server that speaks enough Nextcloud-flavoured WebDAV
//! to be indexed — `PROPFIND` returning a multistatus with `oc:fileid` and
//! etags, `GET` returning file bytes — plus the same deterministic embedding
//! upstream the git pipeline test uses. Then drives a real [`Indexer`] over a
//! collection whose source is `webdav` and asserts the corpus is searchable
//! with correct provenance.
//!
//! What this pins, beyond "it works":
//!   * credentials round-trip through the sealed `source_secrets` column and
//!     are actually presented to the server,
//!   * the indexing path below enumeration is genuinely shared with git —
//!     the same `search_chunks` call answers over a remote corpus,
//!   * a source that cannot be reached fails the ref with an actionable
//!     message instead of going `ready` and empty.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gateway_core::server::crypto::Crypto;
use gateway_core::server::db::{self, rag as rag_db};
use gateway_core::server::upstreams::{
    UpstreamRegistry,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use gateway_features::server::embeddings;
use gateway_features::server::rag::worker::{Indexer, IndexerConfig, search_chunks};
use serde_json::{Value, json};
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const DAV_ROOT: &str = "/remote.php/dav/files/svc";

fn one_hot(input: &str) -> [f32; 4] {
    let s = input.to_lowercase();
    if s.contains("invoice") {
        [1.0, 0.0, 0.0, 0.0]
    } else if s.contains("roadmap") {
        [0.0, 1.0, 0.0, 0.0]
    } else if s.contains("minutes") {
        [0.0, 0.0, 1.0, 0.0]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

async fn start_embedding_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let inputs: Vec<String> = match body.get("input") {
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|v| v.as_str().unwrap_or_default().to_string())
                    .collect(),
                Some(Value::String(s)) => vec![s.clone()],
                _ => Vec::new(),
            };
            let data: Vec<Value> = inputs
                .iter()
                .enumerate()
                .map(|(i, s)| json!({"object": "embedding", "index": i, "embedding": one_hot(s)}))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": data}))
        })
        .mount(&server)
        .await;
    server
}

fn registry_pointed_at(upstream_url: &str) -> Arc<UpstreamRegistry> {
    let mut pools = HashMap::new();
    pools.insert(
        "embed".to_string(),
        UpstreamPoolConfig {
            voices: Default::default(),
            offer_voices: Vec::new(),
            allowed_groups: Vec::new(),
            fallback_offline: None,
            compliance: Default::default(),
            enforce_limits: true,
            kind: PoolKind::Embedding,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                alias: None,
                probe_models: true,
                supports_edit: false,
                name: "mock".into(),
                base_url: upstream_url.into(),
                api_key_env: None,
                api_key: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    let registry = UpstreamRegistry::new(&pools).unwrap();
    let pool = registry
        .pools()
        .into_iter()
        .find(|p| p.name == "embed")
        .unwrap();
    pool.backends[0].set_models(HashSet::from(["embed-test".to_string()]));
    registry
}

/// A multistatus response body for one directory listing.
fn multistatus(entries: &[(&str, bool, &str, &str, u64)]) -> String {
    let mut out = String::from(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">"#,
    );
    for (href, is_dir, etag, fileid, size) in entries {
        let resourcetype = if *is_dir {
            "<d:resourcetype><d:collection/></d:resourcetype>"
        } else {
            "<d:resourcetype/>"
        };
        let size_prop = if *is_dir {
            String::new()
        } else {
            format!(
                "<d:getcontentlength>{size}</d:getcontentlength><d:getcontenttype>text/plain</d:getcontenttype>"
            )
        };
        out.push_str(&format!(
            r#"
  <d:response>
    <d:href>{href}</d:href>
    <d:propstat>
      <d:prop>
        {resourcetype}
        <d:getetag>&quot;{etag}&quot;</d:getetag>
        <oc:fileid>{fileid}</oc:fileid>
        {size_prop}
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>"#
        ));
    }
    out.push_str("\n</d:multistatus>");
    out
}

/// A server holding two files at the root and one inside a subfolder.
async fn start_webdav() -> MockServer {
    let server = MockServer::start().await;

    // The root listing: itself, two files, one subfolder.
    Mock::given(method("PROPFIND"))
        .and(path(DAV_ROOT))
        .respond_with(ResponseTemplate::new(207).set_body_string(multistatus(&[
            (&format!("{DAV_ROOT}/"), true, "root-v1", "1", 0),
            (
                &format!("{DAV_ROOT}/invoice-2025-11.txt"),
                false,
                "inv-v1",
                "10",
                64,
            ),
            (
                &format!("{DAV_ROOT}/roadmap.txt"),
                false,
                "road-v1",
                "11",
                64,
            ),
            (&format!("{DAV_ROOT}/notes"), true, "notes-v1", "12", 0),
            // Excluded by the include globs, so it must never be fetched.
            (&format!("{DAV_ROOT}/photo.png"), false, "img-v1", "13", 900),
        ])))
        .mount(&server)
        .await;

    Mock::given(method("PROPFIND"))
        .and(path(format!("{DAV_ROOT}/notes")))
        .respond_with(ResponseTemplate::new(207).set_body_string(multistatus(&[
            (&format!("{DAV_ROOT}/notes/"), true, "notes-v1", "12", 0),
            (
                &format!("{DAV_ROOT}/notes/minutes.txt"),
                false,
                "min-v1",
                "14",
                64,
            ),
        ])))
        .mount(&server)
        .await;

    for (p, body) in [
        (
            "invoice-2025-11.txt",
            "Invoice 2025-11 from ACME GmbH, total 1234.56 EUR",
        ),
        ("roadmap.txt", "Roadmap for the next quarter"),
        ("notes/minutes.txt", "Meeting minutes from the review"),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("{DAV_ROOT}/{p}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    }
    server
}

fn crypto() -> Arc<Crypto> {
    Arc::new(Crypto::from_key([7u8; 32]))
}

/// Build the `source` spec a `webdav` collection carries, sealing the
/// password exactly as the admin surface will.
fn webdav_source(base_url: &str, crypto: &Crypto) -> rag_db::SourceSpec {
    let secrets = serde_json::json!({ "password": "app-pw" }).to_string();
    let sealed = crypto.seal_str(&secrets).unwrap();
    rag_db::SourceSpec {
        kind: "webdav".into(),
        config: [
            ("base_url".to_string(), base_url.to_string()),
            ("username".to_string(), "svc".to_string()),
        ]
        .into_iter()
        .collect(),
        secrets: Some(sealed),
    }
}

async fn seed(
    pool: &db::Pool,
    source: rag_db::SourceSpec,
) -> (rag_db::Collection, rag_db::CollectionRef) {
    let collection = rag_db::create_collection(
        pool,
        &rag_db::NewCollection {
            name: "docs".into(),
            description: None,
            // Unused by a remote source, but the column is NOT NULL.
            git_url: String::new(),
            git_ref: "main".into(),
            pat: None,
            source,
            profile_id: None,
            extraction_model: None,
            embedding_model: "embed-test".into(),
            include_globs: vec!["*.txt".into()],
            exclude_globs: Vec::new(),
            chunk_size: 200,
            chunk_overlap: 20,
            search_mode: rag_db::SearchMode::Versioned,
        },
    )
    .await
    .unwrap();
    let r = rag_db::add_ref(pool, collection.id, "main", None, true)
        .await
        .unwrap();
    (collection, r)
}

#[tokio::test]
async fn indexes_a_webdav_source_end_to_end_and_search_returns_the_right_document() {
    let dav = start_webdav().await;
    let upstream = start_embedding_upstream().await;
    let registry = registry_pointed_at(&upstream.uri());
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let crypto = crypto();

    let indexer = Indexer::new(
        pool.clone(),
        Arc::clone(&registry),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..IndexerConfig::default()
        },
        Some(Arc::clone(&crypto)),
    );

    let (collection, r) = seed(&pool, webdav_source(&dav.uri(), &crypto)).await;
    indexer.index_ref(r.id).await.unwrap();

    let after = rag_db::find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
    assert_eq!(
        after.status,
        rag_db::CollectionStatus::Ready,
        "last_error: {:?}",
        after.last_error
    );
    assert!(after.is_searchable());

    let store = indexer
        .collection_store(after.id, &after.data_uuid)
        .await
        .unwrap();
    let files = rag_db::list_files_for_collection(&store, collection.id)
        .await
        .unwrap();
    let mut paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec!["invoice-2025-11.txt", "notes/minutes.txt", "roadmap.txt"],
        "the walk recursed into the subfolder and the include globs kept the png out"
    );

    // The whole point: retrieval over a remote corpus goes through exactly
    // the same path as a git one.
    let query_vec = embeddings::embed(
        &reqwest::Client::new(),
        &registry,
        "embed-test",
        &["what invoice did we get".to_string()],
    )
    .await
    .unwrap()
    .pop()
    .unwrap();
    let hits = search_chunks(
        &indexer,
        &after,
        "what invoice did we get",
        &query_vec,
        3,
        None,
    )
    .await
    .unwrap();
    assert!(!hits.is_empty(), "expected at least one hit");
    assert_eq!(
        hits[0].0.file_path, "invoice-2025-11.txt",
        "the invoice document should rank first for an invoice query"
    );
}

#[tokio::test]
async fn the_stored_password_is_presented_to_the_server() {
    let dav = start_webdav().await;
    let upstream = start_embedding_upstream().await;
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let crypto = crypto();

    let indexer = Indexer::new(
        pool.clone(),
        registry_pointed_at(&upstream.uri()),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..IndexerConfig::default()
        },
        Some(Arc::clone(&crypto)),
    );

    let (_, r) = seed(&pool, webdav_source(&dav.uri(), &crypto)).await;
    indexer.index_ref(r.id).await.unwrap();

    // base64("svc:app-pw") — the credential survived sealing, storage and
    // unsealing, and reached the wire.
    let expected = "Basic c3ZjOmFwcC1wdw==";
    let requests = dav.received_requests().await.unwrap();
    assert!(
        requests.iter().any(|req| {
            req.headers
                .get("authorization")
                .is_some_and(|v| v.to_str().unwrap_or_default() == expected)
        }),
        "no request carried the stored credential"
    );
}

#[tokio::test]
async fn an_unreachable_source_fails_the_ref_with_an_actionable_message() {
    let dav = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&dav)
        .await;
    let upstream = start_embedding_upstream().await;
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let crypto = crypto();

    let indexer = Indexer::new(
        pool.clone(),
        registry_pointed_at(&upstream.uri()),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..IndexerConfig::default()
        },
        Some(Arc::clone(&crypto)),
    );

    let (_, r) = seed(&pool, webdav_source(&dav.uri(), &crypto)).await;
    assert!(indexer.index_ref(r.id).await.is_err());

    let after = rag_db::find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
    assert_eq!(after.status, rag_db::CollectionStatus::Error);
    let msg = after.last_error.clone().unwrap_or_default();
    assert!(
        msg.to_lowercase().contains("credential"),
        "the operator is told what to fix, got: {msg}"
    );
    assert!(
        !after.is_searchable(),
        "a source that never listed must not look searchable"
    );
}

#[tokio::test]
async fn a_collection_with_sealed_secrets_but_no_key_says_so() {
    let dav = start_webdav().await;
    let upstream = start_embedding_upstream().await;
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let crypto = crypto();

    // No at-rest key handed to the indexer.
    let indexer = Indexer::new(
        pool.clone(),
        registry_pointed_at(&upstream.uri()),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..IndexerConfig::default()
        },
        None,
    );

    let (_, r) = seed(&pool, webdav_source(&dav.uri(), &crypto)).await;
    assert!(indexer.index_ref(r.id).await.is_err());

    let after = rag_db::find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
    let msg = after.last_error.unwrap_or_default();
    assert!(
        msg.contains("encryption key"),
        "the failure names the real cause, got: {msg}"
    );
}
