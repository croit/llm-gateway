// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Documents — not just text files — reaching the index.
//!
//! Before the extraction ladder the indexer did `String::from_utf8` and
//! `continue`d, so a corpus of PDFs indexed to nothing and reported itself
//! ready. These tests pin the two things that changed:
//!
//!   * a **born-digital PDF** is read from its text layer, in-process, and
//!     never touches the OCR backend — the property that makes a corpus of
//!     thousands of invoices affordable;
//!   * a **scan** goes to the OCR sidecar and comes back searchable, cited by
//!     **page**, because line numbers do not survive extraction and a wrong
//!     citation is worse than a coarse one.
//!
//! Both run over a WebDAV source, so they also cover the seam between the
//! provider layer and the extraction ladder.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gateway_core::server::config::OcrConfig;
use gateway_core::server::crypto::Crypto;
use gateway_core::server::db::{self, rag as rag_db};
use gateway_core::server::upstreams::{
    UpstreamRegistry,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use gateway_core::server::usage::UsageHandle;
use gateway_features::server::embeddings;
use gateway_features::server::ocr::OcrService;
use gateway_features::server::pdf::test_support::multipage_pdf;
use gateway_features::server::rag::worker::{Indexer, IndexerConfig, search_chunks};
use serde_json::{Value, json};
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const DAV_ROOT: &str = "/remote.php/dav/files/svc";

fn one_hot(input: &str) -> [f32; 4] {
    let s = input.to_lowercase();
    if s.contains("invoice") || s.contains("acme") {
        [1.0, 0.0, 0.0, 0.0]
    } else if s.contains("page 2") || s.contains("second") {
        [0.0, 1.0, 0.0, 0.0]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

async fn embedding_upstream() -> MockServer {
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

fn pool(kind: PoolKind, url: &str, model: &str, name: &str) -> UpstreamPoolConfig {
    UpstreamPoolConfig {
        voices: Default::default(),
        offer_voices: Vec::new(),
        allowed_groups: Vec::new(),
        fallback_offline: None,
        compliance: Default::default(),
        enforce_limits: true,
        kind,
        strategy: PickerStrategy::RoundRobin,
        models: Vec::new(),
        backend: vec![BackendConfig {
            alias: None,
            probe_models: true,
            supports_edit: false,
            name: name.into(),
            base_url: url.into(),
            api_key_env: None,
            api_key: None,
            weight: 1,
            max_inflight: 16,
            health_path: "/models".into(),
            models: vec![model.to_string()],
        }],
    }
}

/// Registry with an embedding pool and, optionally, an OCR pool.
fn registry(embed_url: &str, ocr_url: Option<&str>) -> Arc<UpstreamRegistry> {
    let mut pools = HashMap::new();
    pools.insert(
        "embed".to_string(),
        pool(PoolKind::Embedding, embed_url, "embed-test", "mock-embed"),
    );
    if let Some(url) = ocr_url {
        pools.insert(
            "ocr".to_string(),
            pool(PoolKind::Ocr, url, "ocr-test", "mock-ocr"),
        );
    }
    let reg = UpstreamRegistry::new(&pools).unwrap();
    for p in reg.pools() {
        let model = match p.kind {
            PoolKind::Ocr => "ocr-test",
            _ => "embed-test",
        };
        p.backends[0].set_models(HashSet::from([model.to_string()]));
    }
    reg
}

fn multistatus(entries: &[(&str, bool, &str, &str, u64, &str)]) -> String {
    let mut out = String::from(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">"#,
    );
    for (href, is_dir, etag, fileid, size, ctype) in entries {
        let rt = if *is_dir {
            "<d:resourcetype><d:collection/></d:resourcetype>"
        } else {
            "<d:resourcetype/>"
        };
        let extra = if *is_dir {
            String::new()
        } else {
            format!(
                "<d:getcontentlength>{size}</d:getcontentlength>\
                 <d:getcontenttype>{ctype}</d:getcontenttype>"
            )
        };
        out.push_str(&format!(
            r#"
  <d:response><d:href>{href}</d:href><d:propstat><d:prop>
    {rt}<d:getetag>&quot;{etag}&quot;</d:getetag><oc:fileid>{fileid}</oc:fileid>{extra}
  </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#
        ));
    }
    out.push_str("\n</d:multistatus>");
    out
}

/// A WebDAV server serving one file.
async fn dav_with(name: &str, ctype: &str, body: Vec<u8>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path(DAV_ROOT))
        .respond_with(ResponseTemplate::new(207).set_body_string(multistatus(&[
            (&format!("{DAV_ROOT}/"), true, "root-v1", "1", 0, ""),
            (
                &format!("{DAV_ROOT}/{name}"),
                false,
                "doc-v1",
                "10",
                body.len() as u64,
                ctype,
            ),
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{DAV_ROOT}/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;
    server
}

fn crypto() -> Arc<Crypto> {
    Arc::new(Crypto::from_key([9u8; 32]))
}

fn webdav_source(base_url: &str, crypto: &Crypto) -> rag_db::SourceSpec {
    let sealed = crypto
        .seal_str(&json!({"password": "app-pw"}).to_string())
        .unwrap();
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

async fn seed(pool_db: &db::Pool, source: rag_db::SourceSpec) -> rag_db::CollectionRef {
    let c = rag_db::create_collection(
        pool_db,
        &rag_db::NewCollection {
            name: "docs".into(),
            description: None,
            git_url: String::new(),
            git_ref: "main".into(),
            pat: None,
            source,
            profile_id: None,
            extraction_model: None,
            embedding_model: "embed-test".into(),
            // Everything: the point is that documents are no longer filtered
            // out by being binary.
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            chunk_size: 400,
            chunk_overlap: 40,
            search_mode: rag_db::SearchMode::Versioned,
        },
    )
    .await
    .unwrap();
    rag_db::add_ref(pool_db, c.id, "main", None, true)
        .await
        .unwrap()
}

fn ocr_service(reg: Arc<UpstreamRegistry>, db: db::Pool) -> OcrService {
    OcrService::new(
        OcrConfig {
            enabled: true,
            ..OcrConfig::default()
        },
        reg,
        reqwest::Client::new(),
        UsageHandle::disabled(),
        db,
    )
}

#[tokio::test]
async fn a_born_digital_pdf_is_read_from_its_text_layer_and_cited_by_page() {
    // Three pages of real PDF with a text layer. No OCR pool is configured
    // at all, which is the assertion: this path must not need one.
    let pdf = multipage_pdf(3);
    let dav = dav_with("report.pdf", "application/pdf", pdf).await;
    let embed = embedding_upstream().await;
    let reg = registry(&embed.uri(), None);
    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let c = crypto();

    let indexer = Indexer::new(
        db_pool.clone(),
        Arc::clone(&reg),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..IndexerConfig::default()
        },
        Some(Arc::clone(&c)),
    );
    let r = seed(&db_pool, webdav_source(&dav.uri(), &c)).await;
    indexer.index_ref(r.id).await.unwrap();

    let after = rag_db::find_ref_by_id(&db_pool, r.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.status,
        rag_db::CollectionStatus::Ready,
        "last_error: {:?}",
        after.last_error
    );

    let store = indexer
        .collection_store(after.id, &after.data_uuid)
        .await
        .unwrap();
    let chunks: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT loc_kind, loc_from, loc_to FROM rag_chunks ORDER BY loc_from LIMIT 5",
    )
    .fetch_all(&store)
    .await
    .unwrap();
    assert!(!chunks.is_empty(), "the PDF produced no chunks");
    assert!(
        chunks.iter().all(|(kind, _, _)| kind == "page"),
        "a PDF is cited by page, not by line: {chunks:?}"
    );
    assert!(
        chunks.iter().any(|(_, from, _)| *from >= 2),
        "every page was indexed, not just the first: {chunks:?}"
    );
}

#[tokio::test]
async fn a_scan_goes_through_ocr_and_becomes_searchable_by_page() {
    // The OCR sidecar's contract: multipart POST /ocr, per-page markdown back.
    let ocr_backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ocr"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pages": [
                {"page": 1, "markdown": "Invoice 2025-11 from ACME GmbH"},
                {"page": 2, "markdown": "Second page: payment terms"}
            ],
            "pages_total": 2,
            "pages_processed": 2,
            "failed_pages": [],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        })))
        .mount(&ocr_backend)
        .await;

    // A JPEG, so there is no text layer to fall back on — the OCR rung is
    // the only way this document reaches the index.
    let dav = dav_with("scan.jpg", "image/jpeg", vec![0xff, 0xd8, 0xff, 0xe0, 0x00]).await;
    let embed = embedding_upstream().await;
    let reg = registry(&embed.uri(), Some(&ocr_backend.uri()));
    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let c = crypto();

    let indexer = Indexer::new(
        db_pool.clone(),
        Arc::clone(&reg),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..IndexerConfig::default()
        },
        Some(Arc::clone(&c)),
    )
    .with_document_readers(Some(ocr_service(Arc::clone(&reg), db_pool.clone())), None);

    let r = seed(&db_pool, webdav_source(&dav.uri(), &c)).await;
    indexer.index_ref(r.id).await.unwrap();

    let after = rag_db::find_ref_by_id(&db_pool, r.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.status,
        rag_db::CollectionStatus::Ready,
        "last_error: {:?}",
        after.last_error
    );

    // The recognised text is searchable, and the hit cites the page the
    // sidecar reported — which is what a user can open the original to.
    let query = embeddings::embed(
        &reqwest::Client::new(),
        &reg,
        "embed-test",
        &["which invoice did we get from acme".to_string()],
    )
    .await
    .unwrap()
    .pop()
    .unwrap();
    let hits = search_chunks(&indexer, &after, "invoice from acme", &query, 3, None)
        .await
        .unwrap();
    assert!(!hits.is_empty(), "the scan produced no searchable chunks");
    let top = &hits[0].0;
    assert_eq!(top.file_path, "scan.jpg");
    assert!(
        top.content.contains("ACME"),
        "the recognised text is what got indexed: {:?}",
        top.content
    );
    assert_eq!(top.loc.kind, rag_db::LocKind::Page);
    assert_eq!(top.loc.label(), "page 1");
}

/// A rerank pool, scoring by how many query words a passage contains.
///
/// Deliberately a *different* ranking signal from the embedding mock, so the
/// test can tell whether the reranker's opinion actually replaced fusion's
/// rather than coinciding with it.
async fn rerank_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rerank"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let query = body
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            let docs: Vec<String> = body
                .get("documents")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|d| d.as_str().unwrap_or_default().to_lowercase())
                        .collect()
                })
                .unwrap_or_default();
            let results: Vec<Value> = docs
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let hits = query.split_whitespace().filter(|w| d.contains(w)).count();
                    json!({"index": i, "relevance_score": hits as f32})
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({"results": results}))
        })
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_reranker_reorders_what_fusion_returned() {
    // Every chunk embeds identically here, so fusion has no signal at all —
    // exactly the situation a corpus of near-identical documents creates.
    // The cross-encoder is the only thing that can separate them.
    let dav = dav_with(
        "notes.txt",
        "text/plain",
        b"the quick brown fox\n\npayment terms are thirty days\n\nunrelated filler text".to_vec(),
    )
    .await;
    let embed = embedding_upstream().await;
    let rr = rerank_upstream().await;

    let mut pools = HashMap::new();
    pools.insert(
        "embed".to_string(),
        pool(PoolKind::Embedding, &embed.uri(), "embed-test", "e"),
    );
    pools.insert(
        "rr".to_string(),
        pool(PoolKind::Rerank, &rr.uri(), "rerank-test", "r"),
    );
    let reg = UpstreamRegistry::new(&pools).unwrap();
    for p in reg.pools() {
        let m = match p.kind {
            PoolKind::Rerank => "rerank-test",
            _ => "embed-test",
        };
        p.backends[0].set_models(HashSet::from([m.to_string()]));
    }

    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let c = crypto();
    let indexer = Indexer::new(
        db_pool.clone(),
        Arc::clone(&reg),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            // One chunk per line, so there is something to reorder.
            ..IndexerConfig::default()
        },
        Some(Arc::clone(&c)),
    );
    let r = seed(&db_pool, webdav_source(&dav.uri(), &c)).await;
    indexer.index_ref(r.id).await.unwrap();
    let after = rag_db::find_ref_by_id(&db_pool, r.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, rag_db::CollectionStatus::Ready);

    let query = "payment terms";
    let qvec = embeddings::embed(
        &reqwest::Client::new(),
        &reg,
        "embed-test",
        &[query.to_string()],
    )
    .await
    .unwrap()
    .pop()
    .unwrap();
    let hits = search_chunks(&indexer, &after, query, &qvec, 3, None)
        .await
        .unwrap();

    assert!(!hits.is_empty());
    assert!(
        hits[0].0.content.contains("payment terms"),
        "the cross-encoder promoted the passage that actually answers the query, \
         where the embeddings could not tell any of them apart: {:?}",
        hits.iter().map(|(c, _)| &c.content).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn search_still_works_with_no_rerank_pool() {
    // The feature is optional and silently so.
    let dav = dav_with("notes.txt", "text/plain", b"payment terms".to_vec()).await;
    let embed = embedding_upstream().await;
    let reg = registry(&embed.uri(), None);
    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let c = crypto();
    let indexer = Indexer::new(
        db_pool.clone(),
        Arc::clone(&reg),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..IndexerConfig::default()
        },
        Some(Arc::clone(&c)),
    );
    let r = seed(&db_pool, webdav_source(&dav.uri(), &c)).await;
    indexer.index_ref(r.id).await.unwrap();
    let after = rag_db::find_ref_by_id(&db_pool, r.id)
        .await
        .unwrap()
        .unwrap();
    let qvec = embeddings::embed(
        &reqwest::Client::new(),
        &reg,
        "embed-test",
        &["payment".to_string()],
    )
    .await
    .unwrap()
    .pop()
    .unwrap();
    let hits = search_chunks(&indexer, &after, "payment", &qvec, 3, None)
        .await
        .unwrap();
    assert!(!hits.is_empty(), "search works without a reranker");
}

#[tokio::test]
async fn a_scan_with_no_ocr_backend_is_reported_not_silently_dropped() {
    let dav = dav_with("scan.jpg", "image/jpeg", vec![0xff, 0xd8, 0xff, 0xe0, 0x00]).await;
    let embed = embedding_upstream().await;
    let reg = registry(&embed.uri(), None);
    let db_pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let c = crypto();

    let indexer = Indexer::new(
        db_pool.clone(),
        Arc::clone(&reg),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..IndexerConfig::default()
        },
        Some(Arc::clone(&c)),
    );
    let r = seed(&db_pool, webdav_source(&dav.uri(), &c)).await;
    indexer.index_ref(r.id).await.unwrap();

    let after = rag_db::find_ref_by_id(&db_pool, r.id)
        .await
        .unwrap()
        .unwrap();
    // The build succeeded but indexed nothing, and the reason names the thing
    // to switch on — rather than a "ready" collection that is silently empty.
    let msg = after.last_error.clone().unwrap_or_default();
    assert!(
        msg.to_lowercase().contains("ocr"),
        "the operator is told which backend is missing, got: {msg}"
    );

    let entries = rag_db::list_log_entries(&db_pool, r.id, 20).await.unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.message.to_lowercase().contains("ocr")),
        "and it is on the timeline too: {:?}",
        entries.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}
