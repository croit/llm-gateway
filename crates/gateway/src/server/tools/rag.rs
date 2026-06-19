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
             generation. Each collection lists the indexed refs \
             (branches / tags / commits) you can search, and which is the \
             default. Pass a collection `name` to `rag_search` (and \
             optionally a `ref`) to query it.",
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
            let cols = rag_db::list_collections(indexer.db())
                .await
                .map_err(|e| ToolError::Failed(format!("listing collections: {e}")))?;
            let mut items: Vec<Value> = Vec::new();
            for c in &cols {
                let refs = rag_db::list_refs(indexer.db(), c.id)
                    .await
                    .map_err(|e| ToolError::Failed(format!("listing refs: {e}")))?;
                // Only advertise a queryable collection. Versioned: at least
                // one searchable ref. Aggregate: the PRIMARY ref (which holds
                // the single unified index) has finished its first build.
                let advertise = match c.search_mode {
                    rag_db::SearchMode::Aggregate => {
                        refs.iter().any(|r| r.is_primary && r.is_searchable())
                    }
                    rag_db::SearchMode::Versioned => refs.iter().any(|r| r.is_searchable()),
                };
                if !advertise {
                    continue;
                }
                let entry = match c.search_mode {
                    // Aggregate: present it as ONE searchable corpus. We do NOT
                    // enumerate the (possibly dozens of) source repos — listing
                    // them all tempts the model into searching them one-by-one.
                    // One rag_search with no `ref` already covers every source
                    // (it's a single unified index).
                    rag_db::SearchMode::Aggregate => json!({
                        "name": c.name,
                        "description": c.description,
                        "mode": "aggregate",
                        "sources": refs.len(),
                        "usage": "Search the WHOLE collection in a SINGLE rag_search call \
                                  with no `ref` — it is one unified index over all source \
                                  repos. Prefer one broad query; result paths are prefixed \
                                  with the source repo (e.g. `pve-manager/...`).",
                    }),
                    // Versioned: the refs ARE distinct versions, so list them —
                    // the caller needs to pick (or rely on the primary).
                    rag_db::SearchMode::Versioned => {
                        let ref_items: Vec<Value> = refs
                            .iter()
                            .map(|r| {
                                json!({
                                    "ref": r.git_ref,
                                    "primary": r.is_primary,
                                    "searchable": r.is_searchable(),
                                    "status": r.status.as_str(),
                                    "last_indexed_at": r.last_indexed_at.map(|t| t.to_string()),
                                })
                            })
                            .collect();
                        json!({
                            "name": c.name,
                            "description": c.description,
                            "mode": "versioned",
                            "refs": ref_items,
                        })
                    }
                };
                items.push(entry);
            }
            Ok(json!({ "collections": items }))
        })
    }
}

pub struct RagSearch;

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    collection: String,
    /// Which ref/source to search. Omitted → the collection's default: the
    /// primary ref (versioned collections) or all sources (aggregate). For
    /// aggregate collections this names a source repo (e.g. `qemu-server`);
    /// for versioned ones a branch/tag/commit. (`ref` is a Rust keyword.)
    #[serde(default, rename = "ref")]
    git_ref: Option<String>,
    #[serde(default)]
    top_k: Option<u32>,
}

const TOP_K_DEFAULT: u32 = 5;
/// Aggregate collections span many repos, so one search returns more than the
/// single-repo default — enough to cover several subsystems in a single call so
/// the model doesn't re-query once per aspect. Recall comes from the per-source
/// fan-out plus this larger merged result set.
const AGGREGATE_TOP_K_DEFAULT: u32 = 12;
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
             don't know which collections (and which of their refs) are \
             available. Returns the top-k matching chunks with file path, \
             line range, relevance score, and the chunk content. For an \
             aggregate collection, ONE call with no `ref` already searches \
             every source repo at once and merges the results. Prefer a SINGLE \
             broad, well-phrased query covering the whole question and answer \
             from the merged hits — most questions need only one or two \
             searches. Do not decompose into many narrow per-aspect queries or \
             search once per repo; only search again for a genuinely different \
             topic the first results missed (raise `top_k` if you need more \
             depth, up to 25).",
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
                    "ref": {
                        "type": "string",
                        "description": "Which ref/source to search. Omit to search \
                                        the collection's default — its primary ref, \
                                        or for an aggregate collection ALL of its \
                                        sources at once. For an aggregate collection \
                                        this names one source repo (e.g. \
                                        `qemu-server`); for a versioned one a branch \
                                        / tag / commit. See `rag_list_collections`."
                    },
                    "top_k": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": TOP_K_MAX,
                        "description": "How many results to return. Defaults to 5 \
                                        (12 for aggregate collections); max 25."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: SearchArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{query: string, collection: string, ref?: string, \
                     top_k?: integer}}: {e}"
                ))
            })?;
            let indexer = ctx
                .indexer
                .as_ref()
                .ok_or_else(|| ToolError::Failed("RAG is not configured on this gateway".into()))?;
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

            // Aggregate collections default to a larger result set (they span
            // many repos, and one search covers them all); an explicit
            // caller-supplied `top_k` always wins.
            let default_k = match collection.search_mode {
                rag_db::SearchMode::Aggregate => AGGREGATE_TOP_K_DEFAULT,
                rag_db::SearchMode::Versioned => TOP_K_DEFAULT,
            };
            let top_k = args.top_k.unwrap_or(default_k).clamp(1, TOP_K_MAX) as usize;

            // Resolve the single store to query. Versioned: the named ref or
            // the primary. Aggregate: the primary ref holds the collection's
            // ONE unified index (built from every source), so we query it
            // directly — one global dense + lexical ranking. Hit paths are
            // prefixed with the source repo (e.g. `pve-manager/...`).
            let rref = resolve_search(indexer.db(), &collection, args.git_ref.as_deref()).await?;

            // Asymmetric query embedding (instruction-prefixed); documents
            // were embedded bare at index time. See `Indexer::embed_query`.
            let query_vec = indexer
                .embed_query(&collection.embedding_model, &args.query)
                .await
                .map_err(|e| ToolError::Failed(format!("embedding query: {e}")))?;

            let hits = worker::search_chunks(indexer, &rref, &args.query, &query_vec, top_k)
                .await
                .map_err(|e| ToolError::Failed(format!("searching index: {e}")))?;
            let results: Vec<Value> = hits.into_iter().map(hit_json).collect();
            Ok(json!({
                "collection": collection.name,
                "ref": rref.git_ref,
                "hits": results,
            }))
        })
    }
}

/// Render one search hit. The `score` is hybrid (dense + lexical)
/// reciprocal-rank-fusion relevance — relative ordering only, not an
/// absolute similarity.
fn hit_json((chunk, score): (rag_db::Chunk, f32)) -> Value {
    json!({
        "file_path": chunk.file_path,
        "start_line": chunk.start_line,
        "end_line": chunk.end_line,
        "score": score,
        "content": chunk.content,
    })
}

/// Resolve the single ref whose store `rag_search` queries.
/// * Versioned: the named ref, else the primary.
/// * Aggregate: always the primary ref — it holds the collection's one
///   unified index (built from every source), so a single query ranks across
///   the whole corpus. A caller-supplied `ref` is ignored in aggregate mode
///   (the index is merged; hit paths carry the source-repo prefix instead).
async fn resolve_search(
    db: &crate::server::db::Pool,
    collection: &rag_db::Collection,
    git_ref: Option<&str>,
) -> Result<rag_db::CollectionRef, ToolError> {
    use rag_db::SearchMode;
    let rref = match (collection.search_mode, git_ref) {
        (SearchMode::Versioned, Some(r)) => rag_db::find_ref(db, collection.id, r)
            .await
            .map_err(|e| ToolError::Failed(format!("looking up ref: {e}")))?
            .ok_or_else(|| {
                ToolError::Failed(format!(
                    "collection `{}` has no ref `{}` — call rag_list_collections to see \
                     its available refs",
                    collection.name, r
                ))
            })?,
        // Versioned default OR aggregate (unified index lives on the primary).
        _ => rag_db::primary_ref(db, collection.id)
            .await
            .map_err(|e| ToolError::Failed(format!("looking up primary ref: {e}")))?
            .ok_or_else(|| {
                ToolError::Failed(format!(
                    "collection `{}` has no indexed {} yet",
                    collection.name,
                    if collection.search_mode == SearchMode::Aggregate {
                        "sources"
                    } else {
                        "refs"
                    }
                ))
            })?,
    };
    if !rref.is_searchable() {
        return Err(ToolError::Failed(format!(
            "collection `{}` is not ready yet (status = {}); its first index hasn't \
             completed — wait for it or re-queue if it failed",
            collection.name,
            rref.status.as_str()
        )));
    }
    Ok(rref)
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
                compliance: Default::default(),
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
        let c = rag_db::create_collection(
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
                search_mode: rag_db::SearchMode::Versioned,
            },
        )
        .await
        .unwrap();
        // A collection with no searchable ref is not advertised.
        let out = RagListCollections
            .run(ctx_with(indexer.clone()), json!({}))
            .await
            .unwrap();
        assert!(out["collections"].as_array().unwrap().is_empty());

        // Add a ref and bring it to ready → now listed with its refs.
        let r = rag_db::add_ref(&pool, c.id, "reef", None, true)
            .await
            .unwrap();
        rag_db::set_ref_status(&pool, r.id, rag_db::CollectionStatus::Indexing)
            .await
            .unwrap();
        rag_db::swap_ref_index(&pool, r.id, &r.data_uuid, "deadbeef")
            .await
            .unwrap();

        let out = RagListCollections
            .run(ctx_with(indexer), json!({}))
            .await
            .unwrap();
        let cs = out["collections"].as_array().unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0]["name"], "demo");
        let refs = cs[0]["refs"].as_array().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0]["ref"], "reef");
        assert_eq!(refs[0]["primary"], true);
        assert_eq!(refs[0]["searchable"], true);
    }

    #[tokio::test]
    async fn list_collections_summarises_aggregate_without_enumerating_sources() {
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
        let c = rag_db::create_collection(
            &pool,
            &rag_db::NewCollection {
                name: "proxmox".into(),
                description: Some("all repos".into()),
                git_url: "https://example.invalid/default.git".into(),
                git_ref: "master".into(),
                pat: None,
                embedding_model: "embed-test".into(),
                include_globs: vec![],
                exclude_globs: vec![],
                chunk_size: 100,
                chunk_overlap: 10,
                search_mode: rag_db::SearchMode::Aggregate,
            },
        )
        .await
        .unwrap();
        // Two sources; the first is primary and holds the (built) unified
        // index — that's the gate for advertising an aggregate collection.
        for (i, url) in ["https://x/pve-manager.git", "https://x/qemu-server.git"]
            .iter()
            .enumerate()
        {
            let r = rag_db::add_ref(&pool, c.id, "master", Some(url), i == 0)
                .await
                .unwrap();
            if i == 0 {
                rag_db::set_ref_status(&pool, r.id, rag_db::CollectionStatus::Indexing)
                    .await
                    .unwrap();
                rag_db::swap_ref_index(&pool, r.id, &r.data_uuid, "sha")
                    .await
                    .unwrap();
            }
        }

        let out = RagListCollections
            .run(ctx_with(indexer), json!({}))
            .await
            .unwrap();
        let cs = out["collections"].as_array().unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0]["mode"], "aggregate");
        assert_eq!(cs[0]["sources"], 2);
        // The whole point: an aggregate collection is NOT enumerated source by
        // source, so the model searches it in one call instead of looping.
        assert!(
            cs[0].get("refs").is_none(),
            "aggregate collection must not enumerate its sources"
        );
        assert!(
            cs[0]["usage"].as_str().unwrap().contains("SINGLE"),
            "usage hint should steer the model to a single combined search"
        );
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
                search_mode: rag_db::SearchMode::Versioned,
            },
        )
        .await
        .unwrap();
        // Each ref owns its store; add a primary ref and seed it by hand.
        let r = rag_db::add_ref(&pool, c.id, "main", None, true)
            .await
            .unwrap();
        let store = indexer.collection_store(r.id, &r.data_uuid).await.unwrap();
        let f = rag_db::upsert_file(&store, c.id, "src/alpha.rs", "hashA")
            .await
            .unwrap();
        rag_db::insert_chunks(
            &store,
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
        let idx = indexer.open_index(r.id, &r.data_uuid, Some(4)).unwrap();
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
        drop(idx);
        // Bring the ref to `ready` on its current store so it's searchable.
        rag_db::set_ref_status(&pool, r.id, rag_db::CollectionStatus::Indexing)
            .await
            .unwrap();
        rag_db::swap_ref_index(&pool, r.id, &r.data_uuid, "deadbeef")
            .await
            .unwrap();
        let r = rag_db::find_ref_by_id(&pool, r.id).await.unwrap().unwrap();

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
        let raw = search_chunks(&indexer, &r, "alpha please", &q, 5)
            .await
            .unwrap();
        assert!(!raw.is_empty(), "lower layer returned no hits");

        let out = RagSearch
            .run(
                ctx_with(indexer),
                json!({ "query": "alpha please", "collection": "code", "top_k": 3 }),
            )
            .await
            .unwrap();
        assert_eq!(out["collection"], "code");
        assert_eq!(out["ref"], "main");
        let hits = out["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["file_path"], "src/alpha.rs");
        assert_eq!(hits[0]["start_line"], 1);
        assert_eq!(hits[0]["content"], "alpha alpha");
    }

    #[tokio::test]
    async fn aggregate_search_uses_one_unified_index() {
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
        let c = rag_db::create_collection(
            &pool,
            &rag_db::NewCollection {
                name: "proxmox".into(),
                description: None,
                git_url: "https://example.invalid/default.git".into(),
                git_ref: "master".into(),
                pat: None,
                embedding_model: "embed-test".into(),
                include_globs: vec![],
                exclude_globs: vec![],
                chunk_size: 100,
                chunk_overlap: 10,
                search_mode: rag_db::SearchMode::Aggregate,
            },
        )
        .await
        .unwrap();

        // The PRIMARY ref holds the single unified index. Seed its store with
        // chunks from two "sources", paths prefixed with the source repo —
        // exactly the shape `build_ref` produces for an aggregate collection.
        let primary = rag_db::add_ref(&pool, c.id, "master", Some("https://x/all.git"), true)
            .await
            .unwrap();
        let store = indexer
            .collection_store(primary.id, &primary.data_uuid)
            .await
            .unwrap();
        let idx = indexer
            .open_index(primary.id, &primary.data_uuid, Some(4))
            .unwrap();
        let docs = [
            ("pve-manager/PVE/Manager.pm", "alpha alpha", 1i64),
            ("qemu-server/PVE/QemuServer.pm", "beta beta", 2i64),
        ];
        for (path, content, vid) in docs {
            let f = rag_db::upsert_file(&store, c.id, path, "h").await.unwrap();
            rag_db::insert_chunks(
                &store,
                c.id,
                &[rag_db::NewChunk {
                    file_id: f,
                    chunk_index: 0,
                    start_line: 1,
                    end_line: 2,
                    content: content.into(),
                    vector_id: vid,
                }],
            )
            .await
            .unwrap();
            let v = embeddings::embed(
                &reqwest::Client::new(),
                &reg,
                "embed-test",
                &[content.to_string()],
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
            idx.add(vid, &v).unwrap();
        }
        drop(idx);
        rag_db::set_ref_status(&pool, primary.id, rag_db::CollectionStatus::Indexing)
            .await
            .unwrap();
        rag_db::swap_ref_index(&pool, primary.id, &primary.data_uuid, "sha")
            .await
            .unwrap();

        // ONE query over the unified index — `alpha` ranks the pve-manager
        // chunk first, and the hit path is prefixed with its source repo.
        let out = RagSearch
            .run(
                ctx_with(indexer),
                json!({ "query": "alpha please", "collection": "proxmox", "top_k": 5 }),
            )
            .await
            .unwrap();
        assert_eq!(out["collection"], "proxmox");
        let hits = out["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "unified search returned no hits");
        assert_eq!(hits[0]["file_path"], "pve-manager/PVE/Manager.pm");
        assert!(hits[0]["content"].as_str().unwrap().contains("alpha"));
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
        let c = rag_db::create_collection(
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
                search_mode: rag_db::SearchMode::Versioned,
            },
        )
        .await
        .unwrap();
        // A primary ref exists but hasn't completed its first index.
        rag_db::add_ref(&pool, c.id, "main", None, true)
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
