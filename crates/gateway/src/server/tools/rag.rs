// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! RAG tools — the model-facing surface of the indexer.
//!
//! Two tools land here:
//!
//!   * [`RagListCollections`] — a discovery call the model can use to
//!     find out which codebases (and other corpora) the operator has
//!     indexed. Returns name + description + status so the model can
//!     tell ready-to-search from still-indexing.
//!
//!   * [`RagSearch`] — the actual retrieval. Embeds the query through
//!     the collection's configured embedding model, runs a k-NN search
//!     against the per-collection usearch index, joins back to the
//!     SQLite metadata for provenance, and hands the model a list of
//!     `{file, lines, content, score}` records.
//!
//! Both tools fail cleanly when `ToolContext::indexer` is `None`
//! (deployment hasn't wired the indexer up) — a model that gets that
//! error can pivot to other tools rather than retry forever.

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};
use crate::server::db::rag as rag_db;
use crate::server::rag::worker;

pub struct RagListCollections;

impl Tool for RagListCollections {
    fn id(&self) -> &str {
        "rag_list_collections"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "List the codebases / corpora available for retrieval-augmented \
             generation. Returns each collection's name, description, and \
             readiness — pass a `name` back to `rag_search` to query it.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, _args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let indexer = ctx
                .indexer
                .as_ref()
                .ok_or_else(|| ToolError::Failed("RAG is not configured on this gateway".into()))?;
            let rows = rag_db::list_collections(indexer.db())
                .await
                .map_err(|e| ToolError::Failed(format!("listing collections: {e}")))?;
            let items: Vec<Value> = rows
                .iter()
                .map(|c| {
                    json!({
                        "name": c.name,
                        "description": c.description,
                        "status": c.status.as_str(),
                        "last_indexed_at": c.last_indexed_at.map(|t| t.to_string()),
                    })
                })
                .collect();
            Ok(json!({ "collections": items }))
        })
    }
}

pub struct RagSearch;

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    collection: String,
    #[serde(default)]
    top_k: Option<u32>,
}

const TOP_K_DEFAULT: u32 = 5;
const TOP_K_MAX: u32 = 25;

impl Tool for RagSearch {
    fn id(&self) -> &str {
        "rag_search"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Search an indexed codebase or corpus for passages relevant to a \
             natural-language query. Call `rag_list_collections` first if you \
             don't know which collections are available. Returns the top-k \
             matching chunks with file path, line range, similarity score, \
             and the chunk content.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["query", "collection"],
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural-language description of what you're looking for."
                    },
                    "collection": {
                        "type": "string",
                        "description": "Name of the indexed collection to search. \
                                        Get the list with `rag_list_collections`."
                    },
                    "top_k": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": TOP_K_MAX,
                        "description": "How many results to return. Defaults to 5; max 25."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: SearchArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{query: string, collection: string, top_k?: integer}}: {e}"
                ))
            })?;
            let indexer = ctx
                .indexer
                .as_ref()
                .ok_or_else(|| ToolError::Failed("RAG is not configured on this gateway".into()))?;
            let top_k = args.top_k.unwrap_or(TOP_K_DEFAULT).clamp(1, TOP_K_MAX) as usize;

            let collection = rag_db::find_collection_by_name(indexer.db(), &args.collection)
                .await
                .map_err(|e| ToolError::Failed(format!("looking up collection: {e}")))?
                .ok_or_else(|| {
                    ToolError::Failed(format!(
                        "no RAG collection named `{}` — call rag_list_collections to discover \
                         which collections this gateway has indexed",
                        args.collection
                    ))
                })?;
            if collection.status != rag_db::CollectionStatus::Ready {
                return Err(ToolError::Failed(format!(
                    "collection `{}` is not ready (status = {}); ask the operator to wait for \
                     indexing to complete or to re-queue if it failed",
                    collection.name,
                    collection.status.as_str()
                )));
            }

            let query_vec = indexer
                .embed_one(&collection.embedding_model, &args.query)
                .await
                .map_err(|e| ToolError::Failed(format!("embedding query: {e}")))?;
            let hits = worker::search_chunks(indexer, collection.id, &query_vec, top_k)
                .await
                .map_err(|e| ToolError::Failed(format!("searching index: {e}")))?;

            let results: Vec<Value> = hits
                .into_iter()
                .map(|(chunk, distance)| {
                    json!({
                        "file_path": chunk.file_path,
                        "start_line": chunk.start_line,
                        "end_line": chunk.end_line,
                        // usearch's `Cos` metric returns cosine *distance*
                        // (smaller = closer). Surface a similarity in
                        // [-1, 1] so the model has a familiar shape.
                        "similarity": 1.0 - distance,
                        "content": chunk.content,
                    })
                })
                .collect();
            Ok(json!({
                "collection": collection.name,
                "hits": results,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;
    use crate::server::embeddings;
    use crate::server::rag::worker::{Indexer, IndexerConfig, search_chunks};
    use crate::server::upstreams::{
        UpstreamRegistry,
        config::{BackendConfig, PickerStrategy, PoolKind, UpstreamPoolConfig},
    };
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use wiremock::matchers::{method, path as wpath};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// One-hot vectors keyed on the literal substring of the input —
    /// matches the integration-test scaffolding in `tests/rag.rs`.
    fn one_hot(input: &str) -> [f32; 4] {
        let s = input.to_lowercase();
        if s.contains("alpha") {
            [1.0, 0.0, 0.0, 0.0]
        } else if s.contains("beta") {
            [0.0, 1.0, 0.0, 0.0]
        } else {
            [0.5, 0.5, 0.5, 0.5]
        }
    }

    async fn embedding_upstream() -> MockServer {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wpath("/embeddings"))
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
                        let s = val.as_str().unwrap_or("");
                        let v = one_hot(s);
                        json!({"object": "embedding", "index": i, "embedding": v})
                    })
                    .collect();
                ResponseTemplate::new(200).set_body_json(json!({
                    "object": "list",
                    "model": "embed-test",
                    "data": data,
                }))
            })
            .mount(&upstream)
            .await;
        upstream
    }

    fn registry(upstream_url: &str) -> Arc<UpstreamRegistry> {
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
        let r = UpstreamRegistry::new(&pools).unwrap();
        let pool = r.pools().find(|p| p.name == "embed").unwrap();
        pool.backends[0].set_models(HashSet::from(["embed-test".to_string()]));
        r
    }

    fn ctx_with(indexer: Indexer) -> ToolContext {
        ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: indexer.db().clone(),
            s3: None,
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: Some(indexer),
        }
    }

    fn ctx_without_indexer(pool: db::Pool) -> ToolContext {
        ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: pool,
            s3: None,
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
        }
    }

    #[tokio::test]
    async fn list_collections_shows_status() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let upstream = embedding_upstream().await;
        let indexer = Indexer::new(
            pool.clone(),
            registry(&upstream.uri()),
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
                ..IndexerConfig::default()
            },
        );
        rag_db::create_collection(
            &pool,
            &rag_db::NewCollection {
                name: "demo".into(),
                description: Some("a demo".into()),
                git_url: "https://example.invalid/repo".into(),
                git_ref: "main".into(),
                pat: None,
                embedding_model: "embed-test".into(),
                include_globs: vec![],
                exclude_globs: vec![],
                chunk_size: 100,
                chunk_overlap: 10,
            },
        )
        .await
        .unwrap();
        let out = RagListCollections
            .run(ctx_with(indexer), json!({}))
            .await
            .unwrap();
        let cs = out["collections"].as_array().unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0]["name"], "demo");
        assert_eq!(cs[0]["status"], "pending");
    }

    #[tokio::test]
    async fn list_collections_without_indexer_is_clear_error() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let err = RagListCollections
            .run(ctx_without_indexer(pool), json!({}))
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("RAG is not configured")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_against_ready_collection_returns_provenance() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let upstream = embedding_upstream().await;
        let reg = registry(&upstream.uri());
        let indexer = Indexer::new(
            pool.clone(),
            Arc::clone(&reg),
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
                ..IndexerConfig::default()
            },
        );

        // Seed the DB + index by hand (avoid the git path here — the
        // integration test in tests/rag.rs covers that end-to-end).
        let c = rag_db::create_collection(
            &pool,
            &rag_db::NewCollection {
                name: "code".into(),
                description: None,
                git_url: "https://example.invalid".into(),
                git_ref: "main".into(),
                pat: None,
                embedding_model: "embed-test".into(),
                include_globs: vec![],
                exclude_globs: vec![],
                chunk_size: 100,
                chunk_overlap: 10,
            },
        )
        .await
        .unwrap();
        let f = rag_db::upsert_file(&pool, c.id, "src/alpha.rs", "hashA")
            .await
            .unwrap();
        rag_db::insert_chunks(
            &pool,
            c.id,
            &[rag_db::NewChunk {
                file_id: f,
                chunk_index: 0,
                start_line: 1,
                end_line: 5,
                content: "alpha alpha".into(),
                vector_id: 1,
            }],
        )
        .await
        .unwrap();
        // Open the index, push the matching vector.
        let idx = indexer.open_index(c.id, Some(4)).unwrap();
        let v = embeddings::embed(
            &reqwest::Client::new(),
            &reg,
            "embed-test",
            &["alpha alpha".to_string()],
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
        idx.add(1, &v).unwrap();
        // Drop manually so we can search via the cached handle.
        drop(idx);
        rag_db::mark_indexed(&pool, c.id, "deadbeef").await.unwrap();

        // Sanity-check the lower layer first so a search-tool failure
        // doesn't get blamed on the index plumbing.
        let q = embeddings::embed(
            &reqwest::Client::new(),
            &reg,
            "embed-test",
            &["alpha please".to_string()],
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
        let raw = search_chunks(&indexer, c.id, &q, 5).await.unwrap();
        assert!(!raw.is_empty(), "lower layer returned no hits");

        let out = RagSearch
            .run(
                ctx_with(indexer),
                json!({ "query": "alpha please", "collection": "code", "top_k": 3 }),
            )
            .await
            .unwrap();
        assert_eq!(out["collection"], "code");
        let hits = out["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["file_path"], "src/alpha.rs");
        assert_eq!(hits[0]["start_line"], 1);
        assert_eq!(hits[0]["content"], "alpha alpha");
    }

    #[tokio::test]
    async fn search_rejects_not_ready_collection_with_status_hint() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let upstream = embedding_upstream().await;
        let indexer = Indexer::new(
            pool.clone(),
            registry(&upstream.uri()),
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
                ..IndexerConfig::default()
            },
        );
        rag_db::create_collection(
            &pool,
            &rag_db::NewCollection {
                name: "still-pending".into(),
                description: None,
                git_url: "https://e.invalid".into(),
                git_ref: "main".into(),
                pat: None,
                embedding_model: "embed-test".into(),
                include_globs: vec![],
                exclude_globs: vec![],
                chunk_size: 100,
                chunk_overlap: 10,
            },
        )
        .await
        .unwrap();
        let err = RagSearch
            .run(
                ctx_with(indexer),
                json!({"query": "x", "collection": "still-pending"}),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => {
                assert!(msg.contains("not ready"), "{msg}");
                assert!(msg.contains("pending"), "{msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_rejects_unknown_collection_with_discovery_hint() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let upstream = embedding_upstream().await;
        let indexer = Indexer::new(
            pool.clone(),
            registry(&upstream.uri()),
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
                ..IndexerConfig::default()
            },
        );
        let err = RagSearch
            .run(
                ctx_with(indexer),
                json!({"query": "x", "collection": "no-such-thing"}),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("rag_list_collections"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn schema_ids_match() {
        assert_eq!(
            RagListCollections.id(),
            RagListCollections.schema().function.name
        );
        assert_eq!(RagSearch.id(), RagSearch.schema().function.name);
    }
}
