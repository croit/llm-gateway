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
                // Only advertise collections with at least one searchable
                // ref — a collection still building its first index isn't
                // queryable yet.
                if !refs.iter().any(|r| r.is_searchable()) {
                    continue;
                }
                let ref_items: Vec<Value> = refs
                    .iter()
                    .map(|r| {
                        json!({
                            "ref": r.git_ref,
                            // For aggregate collections this is the source repo
                            // (e.g. `qemu-server`) you can pass as `ref` to
                            // scope a search to one component.
                            "source": r.source_label(c),
                            "primary": r.is_primary,
                            "searchable": r.is_searchable(),
                            "status": r.status.as_str(),
                            "last_indexed_at": r.last_indexed_at.map(|t| t.to_string()),
                        })
                    })
                    .collect();
                items.push(json!({
                    "name": c.name,
                    "description": c.description,
                    // `aggregate`: rag_search with no `ref` searches ALL sources
                    // at once (one corpus). `versioned`: it uses the primary ref.
                    "mode": c.search_mode.as_str(),
                    "refs": ref_items,
                }));
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
             line range, relevance score, and the chunk content.",
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
                    "expected {{query: string, collection: string, ref?: string, \
                     top_k?: integer}}: {e}"
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

            // Resolve which source(s) to search. Versioned collections
            // search exactly one ref (named or primary); aggregate ones fan
            // out across all searchable sources (or the named source).
            let plan = resolve_search(indexer.db(), &collection, args.git_ref.as_deref()).await?;

            // Asymmetric query embedding (instruction-prefixed); documents
            // were embedded bare at index time. See `Indexer::embed_query`.
            // Embedded ONCE and reused across every source in the fan-out.
            let query_vec = indexer
                .embed_query(&collection.embedding_model, &args.query)
                .await
                .map_err(|e| ToolError::Failed(format!("embedding query: {e}")))?;

            match plan {
                SearchPlan::Single(rref) => {
                    let hits =
                        worker::search_chunks(indexer, &rref, &args.query, &query_vec, top_k)
                            .await
                            .map_err(|e| ToolError::Failed(format!("searching index: {e}")))?;
                    let results: Vec<Value> = hits.into_iter().map(hit_json).collect();
                    Ok(json!({
                        "collection": collection.name,
                        "ref": rref.git_ref,
                        "hits": results,
                    }))
                }
                SearchPlan::Aggregate(refs) => {
                    // Search every source, tag each hit with its origin repo,
                    // then merge into one global top-k by fused score.
                    let mut all: Vec<(String, String, rag_db::Chunk, f32)> = Vec::new();
                    for r in &refs {
                        let label = r.source_label(&collection);
                        let hits =
                            worker::search_chunks(indexer, r, &args.query, &query_vec, top_k)
                                .await
                                .map_err(|e| {
                                    ToolError::Failed(format!("searching `{label}`: {e}"))
                                })?;
                        for (chunk, score) in hits {
                            all.push((label.clone(), r.git_ref.clone(), chunk, score));
                        }
                    }
                    all.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
                    all.truncate(top_k);
                    let results: Vec<Value> = all
                        .into_iter()
                        .map(|(source, git_ref, chunk, score)| {
                            json!({
                                "source": source,
                                "ref": git_ref,
                                "file_path": chunk.file_path,
                                "start_line": chunk.start_line,
                                "end_line": chunk.end_line,
                                "score": score,
                                "content": chunk.content,
                            })
                        })
                        .collect();
                    Ok(json!({
                        "collection": collection.name,
                        "mode": "aggregate",
                        "sources_searched": refs.len(),
                        "hits": results,
                    }))
                }
            }
        })
    }
}

/// What `rag_search` will actually query, after resolving collection mode
/// and the caller's optional `ref`.
enum SearchPlan {
    /// One ref (versioned: named or primary).
    Single(rag_db::CollectionRef),
    /// Many sources merged into one ranking (aggregate: all, or the
    /// source(s) matching a named repo). Always ≥1 entry.
    Aggregate(Vec<rag_db::CollectionRef>),
}

/// Render one versioned-search hit. The `score` is hybrid (dense + lexical)
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

async fn resolve_search(
    db: &crate::server::db::Pool,
    collection: &rag_db::Collection,
    git_ref: Option<&str>,
) -> Result<SearchPlan, ToolError> {
    use rag_db::SearchMode;
    let not_ready = |r: &rag_db::CollectionRef| {
        ToolError::Failed(format!(
            "ref `{}` of `{}` is not ready yet (status = {}); its first index hasn't \
             completed — wait for it or re-queue if it failed",
            r.git_ref,
            collection.name,
            r.status.as_str()
        ))
    };
    match (git_ref, collection.search_mode) {
        // Versioned: exactly one ref, named or primary.
        (Some(r), SearchMode::Versioned) => {
            let rref = rag_db::find_ref(db, collection.id, r)
                .await
                .map_err(|e| ToolError::Failed(format!("looking up ref: {e}")))?
                .ok_or_else(|| {
                    ToolError::Failed(format!(
                        "collection `{}` has no ref `{}` — call rag_list_collections to see \
                         its available refs",
                        collection.name, r
                    ))
                })?;
            if !rref.is_searchable() {
                return Err(not_ready(&rref));
            }
            Ok(SearchPlan::Single(rref))
        }
        (None, SearchMode::Versioned) => {
            let rref = rag_db::primary_ref(db, collection.id)
                .await
                .map_err(|e| ToolError::Failed(format!("looking up primary ref: {e}")))?
                .ok_or_else(|| {
                    ToolError::Failed(format!(
                        "collection `{}` has no indexed refs yet",
                        collection.name
                    ))
                })?;
            if !rref.is_searchable() {
                return Err(not_ready(&rref));
            }
            Ok(SearchPlan::Single(rref))
        }
        // Aggregate: all searchable sources, or those matching a named repo.
        (maybe_named, SearchMode::Aggregate) => {
            let refs = rag_db::searchable_refs(db, collection.id)
                .await
                .map_err(|e| ToolError::Failed(format!("listing sources: {e}")))?;
            let refs = match maybe_named {
                Some(name) => refs
                    .into_iter()
                    .filter(|x| x.source_label(collection) == name || x.git_ref == name)
                    .collect::<Vec<_>>(),
                None => refs,
            };
            if refs.is_empty() {
                return Err(ToolError::Failed(match maybe_named {
                    Some(name) => format!(
                        "collection `{}` has no searchable source matching `{}` — call \
                         rag_list_collections to see its sources",
                        collection.name, name
                    ),
                    None => format!(
                        "collection `{}` has no searchable sources yet",
                        collection.name
                    ),
                }));
            }
            Ok(SearchPlan::Aggregate(refs))
        }
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
    async fn aggregate_search_fans_out_across_sources() {
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

        // Seed two different repos on the same branch, each in its own store.
        // (url, file, content) → content's one_hot vector is its dense rep.
        let sources = [
            ("https://x/pve-manager.git", "PVE/Manager.pm", "alpha alpha"),
            (
                "https://x/qemu-server.git",
                "PVE/QemuServer.pm",
                "beta beta",
            ),
        ];
        for (url, file, content) in sources {
            let r = rag_db::add_ref(&pool, c.id, "master", Some(url), false)
                .await
                .unwrap();
            let store = indexer.collection_store(r.id, &r.data_uuid).await.unwrap();
            let f = rag_db::upsert_file(&store, c.id, file, "h").await.unwrap();
            rag_db::insert_chunks(
                &store,
                c.id,
                &[rag_db::NewChunk {
                    file_id: f,
                    chunk_index: 0,
                    start_line: 1,
                    end_line: 2,
                    content: content.into(),
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
                &[content.to_string()],
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
            idx.add(1, &v).unwrap();
            drop(idx);
            rag_db::set_ref_status(&pool, r.id, rag_db::CollectionStatus::Indexing)
                .await
                .unwrap();
            rag_db::swap_ref_index(&pool, r.id, &r.data_uuid, "sha")
                .await
                .unwrap();
        }

        // No ref → fan out over ALL sources, merged into one ranking. The
        // alpha query ranks the pve-manager chunk top; each hit is tagged
        // with its origin repo.
        let out = RagSearch
            .run(
                ctx_with(indexer.clone()),
                json!({ "query": "alpha please", "collection": "proxmox", "top_k": 5 }),
            )
            .await
            .unwrap();
        assert_eq!(out["mode"], "aggregate");
        assert_eq!(out["sources_searched"], 2);
        let hits = out["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "aggregate search returned no hits");
        assert_eq!(hits[0]["source"], "pve-manager");
        assert_eq!(hits[0]["ref"], "master");
        assert!(hits[0]["content"].as_str().unwrap().contains("alpha"));
        assert!(
            hits.iter()
                .all(|h| h.get("source").and_then(Value::as_str).is_some()),
            "every aggregate hit must carry a source label"
        );

        // A named source scopes the fan-out to just that repo.
        let out = RagSearch
            .run(
                ctx_with(indexer),
                json!({ "query": "beta please", "collection": "proxmox", "ref": "qemu-server" }),
            )
            .await
            .unwrap();
        assert_eq!(out["sources_searched"], 1);
        let hits = out["hits"].as_array().unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h["source"] == "qemu-server"));
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
