// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! RAG retrieval eval harness.
//!
//! A deterministic, CI-friendly stand-in for the live golden-query eval.
//! It encodes the exact failure the hybrid retriever was built to fix:
//! a query whose embedding drifts toward semantically-adjacent chunks
//! (CRUSH tunables) while the chunk that actually answers it is an exact
//! identifier (`osd_op_timeout`) the dense vector smears away.
//!
//! The embedding upstream is stubbed so the "drift" is reproducible:
//!   * the query and the CRUSH distractors embed to the SAME vector
//!     (dense can't tell them apart — it ranks distractors first),
//!   * the `osd_op_timeout` chunk embeds orthogonally (dense ranks it
//!     last / off the end of a small top-k).
//!
//! We then assert the lift: dense-only top-3 MISSES the answer, while
//! the hybrid retriever (dense ⊕ FTS5/BM25 via RRF) surfaces it. The
//! golden set is a table so new cases drop in next to this one.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gateway::server::db;
use gateway::server::db::rag as rag_db;
use gateway::server::rag::worker::{Indexer, IndexerConfig, search_chunks};
use gateway::server::upstreams::{
    UpstreamRegistry,
    config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const EMBED_MODEL: &str = "embed-test";

/// Stubbed embedding space with a built-in semantic drift: the query
/// ("osd op timeout") and the CRUSH distractors share one vector, while
/// the exact-identifier answer (`osd_op_timeout`) is orthogonal. Mirrors
/// the real-world miss where "osd timeout" pulls CRUSH tunables.
fn drift_vec(text: &str) -> [f32; 4] {
    let s = text.to_lowercase();
    // Underscored identifier first: it must NOT also count as the spaced
    // query phrase below.
    if s.contains("osd_op_timeout") {
        [0.0, 0.0, 0.0, 1.0]
    } else if s.contains("crush") || s.contains("osd op timeout") {
        [1.0, 0.0, 0.0, 0.0]
    } else {
        [0.25, 0.25, 0.25, 0.25]
    }
}

async fn start_embedding_upstream() -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
            let inputs = body
                .get("input")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let data: Vec<Value> = inputs
                .iter()
                .enumerate()
                .map(|(i, val)| {
                    let v = drift_vec(val.as_str().unwrap_or(""));
                    json!({ "object": "embedding", "index": i, "embedding": v })
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "model": EMBED_MODEL, "data": data,
            }))
        })
        .mount(&upstream)
        .await;
    upstream
}

fn registry_pointed_at(upstream_url: &str) -> Arc<UpstreamRegistry> {
    let mut pools = HashMap::new();
    pools.insert(
        "embed".to_string(),
        UpstreamPoolConfig {
            kind: PoolKind::Embedding,
            strategy: PickerStrategy::RoundRobin,
            models: Vec::new(),
            backend: vec![BackendConfig {
                name: "mock".into(),
                base_url: upstream_url.into(),
                api_key_env: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                models: Vec::new(),
            }],
        },
    );
    let registry = UpstreamRegistry::new(&pools).unwrap();
    registry
        .pools()
        .find(|p| p.name == "embed")
        .unwrap()
        .backends[0]
        .set_models(HashSet::from([EMBED_MODEL.to_string()]));
    registry
}

/// One corpus document: file path + chunk text. The vector is derived
/// from the text by `drift_vec`, same as the indexer would get from the
/// embedding upstream.
struct Doc {
    path: &'static str,
    content: &'static str,
}

/// Seed a ready primary ref from `docs` by hand (skip the git/clone path —
/// that's covered in tests/rag.rs). Returns the ready `CollectionRef`.
async fn seed_collection(
    indexer: &Indexer,
    reg: &UpstreamRegistry,
    docs: &[Doc],
) -> rag_db::CollectionRef {
    let central = indexer.db();
    let c = rag_db::create_collection(
        central,
        &rag_db::NewCollection {
            name: "ceph".into(),
            description: None,
            git_url: "https://example.invalid".into(),
            git_ref: "main".into(),
            pat: None,
            embedding_model: EMBED_MODEL.into(),
            include_globs: vec![],
            exclude_globs: vec![],
            chunk_size: 800,
            chunk_overlap: 100,
        },
    )
    .await
    .unwrap();
    let r = rag_db::add_ref(central, c.id, "main", true).await.unwrap();

    let store = indexer.collection_store(r.id, &r.data_uuid).await.unwrap();
    let idx = indexer.open_index(r.id, &r.data_uuid, Some(4)).unwrap();
    for (vid, d) in (1i64..).zip(docs.iter()) {
        let file_id = rag_db::upsert_file(&store, c.id, d.path, "hash")
            .await
            .unwrap();
        rag_db::insert_chunks(
            &store,
            c.id,
            &[rag_db::NewChunk {
                file_id,
                chunk_index: 0,
                start_line: 1,
                end_line: 1,
                content: d.content.into(),
                vector_id: vid,
            }],
        )
        .await
        .unwrap();
        let v = gateway::server::embeddings::embed(
            &reqwest::Client::new(),
            reg,
            EMBED_MODEL,
            &[d.content.to_string()],
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
        idx.add(vid, &v).unwrap();
    }
    drop(idx);
    rag_db::set_ref_status(central, r.id, rag_db::CollectionStatus::Indexing)
        .await
        .unwrap();
    rag_db::swap_ref_index(central, r.id, &r.data_uuid, "deadbeef")
        .await
        .unwrap();
    rag_db::find_ref_by_id(central, r.id)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn hybrid_recovers_exact_identifier_that_dense_only_misses() {
    let upstream = start_embedding_upstream().await;
    let reg = registry_pointed_at(&upstream.uri());
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let indexer = Indexer::new(
        pool.clone(),
        Arc::clone(&reg),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            ..IndexerConfig::default()
        },
    );

    // 3 CRUSH distractors (cluster with the query in vector space) + the
    // one chunk that actually answers it.
    let docs = [
        Doc {
            path: "src/crush/mapper.c",
            content: "crush choose_total_tries controls retry attempts during placement",
        },
        Doc {
            path: "src/crush/builder.c",
            content: "crush bucket weights influence the choose timeout-like retry budget",
        },
        Doc {
            path: "src/crush/CrushWrapper.h",
            content: "crush tunables: choose_local_tries, choose_local_fallback_tries",
        },
        Doc {
            path: "src/common/options/global.yaml.in",
            content: "name: osd_op_timeout desc: timeout for ops handled by osds default 0",
        },
    ];
    let r = seed_collection(&indexer, &reg, &docs).await;

    let query = "osd op timeout";
    let qvec = indexer.embed_query(EMBED_MODEL, query).await.unwrap();

    // Dense-only baseline: top-3 by vector distance. The drift stub puts
    // the query on top of the 3 CRUSH chunks, so the answer is shut out.
    let dense_top3: Vec<String> = indexer
        .open_index(r.id, &r.data_uuid, None)
        .unwrap()
        .search(&qvec, 3)
        .unwrap()
        .into_iter()
        .map(|(vid, _)| vid.to_string())
        .collect();
    // The answer's vector id is 4 (seeded last). Confirm dense-only misses it.
    assert!(
        !dense_top3.contains(&"4".to_string()),
        "precondition: dense-only top-3 should miss the answer (got {dense_top3:?})"
    );

    // Hybrid: the lexical side matches osd/op/timeout against
    // `osd_op_timeout` and RRF lifts it into the results.
    let hits = search_chunks(&indexer, &r, query, &qvec, 5).await.unwrap();
    let files: Vec<&str> = hits.iter().map(|(c, _)| c.file_path.as_str()).collect();
    assert!(
        files.contains(&"src/common/options/global.yaml.in"),
        "hybrid retrieval must surface the osd_op_timeout chunk; got {files:?}"
    );
}

#[tokio::test]
async fn lexical_alone_answers_when_vector_index_is_absent() {
    // Resilience: a collection whose usearch file never got built (or was
    // wiped) should still answer from the FTS side rather than 500.
    let upstream = start_embedding_upstream().await;
    let reg = registry_pointed_at(&upstream.uri());
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let indexer = Indexer::new(
        pool.clone(),
        Arc::clone(&reg),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            ..IndexerConfig::default()
        },
    );
    // Seed DB rows + FTS, but DON'T open/populate a usearch index.
    let c = rag_db::create_collection(
        &pool,
        &rag_db::NewCollection {
            name: "ceph".into(),
            description: None,
            git_url: "https://example.invalid".into(),
            git_ref: "main".into(),
            pat: None,
            embedding_model: EMBED_MODEL.into(),
            include_globs: vec![],
            exclude_globs: vec![],
            chunk_size: 800,
            chunk_overlap: 100,
        },
    )
    .await
    .unwrap();
    let r = rag_db::add_ref(&pool, c.id, "main", true).await.unwrap();
    let store = indexer.collection_store(r.id, &r.data_uuid).await.unwrap();
    let file_id = rag_db::upsert_file(&store, c.id, "src/common/options/global.yaml.in", "h")
        .await
        .unwrap();
    rag_db::insert_chunks(
        &store,
        c.id,
        &[rag_db::NewChunk {
            file_id,
            chunk_index: 0,
            start_line: 1,
            end_line: 1,
            content: "name: osd_op_timeout desc: timeout for ops handled by osds".into(),
            vector_id: 1,
        }],
    )
    .await
    .unwrap();

    let qvec = vec![0.0_f32; 4]; // no index to search anyway
    let hits = search_chunks(&indexer, &r, "osd op timeout", &qvec, 5)
        .await
        .unwrap();
    let files: Vec<&str> = hits.iter().map(|(c, _)| c.file_path.as_str()).collect();
    assert!(
        files.contains(&"src/common/options/global.yaml.in"),
        "lexical fallback must answer with no vector index; got {files:?}"
    );
}
