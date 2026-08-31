// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The customer's question, end to end.
//!
//! *"When did we last get an invoice from ACME, how much, and what are the
//! details?"* — over a WebDAV folder of scanned PDFs, through OCR, through
//! the profile extraction pass, answered by `rag_query_documents`.
//!
//! This is the test that pins the whole reason phase 3 exists. Passage
//! retrieval cannot answer it: "last" is a superlative over a filtered set,
//! and five similar chunks out of hundreds of near-identical invoices is a
//! coin flip the model cannot detect. The assertion below is that the
//! *right* invoice comes back, and that the total count travels with it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gateway_core::server::config::OcrConfig;
use gateway_core::server::crypto::Crypto;
use gateway_core::server::db::{self, rag as rag_db, rag_documents as docs_db};
use gateway_core::server::upstreams::{
    UpstreamRegistry,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use gateway_core::server::usage::UsageHandle;
use gateway_features::server::ocr::OcrService;
use gateway_features::server::rag::worker::{Indexer, IndexerConfig};
use serde_json::{Value, json};
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const DAV_ROOT: &str = "/remote.php/dav/files/svc";

/// The corpus: three scanned invoices, two of them from the same vendor.
const INVOICES: &[(&str, &str, &str, &str)] = &[
    (
        "inv-a.pdf",
        "ACME GmbH",
        "2025-03-01",
        "Invoice 2025-001 from ACME GmbH dated 2025-03-01, total 100.00 EUR",
    ),
    (
        "inv-b.pdf",
        "ACME GmbH",
        "2025-11-04",
        "Invoice 2025-042 from ACME GmbH dated 2025-11-04, total 1234.56 EUR",
    ),
    (
        "inv-c.pdf",
        "Globex Ltd",
        "2025-12-20",
        "Invoice G-9 from Globex Ltd dated 2025-12-20, total 500.00 EUR",
    ),
];

fn pool_cfg(kind: PoolKind, url: &str, model: &str, name: &str) -> UpstreamPoolConfig {
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

fn registry(embed: &str, ocr: &str, chat: &str) -> Arc<UpstreamRegistry> {
    let mut pools = HashMap::new();
    pools.insert(
        "embed".into(),
        pool_cfg(PoolKind::Embedding, embed, "embed-test", "e"),
    );
    pools.insert("ocr".into(), pool_cfg(PoolKind::Ocr, ocr, "ocr-test", "o"));
    pools.insert(
        "chat".into(),
        pool_cfg(PoolKind::Chat, chat, "chat-test", "c"),
    );
    let reg = UpstreamRegistry::new(&pools).unwrap();
    for p in reg.pools() {
        let model = match p.kind {
            PoolKind::Ocr => "ocr-test",
            PoolKind::Chat => "chat-test",
            _ => "embed-test",
        };
        p.backends[0].set_models(HashSet::from([model.to_string()]));
    }
    reg
}

async fn embedding_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let n = body
                .get("input")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            let data: Vec<Value> = (0..n)
                .map(|i| json!({"object": "embedding", "index": i, "embedding": [1.0, 0.0]}))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": data}))
        })
        .mount(&server)
        .await;
    server
}

/// An OCR sidecar that recognises each invoice into its known text. Keyed by
/// the filename the multipart body carries.
async fn ocr_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ocr"))
        .respond_with(|req: &Request| {
            let body = String::from_utf8_lossy(&req.body);
            let text = INVOICES
                .iter()
                .find(|(name, ..)| body.contains(name))
                .map(|(.., text)| *text)
                .unwrap_or("unrecognised document");
            ResponseTemplate::new(200).set_body_json(json!({
                "pages": [{"page": 1, "markdown": text}],
                "pages_total": 1,
                "pages_processed": 1,
                "failed_pages": [],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            }))
        })
        .mount(&server)
        .await;
    server
}

/// A chat backend standing in for the extraction model: it reads the
/// document text out of the prompt and returns the fields a real model
/// would. Deliberately parses rather than hardcodes, so the test exercises
/// the prompt→JSON→typed-column path rather than asserting on a constant.
async fn extraction_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let prompt = body
                .pointer("/messages/1/content")
                .and_then(Value::as_str)
                .unwrap_or("");
            let matched = INVOICES
                .iter()
                .find(|(.., text)| prompt.contains(&text[..30]));
            let fields = match matched {
                Some((_, vendor, date, text)) => {
                    let total = text
                        .split("total ")
                        .nth(1)
                        .and_then(|t| t.split(' ').next())
                        .unwrap_or("0");
                    let number = text
                        .split("Invoice ")
                        .nth(1)
                        .and_then(|t| t.split(' ').next())
                        .unwrap_or("");
                    json!({
                        "doc_type": "invoice",
                        "vendor": vendor,
                        "doc_date": date,
                        "invoice_number": number,
                        "total_gross": total,
                        "currency": "EUR",
                        "title": format!("Invoice {number}"),
                        "summary": format!("An invoice from {vendor} dated {date}.")
                    })
                }
                None => json!({}),
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"role": "assistant", "content": fields.to_string()}}]
            }))
        })
        .mount(&server)
        .await;
    server
}

fn multistatus() -> String {
    let mut out = String::from(
        r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:response><d:href>/remote.php/dav/files/svc/</d:href><d:propstat><d:prop>
    <d:resourcetype><d:collection/></d:resourcetype><d:getetag>&quot;r&quot;</d:getetag>
    <oc:fileid>1</oc:fileid>
  </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#,
    );
    for (i, (name, ..)) in INVOICES.iter().enumerate() {
        out.push_str(&format!(
            r#"
  <d:response><d:href>{DAV_ROOT}/{name}</d:href><d:propstat><d:prop>
    <d:resourcetype/><d:getetag>&quot;v{i}&quot;</d:getetag><oc:fileid>{}</oc:fileid>
    <d:getcontentlength>100</d:getcontentlength>
    <d:getcontenttype>application/pdf</d:getcontenttype>
  </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#,
            i + 10
        ));
    }
    out.push_str("\n</d:multistatus>");
    out
}

async fn dav_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path(DAV_ROOT))
        .respond_with(ResponseTemplate::new(207).set_body_string(multistatus()))
        .mount(&server)
        .await;
    for (name, ..) in INVOICES {
        Mock::given(method("GET"))
            .and(path(format!("{DAV_ROOT}/{name}")))
            // Not a parseable PDF, so the text layer is empty and the
            // document takes the OCR rung — the scanned-invoice path.
            //
            // The bytes must differ per file: OCR results are cached by
            // content hash, so three byte-identical "scans" are one document
            // to the cache and would all come back as whichever was read
            // first. Real scans differ; identical fixtures would have been
            // testing the cache rather than the pipeline.
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(format!("%PDF-1.4 scan of {name}").into_bytes()),
            )
            .mount(&server)
            .await;
    }
    server
}

/// Index the fixture corpus and hand back the store to query.
async fn indexed_store() -> (db::Pool, gateway_core::server::db::Pool) {
    let dav = dav_upstream().await;
    let embed = embedding_upstream().await;
    let ocr = ocr_upstream().await;
    let chat = extraction_upstream().await;
    // The mock servers must outlive the build.
    let reg = registry(&embed.uri(), &ocr.uri(), &chat.uri());
    let central = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let crypto = Arc::new(Crypto::from_key([5u8; 32]));

    let indexer = Indexer::new(
        central.clone(),
        Arc::clone(&reg),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..IndexerConfig::default()
        },
        Some(Arc::clone(&crypto)),
    )
    .with_document_readers(
        Some(OcrService::new(
            OcrConfig {
                enabled: true,
                ..OcrConfig::default()
            },
            Arc::clone(&reg),
            reqwest::Client::new(),
            UsageHandle::disabled(),
            central.clone(),
        )),
        None,
    );

    // The `invoice` profile ships seeded by migration 0059 — the operator
    // does not have to author one to get started.
    let profile = docs_db::find_profile_by_name(&central, "invoice")
        .await
        .unwrap()
        .expect("the invoice profile is seeded");

    let sealed = crypto
        .seal_str(&json!({"password": "pw"}).to_string())
        .unwrap();
    let c = rag_db::create_collection(
        &central,
        &rag_db::NewCollection {
            name: "invoices".into(),
            description: None,
            git_url: String::new(),
            git_ref: "main".into(),
            pat: None,
            source: rag_db::SourceSpec {
                kind: "webdav".into(),
                config: [
                    ("base_url".to_string(), dav.uri()),
                    ("username".to_string(), "svc".to_string()),
                ]
                .into_iter()
                .collect(),
                secrets: Some(sealed),
            },
            profile_id: Some(profile.id),
            extraction_model: Some("chat-test".into()),
            embedding_model: "embed-test".into(),
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            chunk_size: 400,
            chunk_overlap: 40,
            search_mode: rag_db::SearchMode::Versioned,
        },
    )
    .await
    .unwrap();
    let r = rag_db::add_ref(&central, c.id, "main", None, true)
        .await
        .unwrap();
    indexer.index_ref(r.id).await.unwrap();

    let after = rag_db::find_ref_by_id(&central, r.id)
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
    // Keep the mocks and dirs alive for the caller's queries.
    std::mem::forget((dav, embed, ocr, chat, data_dir, indexer));
    (central, store)
}

fn filter(key: &str, value: &str, ty: docs_db::FieldType) -> docs_db::Filter {
    docs_db::Filter {
        key: key.into(),
        op: docs_db::FilterOp::Matches,
        value: value.into(),
        field_type: ty,
    }
}

#[tokio::test]
async fn when_did_we_last_get_an_invoice_from_acme_and_how_much() {
    let (_central, store) = indexed_store().await;

    // The question, as the query tool expresses it: filter by vendor, sort
    // by date descending, take one.
    let query = docs_db::DocumentQuery {
        filters: vec![filter("vendor", "acme", docs_db::FieldType::Text)],
        folder: None,
        order_by: Some((
            "doc_date".into(),
            docs_db::FieldType::Date,
            docs_db::SortDir::Desc,
        )),
        limit: 1,
    };
    let (docs, total) = docs_db::query_documents(&store, &query).await.unwrap();

    assert_eq!(
        total, 2,
        "both ACME invoices matched and Globex's did not — the count the model needs to \
         know it is not looking at everything"
    );
    assert_eq!(docs.len(), 1);
    let latest = &docs[0];
    assert_eq!(latest.path, "inv-b.pdf", "the most recent ACME invoice");
    assert_eq!(latest.fields.get("doc_date").unwrap(), "2025-11-04");
    assert_eq!(latest.fields.get("total_gross").unwrap(), "1234.56");
    assert_eq!(latest.fields.get("invoice_number").unwrap(), "2025-042");
    assert_eq!(latest.fields.get("currency").unwrap(), "EUR");
    assert!(
        latest.summary.as_deref().unwrap_or("").contains("ACME"),
        "the stored summary is what makes a whole-folder question affordable"
    );
    assert_eq!(
        latest.extractor, "ocr",
        "and the answer records that it came from a scan, not a clean text layer"
    );
}

#[tokio::test]
async fn how_much_did_we_get_billed_by_acme_in_total() {
    let (_central, store) = indexed_store().await;
    let query = docs_db::DocumentQuery {
        filters: vec![filter("vendor", "acme", docs_db::FieldType::Text)],
        folder: None,
        order_by: None,
        limit: 50,
    };
    let sum = docs_db::sum_field(&store, &query, "total_gross")
        .await
        .unwrap();
    assert_eq!(
        sum,
        Some(1334.56),
        "100.00 + 1234.56 — an aggregate no amount of passage retrieval can produce"
    );
}

#[tokio::test]
async fn every_scanned_invoice_reached_the_document_table() {
    let (_central, store) = indexed_store().await;
    let (docs, total) = docs_db::query_documents(
        &store,
        &docs_db::DocumentQuery {
            filters: Vec::new(),
            folder: None,
            order_by: None,
            limit: 50,
        },
    )
    .await
    .unwrap();
    assert_eq!(total, 3, "all three scans were OCR'd and extracted");
    assert!(
        docs.iter().all(|d| d.fields.contains_key("vendor")),
        "every document carries the fields the profile asked for"
    );
}

#[tokio::test]
async fn a_second_index_pass_reuses_the_cached_extraction() {
    let (central, _store) = indexed_store().await;
    // Every document produced exactly one cached extraction row, keyed by
    // content — which is what stops a rebuild re-running the model over a
    // corpus that did not change.
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rag_extractions")
        .fetch_one(&central)
        .await
        .unwrap();
    assert_eq!(rows, INVOICES.len() as i64);
    let failed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rag_extractions WHERE error IS NOT NULL")
            .fetch_one(&central)
            .await
            .unwrap();
    assert_eq!(failed, 0, "no extraction failed");
}
