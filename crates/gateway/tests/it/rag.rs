// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! End-to-end test for the RAG indexer pipeline.
//!
//! Stands up:
//!   * a fixture git repo (init + commit two text files in a tempdir),
//!   * a wiremock-backed "embedding" upstream whose response is
//!     deterministic per input — strings containing `alpha` map to
//!     `[1,0,0,0]`, strings containing `beta` map to `[0,1,0,0]`, etc.
//!     so an "alpha" query unambiguously returns alpha chunks,
//!   * an in-memory SQLite with the rag migration applied,
//!   * an [`Indexer`] wired to all three.
//!
//! Then drives `index_one` and `search_chunks` and asserts the
//! provenance round-trips correctly.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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

fn one_hot(input: &str) -> [f32; 4] {
    let s = input.to_lowercase();
    if s.contains("alpha") {
        [1.0, 0.0, 0.0, 0.0]
    } else if s.contains("beta") {
        [0.0, 1.0, 0.0, 0.0]
    } else if s.contains("gamma") {
        [0.0, 0.0, 1.0, 0.0]
    } else {
        [0.5, 0.5, 0.5, 0.5]
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
                    let s = val.as_str().unwrap_or("");
                    let v = one_hot(s);
                    json!({
                        "object": "embedding",
                        "index": i,
                        "embedding": v,
                    })
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

fn registry_pointed_at(upstream_url: &str) -> Arc<UpstreamRegistry> {
    let mut pools = HashMap::new();
    pools.insert(
        "embed".to_string(),
        UpstreamPoolConfig {
            voices: Default::default(),
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

/// Initialise a small git repo with two text files. Returns `None` if
/// system `git` isn't on PATH — the indexer is git-shell-dependent, so
/// without `git` the rest of the test is meaningless.
fn fixture_repo() -> Option<tempfile::TempDir> {
    let dir = tempdir().unwrap();
    let p = dir.path();
    let init = std::process::Command::new("git")
        .args(["init", "-q", "-b", "main", "."])
        .current_dir(p)
        .output();
    let Ok(init) = init else { return None };
    if !init.status.success() {
        return None;
    }
    for args in [
        &["config", "user.email", "t@example.invalid"][..],
        &["config", "user.name", "t"][..],
        &["config", "commit.gpgsign", "false"][..],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(p)
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(p.join("alpha.txt"), b"alpha alpha alpha alpha\n").unwrap();
    std::fs::write(p.join("beta.txt"), b"beta beta beta beta\n").unwrap();
    std::fs::write(
        p.join("README.md"),
        b"# project\n\nignored by include glob.\n",
    )
    .unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(p)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args(["commit", "-q", "-m", "seed"])
            .current_dir(p)
            .status()
            .unwrap()
            .success()
    );
    Some(dir)
}

#[tokio::test]
async fn indexer_clones_chunks_embeds_then_search_returns_right_chunk() {
    let Some(repo) = fixture_repo() else {
        eprintln!("git not on PATH — skipping");
        return;
    };
    let upstream = start_embedding_upstream().await;
    let registry = registry_pointed_at(&upstream.uri());
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let indexer = Indexer::new(
        pool.clone(),
        Arc::clone(&registry),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..IndexerConfig::default()
        },
    );

    let collection = rag_db::create_collection(
        &pool,
        &rag_db::NewCollection {
            name: "fixture".into(),
            description: None,
            git_url: repo.path().to_string_lossy().to_string(),
            git_ref: "main".into(),
            pat: None,
            embedding_model: "embed-test".into(),
            include_globs: vec!["*.txt".into()],
            exclude_globs: Vec::new(),
            chunk_size: 80,
            chunk_overlap: 10,
            search_mode: rag_db::SearchMode::Versioned,
        },
    )
    .await
    .unwrap();

    // Index the collection's primary ref.
    let r = rag_db::add_ref(&pool, collection.id, "main", None, true)
        .await
        .unwrap();
    indexer.index_ref(r.id).await.unwrap();

    // Re-fetch the ref: a successful build swapped its data_uuid + marked it
    // ready/searchable.
    let after = rag_db::find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
    assert_eq!(after.status, rag_db::CollectionStatus::Ready);
    assert!(after.last_indexed_commit.is_some());
    assert!(after.is_searchable());

    // Content lives in the ref's own store.
    let store = indexer
        .collection_store(after.id, &after.data_uuid)
        .await
        .unwrap();
    let files = rag_db::list_files_for_collection(&store, collection.id)
        .await
        .unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.contains(&"alpha.txt"));
    assert!(paths.contains(&"beta.txt"));
    assert!(!paths.contains(&"README.md")); // filtered by *.txt include

    let total_chunks: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rag_chunks WHERE collection_id = ?")
            .bind(collection.id)
            .fetch_one(&store)
            .await
            .unwrap();
    assert!(
        total_chunks >= 2,
        "expected at least one chunk per file, got {total_chunks}"
    );

    // Now: embed an "alpha" query through the same upstream and search.
    let query_vec = embeddings::embed(
        &reqwest::Client::new(),
        &registry,
        "embed-test",
        &["please find alpha".to_string()],
    )
    .await
    .unwrap()
    .pop()
    .unwrap();

    let hits = search_chunks(&indexer, &after, "please find alpha", &query_vec, 5, None)
        .await
        .unwrap();
    assert!(!hits.is_empty(), "search returned no hits");
    let alpha_file_id = files.iter().find(|f| f.path == "alpha.txt").unwrap().id;
    let top = &hits[0].0;
    assert_eq!(
        top.file_id, alpha_file_id,
        "top hit was {:?}, expected alpha.txt",
        top
    );
}

#[tokio::test]
async fn reindex_after_edit_drops_old_chunks_and_picks_up_new_content() {
    let Some(repo) = fixture_repo() else {
        eprintln!("git not on PATH — skipping");
        return;
    };
    let upstream = start_embedding_upstream().await;
    let registry = registry_pointed_at(&upstream.uri());
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let indexer = Indexer::new(
        pool.clone(),
        Arc::clone(&registry),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..IndexerConfig::default()
        },
    );

    let collection = rag_db::create_collection(
        &pool,
        &rag_db::NewCollection {
            name: "fixture2".into(),
            description: None,
            git_url: repo.path().to_string_lossy().to_string(),
            git_ref: "main".into(),
            pat: None,
            embedding_model: "embed-test".into(),
            include_globs: vec!["*.txt".into()],
            exclude_globs: Vec::new(),
            chunk_size: 80,
            chunk_overlap: 10,
            search_mode: rag_db::SearchMode::Versioned,
        },
    )
    .await
    .unwrap();
    let r = rag_db::add_ref(&pool, collection.id, "main", None, true)
        .await
        .unwrap();
    indexer.index_ref(r.id).await.unwrap();

    // Content lives in the ref's own store (re-fetch for its current uuid).
    let r = rag_db::find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
    let store = indexer.collection_store(r.id, &r.data_uuid).await.unwrap();
    let first_chunks: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rag_chunks WHERE collection_id = ?")
            .bind(collection.id)
            .fetch_one(&store)
            .await
            .unwrap();
    assert!(first_chunks > 0);

    // Edit alpha.txt upstream + commit.
    std::fs::write(repo.path().join("alpha.txt"), b"gamma gamma gamma\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args(["commit", "-q", "-m", "edit"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );

    rag_db::request_ref_reindex(&pool, r.id).await.unwrap();
    indexer.index_ref(r.id).await.unwrap();

    // The rebuild swapped in a fresh store; re-fetch the ref + reopen it.
    let r = rag_db::find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
    let store = indexer.collection_store(r.id, &r.data_uuid).await.unwrap();
    // File count stays at 2 (no deletions), but alpha.txt's hash changed.
    let files = rag_db::list_files_for_collection(&store, collection.id)
        .await
        .unwrap();
    let alpha = files.iter().find(|f| f.path == "alpha.txt").unwrap();
    let beta = files.iter().find(|f| f.path == "beta.txt").unwrap();
    assert_ne!(alpha.content_hash, beta.content_hash);

    // A "gamma" query should now top-hit alpha.txt — it was rewritten.
    let qvec = embeddings::embed(
        &reqwest::Client::new(),
        &registry,
        "embed-test",
        &["gamma please".to_string()],
    )
    .await
    .unwrap()
    .pop()
    .unwrap();
    let hits = search_chunks(&indexer, &r, "gamma please", &qvec, 5, None)
        .await
        .unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].0.file_id, alpha.id);
}

/// A throwaway git repo containing a single uniquely-named file, so an
/// aggregate build folding several of these can be told apart by path.
fn fixture_repo_with(marker: &str) -> Option<tempfile::TempDir> {
    let dir = tempdir().unwrap();
    let p = dir.path();
    let ok = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(p)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !ok(&["init", "-q", "-b", "main", "."]) {
        return None;
    }
    assert!(ok(&["config", "user.email", "t@example.invalid"]));
    assert!(ok(&["config", "user.name", "t"]));
    assert!(ok(&["config", "commit.gpgsign", "false"]));
    std::fs::write(
        p.join(format!("{marker}.txt")),
        format!("{marker} {marker} {marker} {marker}\n"),
    )
    .unwrap();
    assert!(ok(&["add", "-A"]));
    assert!(ok(&["commit", "-q", "-m", "seed"]));
    Some(dir)
}

/// An aggregate collection with several sources builds ONE unified index that
/// folds in *every* source. The sources are cloned concurrently (bounded by
/// `clone_concurrency`), so this also guards that the parallel clone fan-out
/// produces the complete, correctly-prefixed corpus — not just whichever clone
/// happened to finish first.
#[tokio::test]
async fn aggregate_folds_every_source_when_clones_run_in_parallel() {
    let markers = ["src0", "src1", "src2"];
    let mut repos = Vec::new();
    for m in markers {
        let Some(repo) = fixture_repo_with(m) else {
            eprintln!("git not on PATH — skipping");
            return;
        };
        repos.push(repo);
    }

    let upstream = start_embedding_upstream().await;
    let registry = registry_pointed_at(&upstream.uri());
    let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
    let data_dir = tempdir().unwrap();
    let indexer = Indexer::new(
        pool.clone(),
        Arc::clone(&registry),
        reqwest::Client::new(),
        IndexerConfig {
            data_dir: data_dir.path().to_path_buf(),
            // Force real parallelism across the sources.
            clone_concurrency: 3,
            ..IndexerConfig::default()
        },
    );

    // Aggregate collection carries no single repo URL — each source brings its
    // own (added below).
    let collection = rag_db::create_collection(
        &pool,
        &rag_db::NewCollection {
            name: "corpus".into(),
            description: None,
            git_url: String::new(),
            git_ref: "main".into(),
            pat: None,
            embedding_model: "embed-test".into(),
            include_globs: vec!["*.txt".into()],
            exclude_globs: Vec::new(),
            chunk_size: 80,
            chunk_overlap: 10,
            search_mode: rag_db::SearchMode::Aggregate,
        },
    )
    .await
    .unwrap();

    // First source is the primary (holds the unified index); the rest are
    // config-only sources folded into it.
    let mut primary_id = None;
    for (i, repo) in repos.iter().enumerate() {
        let url = repo.path().to_string_lossy().to_string();
        let r = rag_db::add_ref(&pool, collection.id, "main", Some(&url), i == 0)
            .await
            .unwrap();
        if i == 0 {
            primary_id = Some(r.id);
        }
    }

    // Building the primary clones EVERY source (in parallel) and indexes the
    // combined tree.
    indexer.index_ref(primary_id.unwrap()).await.unwrap();

    let after = rag_db::find_ref_by_id(&pool, primary_id.unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, rag_db::CollectionStatus::Ready);

    let store = indexer
        .collection_store(after.id, &after.data_uuid)
        .await
        .unwrap();
    let files = rag_db::list_files_for_collection(&store, collection.id)
        .await
        .unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    // Every source's file must be present, each under its own repo-label prefix
    // (`<label>/src{N}.txt`) — proving all three parallel clones were folded in.
    for m in markers {
        let want = format!("{m}.txt");
        assert!(
            paths.iter().any(|p| p.ends_with(&want) && p.contains('/')),
            "aggregate corpus is missing source {m}; got {paths:?}"
        );
    }
}
