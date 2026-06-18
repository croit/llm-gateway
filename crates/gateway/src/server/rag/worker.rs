// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Background indexer worker.
//!
//! One [`Indexer`] per gateway instance, `Arc`-shared between the
//! background loop (which drains `status='pending'` rows) and the
//! search-tool path (which opens the same on-disk index files to answer
//! queries). The indexer is deliberately serial per collection: the
//! pipeline (clone → walk → diff → chunk → embed → insert) holds the
//! collection's lifecycle row in `cloning` / `indexing` while it runs,
//! so a re-queue request only takes effect on the next pass — there's no
//! concurrent re-index of the same collection.
//!
//! The shape mirrors `server::geoip::update::spawn`: a long-lived tokio
//! task that wakes on an interval, scans the DB, and runs one job per
//! tick. Phase 4 will add an in-process kick channel for the admin API
//! so an operator's "re-index now" click doesn't wait the full poll
//! interval.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::server::db::Pool;
use crate::server::db::rag as rag_db;
use crate::server::embeddings::{self, EmbedError};
use crate::server::rag::chunk::{self, Chunk as ChunkPiece};
use crate::server::rag::git::{self, GitError};
use crate::server::rag::index::{CollectionIndex, IndexError};
use crate::server::rag::walk::{self, Filter};
use crate::server::upstreams::UpstreamRegistry;

/// Instruction prefix prepended to *query* embeddings (see
/// [`Indexer::embed_query`]). Kept here next to the indexer so the
/// query side and the (bare) document side are obviously paired.
const QUERY_INSTRUCTION: &str = "Instruct: Given a code-search question, retrieve the source-code or \
     documentation passages that answer it\nQuery: ";

/// Tunable knobs the indexer reads at construction time. The default
/// values are sized for "single small-medium codebase per collection";
/// operators can tighten them in config when running on constrained
/// hardware.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// Where the gateway puts its RAG state (one usearch file per
    /// collection + the per-collection clone cache).
    pub data_dir: PathBuf,
    /// Files larger than this are skipped during the walk.
    pub max_file_bytes: u64,
    /// How many chunks we send to the embedding upstream per request.
    pub embed_batch_size: usize,
    /// Poll cadence of the background loop — how often it scans for
    /// `status='pending'` rows.
    pub poll_interval: Duration,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data/rag"),
            max_file_bytes: 1_000_000,
            embed_batch_size: 32,
            poll_interval: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("db: {0}")]
    Db(#[from] crate::server::db::DbError),
    #[error("git: {0}")]
    Git(#[from] GitError),
    #[error("embedding: {0}")]
    Embed(#[from] EmbedError),
    #[error("vector index: {0}")]
    Index(#[from] IndexError),
    #[error("filesystem: {0}")]
    Io(#[from] std::io::Error),
    #[error("collection {id} not found")]
    NotFound { id: i64 },
}

/// Shared indexer state. Cheap to clone (everything is `Arc`-shared).
#[derive(Clone)]
pub struct Indexer {
    inner: Arc<IndexerInner>,
}

struct IndexerInner {
    /// Central registry DB (`gateway.sqlite`) — holds `rag_collections`
    /// config/status only. Per-collection content lives in each
    /// collection's own store (see `stores`).
    db: Pool,
    upstreams: Arc<UpstreamRegistry>,
    http: reqwest::Client,
    config: IndexerConfig,
    /// One [`CollectionIndex`] per collection, opened lazily on first
    /// search/insert. Kept around so subsequent operations skip the
    /// metadata-read + mmap setup.
    indexes: Mutex<HashMap<i64, Arc<CollectionIndex>>>,
    /// One SQLite [`Pool`] per collection over its `rag.sqlite` store
    /// (`<data_dir>/<uuid>/rag.sqlite`), opened lazily and cached so we
    /// don't re-open the file per query. Keyed by collection id.
    stores: Mutex<HashMap<i64, Pool>>,
}

impl Indexer {
    pub fn new(
        db: Pool,
        upstreams: Arc<UpstreamRegistry>,
        http: reqwest::Client,
        mut config: IndexerConfig,
    ) -> Self {
        // Resolve `data_dir` to an absolute path so every downstream
        // error message names the real on-disk path. Without this, a
        // gateway whose CWD is `/` (common under launchd / systemd
        // without `WorkingDirectory=`) silently tries to write under
        // `/data/rag/...` and the operator sees a baffling "permission
        // denied". `current_dir().join(...)` is enough — we don't need
        // canonicalisation (which would fail if the dir doesn't exist
        // yet).
        if !config.data_dir.is_absolute()
            && let Ok(cwd) = std::env::current_dir()
        {
            config.data_dir = cwd.join(&config.data_dir);
        }
        // Best-effort preflight: try to materialise the directory at
        // startup so a botched config fails loudly rather than on first
        // index. A failure here only logs; the per-collection error
        // path still surfaces the real reason on the /rag page.
        if let Err(err) = std::fs::create_dir_all(&config.data_dir) {
            tracing::warn!(
                error = %err,
                data_dir = %config.data_dir.display(),
                "rag indexer: could not create data_dir at startup"
            );
        } else {
            tracing::info!(
                data_dir = %config.data_dir.display(),
                "rag indexer ready"
            );
        }
        Self {
            inner: Arc::new(IndexerInner {
                db,
                upstreams,
                http,
                config,
                indexes: Mutex::new(HashMap::new()),
                stores: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn config(&self) -> &IndexerConfig {
        &self.inner.config
    }

    pub fn db(&self) -> &Pool {
        &self.inner.db
    }

    /// Embed a single text through the configured embedding model. The
    /// indexer uses this for document chunks; queries should go through
    /// [`Indexer::embed_query`] instead so they carry the instruction
    /// prefix.
    pub async fn embed_one(&self, model: &str, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut out = embeddings::embed(
            &self.inner.http,
            &self.inner.upstreams,
            model,
            &[text.to_string()],
        )
        .await?;
        out.pop().ok_or(EmbedError::CountMismatch {
            expected: 1,
            got: 0,
        })
    }

    /// Embed a user *query* for retrieval. Qwen3-Embedding (and the BGE /
    /// E5 family generally) is instruction-tuned and **asymmetric**: the
    /// query side is meant to carry a task instruction while the document
    /// side is embedded bare. We embed chunks bare in [`Self::index_one`]
    /// and add the instruction here, matching the model's recommended
    /// format. This lifts the query and its matching passages into the
    /// same region of the space, so a natural-language ask like "osd
    /// operation timeout" lands near the option that defines it instead of
    /// drifting toward lexically-similar but unrelated code.
    ///
    /// Embedding models that ignore the prefix simply treat it as a few
    /// extra tokens — harmless. The prefix is deliberately generic so it
    /// works for prose docs and source alike.
    pub async fn embed_query(&self, model: &str, query: &str) -> Result<Vec<f32>, EmbedError> {
        let text = format!("{QUERY_INSTRUCTION}{query}");
        self.embed_one(model, &text).await
    }

    /// This collection's self-contained store folder,
    /// `<data_dir>/<uuid>/`. All of a collection's regenerable state —
    /// `rag.sqlite`, `index.usearch`, `clone/` — lives under here, so
    /// teardown is a single `rm -rf`.
    fn collection_dir(&self, uuid: &str) -> PathBuf {
        self.inner.config.data_dir.join(uuid)
    }

    /// Path on disk for this collection's usearch vector file.
    fn index_path(&self, uuid: &str) -> PathBuf {
        self.collection_dir(uuid).join("index.usearch")
    }

    /// Path on disk for this collection's git clone working tree.
    fn clone_path(&self, uuid: &str) -> PathBuf {
        self.collection_dir(uuid).join("clone")
    }

    /// Lookup-or-open the per-collection SQLite store pool (its
    /// `rag.sqlite`), cached by collection id.
    pub async fn collection_store(
        &self,
        collection_id: i64,
        uuid: &str,
    ) -> Result<Pool, crate::server::db::DbError> {
        if let Some(existing) = self
            .inner
            .stores
            .lock()
            .expect("indexer store cache mutex poisoned")
            .get(&collection_id)
        {
            return Ok(existing.clone());
        }
        let path = self.collection_dir(uuid).join("rag.sqlite");
        let pool = crate::server::db::open_collection_store(&path).await?;
        let mut guard = self
            .inner
            .stores
            .lock()
            .expect("indexer store cache mutex poisoned");
        // Another task may have opened it while we awaited; keep the first.
        let entry = guard.entry(collection_id).or_insert(pool);
        Ok(entry.clone())
    }

    /// Lookup-or-open the in-memory index handle for a collection (keyed
    /// by id; file lives under the collection's `uuid` folder).
    /// `dimensions` is required for the first call — subsequent calls
    /// can pass `None` (we use the loaded index's dim).
    pub fn open_index(
        &self,
        collection_id: i64,
        uuid: &str,
        dimensions: Option<usize>,
    ) -> Result<Arc<CollectionIndex>, IndexError> {
        let mut guard = self
            .inner
            .indexes
            .lock()
            .expect("indexer cache mutex poisoned");
        if let Some(existing) = guard.get(&collection_id) {
            return Ok(Arc::clone(existing));
        }
        let path = self.index_path(uuid);
        let dim = match (path.exists(), dimensions) {
            (true, _) => {
                // Discover from the file header rather than trust the
                // caller — keeps reopen sound when the embedding model
                // got changed under us.
                let meta = usearch::Index::metadata(&path.to_string_lossy()).map_err(|e| {
                    IndexError::Open {
                        path: path.clone(),
                        message: e.to_string(),
                    }
                })?;
                meta.dimensions as usize
            }
            (false, Some(d)) => d,
            (false, None) => {
                return Err(IndexError::Open {
                    path,
                    message: "no index on disk yet and caller did not supply dimensions".into(),
                });
            }
        };
        let index = Arc::new(CollectionIndex::open_or_create(&path, dim)?);
        guard.insert(collection_id, Arc::clone(&index));
        Ok(index)
    }

    /// Tear down a collection's on-disk storage: evict the cached store
    /// pool + index handle, then `rm -rf` its `<data_dir>/<uuid>/` folder
    /// (rag.sqlite + index.usearch + clone/). Call after deleting the
    /// central registry row. Best-effort on the filesystem — a failed
    /// remove only logs, since the row is already gone.
    pub fn drop_collection_storage(&self, collection_id: i64, uuid: &str) {
        // Drop cached handles first so we're not holding the files open.
        self.inner
            .indexes
            .lock()
            .expect("indexer cache mutex poisoned")
            .remove(&collection_id);
        self.inner
            .stores
            .lock()
            .expect("indexer store cache mutex poisoned")
            .remove(&collection_id);
        let dir = self.collection_dir(uuid);
        if let Err(err) = std::fs::remove_dir_all(&dir)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                error = %err,
                dir = %dir.display(),
                "rag: failed to remove collection storage folder"
            );
        }
    }

    /// Run the full pipeline against one collection. Returns the
    /// resolved HEAD commit on success. Any error path also surfaces
    /// `mark_failed` against the DB row.
    pub async fn index_one(&self, collection_id: i64) -> Result<String, WorkerError> {
        match self.index_one_inner(collection_id).await {
            Ok(sha) => Ok(sha),
            Err(err) => {
                let msg = err.to_string();
                let _ = rag_db::mark_failed(&self.inner.db, collection_id, &msg).await;
                Err(err)
            }
        }
    }

    async fn index_one_inner(&self, collection_id: i64) -> Result<String, WorkerError> {
        let collection = rag_db::find_collection_by_id(&self.inner.db, collection_id)
            .await?
            .ok_or(WorkerError::NotFound { id: collection_id })?;

        // Resolve this collection's store-folder id. Pre-migration rows
        // have none yet; mint one and persist it so the folder is stable.
        let uuid = match collection.data_uuid.clone() {
            Some(u) => u,
            None => {
                let u = uuid::Uuid::new_v4().to_string();
                rag_db::assign_data_uuid(&self.inner.db, collection.id, &u).await?;
                u
            }
        };
        // All content (files/chunks/FTS) goes to the per-collection store,
        // never the central registry DB.
        let store = self.collection_store(collection.id, &uuid).await?;

        rag_db::set_collection_status(
            &self.inner.db,
            collection.id,
            rag_db::CollectionStatus::Cloning,
        )
        .await?;
        let clone_dir = self.clone_path(&uuid);
        let head = git::clone_or_update(
            &collection.git_url,
            &collection.git_ref,
            collection.pat.as_deref(),
            &clone_dir,
        )
        .await?;

        rag_db::set_collection_status(
            &self.inner.db,
            collection.id,
            rag_db::CollectionStatus::Indexing,
        )
        .await?;

        let filter = Filter::new(
            &collection.include_globs,
            &collection.exclude_globs,
            self.inner.config.max_file_bytes,
        );
        let walked = walk::walk(&clone_dir, &filter)?;

        // Snapshot the prior file state for diffing.
        let prior = rag_db::list_files_for_collection(&store, collection.id).await?;
        let mut prior_by_path: HashMap<String, rag_db::IndexedFile> =
            prior.into_iter().map(|f| (f.path.clone(), f)).collect();

        let mut next_vector_id = rag_db::max_vector_id(&store, collection.id)
            .await?
            .map(|v| v + 1)
            .unwrap_or(1);
        let mut dimensions: Option<usize> = None;
        let mut index: Option<Arc<CollectionIndex>> = None;

        for file in &walked {
            let bytes = match std::fs::read(&file.abs_path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let content = match String::from_utf8(bytes) {
                Ok(s) => s,
                // Binary file — skip silently; the walker is best-effort
                // and binaries shouldn't have been included anyway.
                Err(_) => continue,
            };
            let hash = sha256_hex(&content);
            if let Some(existing) = prior_by_path.remove(&file.rel_path)
                && existing.content_hash == hash
            {
                continue;
            }

            // Re-embedding this file — drop the old chunks + vectors first.
            if let Some(prior_file) = prior_by_path.remove(&file.rel_path) {
                let old_vids = rag_db::chunk_vector_ids_for_file(&store, prior_file.id).await?;
                rag_db::delete_chunks_for_file(&store, prior_file.id).await?;
                if let Some(idx) = &index {
                    for vid in old_vids {
                        idx.remove(vid)?;
                    }
                }
            }
            // ... or this file is *new*; still safe to fall through.

            let pieces: Vec<ChunkPiece> = chunk::chunk_text(
                &content,
                collection.chunk_size as usize,
                collection.chunk_overlap as usize,
            );
            if pieces.is_empty() {
                continue;
            }

            let file_id = rag_db::upsert_file(&store, collection.id, &file.rel_path, &hash).await?;

            // Embed in batches; on the first response, open / sanity-check
            // the index using the discovered dimensionality.
            for batch in pieces.chunks(self.inner.config.embed_batch_size) {
                let inputs: Vec<String> = batch.iter().map(|p| p.content.clone()).collect();
                let vectors = embeddings::embed(
                    &self.inner.http,
                    &self.inner.upstreams,
                    &collection.embedding_model,
                    &inputs,
                )
                .await?;
                if vectors.is_empty() {
                    continue;
                }
                let dim = vectors[0].len();
                let idx = match &index {
                    Some(i) => Arc::clone(i),
                    None => {
                        dimensions = Some(dim);
                        let opened = self.open_index(collection.id, &uuid, Some(dim))?;
                        index = Some(Arc::clone(&opened));
                        opened
                    }
                };
                if dimensions != Some(dim) {
                    return Err(WorkerError::Index(IndexError::BadVectorLen {
                        expected: dimensions.unwrap_or(0),
                        got: dim,
                    }));
                }

                let mut new_chunks: Vec<rag_db::NewChunk> = Vec::with_capacity(batch.len());
                let mut to_index: Vec<(i64, Vec<f32>)> = Vec::with_capacity(batch.len());
                for (piece, vec) in batch.iter().zip(vectors.iter()) {
                    let vid = next_vector_id;
                    next_vector_id += 1;
                    new_chunks.push(rag_db::NewChunk {
                        file_id,
                        chunk_index: piece.chunk_index as i64,
                        start_line: piece.start_line as i64,
                        end_line: piece.end_line as i64,
                        content: piece.content.clone(),
                        vector_id: vid,
                    });
                    to_index.push((vid, vec.clone()));
                }
                rag_db::insert_chunks(&store, collection.id, &new_chunks).await?;
                for (vid, vec) in to_index {
                    idx.add(vid, &vec)?;
                }
            }
        }

        // Files left in `prior_by_path` are deletions on this pass — drop
        // their chunks/vectors so a tombstoned file disappears from search.
        for (_, gone) in prior_by_path.drain() {
            let old_vids = rag_db::chunk_vector_ids_for_file(&store, gone.id).await?;
            rag_db::delete_chunks_for_file(&store, gone.id).await?;
            rag_db::delete_file(&store, gone.id).await?;
            if let Some(idx) = &index {
                for vid in old_vids {
                    idx.remove(vid)?;
                }
            }
        }

        if let Some(idx) = &index {
            idx.save()?;
        }
        rag_db::mark_indexed(&self.inner.db, collection.id, &head).await?;
        Ok(head)
    }
}

/// Spawn the background loop. Runs forever until the gateway shuts down.
/// Each tick: pick the oldest `pending` collection (or `error` ones that
/// were re-queued via [`rag_db::request_reindex`]) and run the pipeline.
/// Failures are logged + recorded against the row; the loop never panics.
pub fn spawn(indexer: Indexer) {
    let inner = indexer.clone();
    tokio::spawn(async move {
        let interval = inner.config().poll_interval;
        loop {
            if let Err(err) = drain_once(&inner).await {
                tracing::warn!(error = %err, "rag indexer pass failed");
            }
            tokio::time::sleep(interval).await;
        }
    });
}

async fn drain_once(indexer: &Indexer) -> Result<(), WorkerError> {
    let pending = rag_db::list_collections(&indexer.inner.db).await?;
    for c in pending {
        if matches!(c.status, rag_db::CollectionStatus::Pending)
            && let Err(err) = indexer.index_one(c.id).await
        {
            tracing::warn!(collection_id = c.id, error = %err, "rag: indexing failed");
        }
    }
    Ok(())
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let bytes = h.finalize();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Reciprocal-rank-fusion constant. The standard k=60 from Cormack et
/// al.; it damps the contribution of low-ranked items so the head of
/// each list dominates without any single list being able to veto.
const RRF_K: f64 = 60.0;
/// Per-retriever candidate pool size relative to the caller's final `k`.
/// We pull more from each side than we'll return so fusion has room to
/// rerank across the dense and lexical signals.
const CANDIDATE_MULTIPLIER: usize = 4;
const MIN_CANDIDATES: usize = 20;

/// Fuse several ranked id-lists into one via Reciprocal Rank Fusion.
/// Each list contributes `1 / (RRF_K + rank)` to an id's score (rank
/// 1-based). Rank position is all that matters — no need to calibrate a
/// cosine distance against a BM25 score. Returns `(vector_id, score)`
/// best-first, capped at `k`; ties break by id for deterministic output.
fn reciprocal_rank_fusion(lists: &[&[i64]], k: usize) -> Vec<(i64, f64)> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for list in lists {
        for (rank, &id) in list.iter().enumerate() {
            *scores.entry(id).or_insert(0.0) += 1.0 / (RRF_K + (rank as f64) + 1.0);
        }
    }
    let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    ranked.truncate(k);
    ranked
}

/// Hybrid retrieval for `collection_id`: dense vector kNN fused with
/// FTS5/BM25 lexical ranking. Dense recall catches paraphrase and
/// conceptual matches; lexical recall catches exact identifiers
/// (`osd_op_timeout`) that embeddings smear across neighbours. The two
/// are combined with reciprocal rank fusion so neither dominates.
///
/// Either side degrading is non-fatal: a collection whose usearch file
/// hasn't been built yet still answers from the lexical index, and a
/// query with no usable lexical tokens still answers from vectors. The
/// returned `f32` is the RRF score (higher = more relevant), not a
/// cosine distance. Public so the `rag_search` tool can reach the
/// indexer directly without rebuilding the index cache.
pub async fn search_chunks(
    indexer: &Indexer,
    collection_id: i64,
    query_text: &str,
    query_vec: &[f32],
    k: usize,
) -> Result<Vec<(rag_db::Chunk, f32)>, WorkerError> {
    if k == 0 {
        return Ok(Vec::new());
    }

    // Resolve the collection's store folder. No row, or no folder id yet
    // (never indexed) → nothing to search.
    let Some(collection) = rag_db::find_collection_by_id(&indexer.inner.db, collection_id).await?
    else {
        return Ok(Vec::new());
    };
    let Some(uuid) = collection.data_uuid else {
        return Ok(Vec::new());
    };
    let store = indexer.collection_store(collection_id, &uuid).await?;

    let pool = (k * CANDIDATE_MULTIPLIER).max(MIN_CANDIDATES);

    // Dense side. A missing on-disk index (collection never finished
    // indexing) is not an error here — fall back to lexical-only.
    let dense: Vec<i64> = match indexer.open_index(collection_id, &uuid, None) {
        Ok(index) => index
            .search(query_vec, pool)?
            .into_iter()
            .map(|(vid, _)| vid)
            .collect(),
        Err(IndexError::Open { .. }) => Vec::new(),
        Err(other) => return Err(other.into()),
    };

    // Lexical side (BM25 over chunk text) — from the per-collection store.
    let lexical = rag_db::lexical_search(&store, collection_id, query_text, pool).await?;

    let fused = reciprocal_rank_fusion(&[&dense, &lexical], k);
    if fused.is_empty() {
        return Ok(Vec::new());
    }

    let vids: Vec<i64> = fused.iter().map(|(vid, _)| *vid).collect();
    let chunks = rag_db::chunks_by_vector_ids(&store, collection_id, &vids).await?;
    let mut by_vid: HashMap<i64, rag_db::Chunk> =
        chunks.into_iter().map(|c| (c.vector_id, c)).collect();
    // Re-join in fused order, carrying the RRF score; drop any vector id
    // whose chunk row didn't come back (index/db drift; rare).
    Ok(fused
        .into_iter()
        .filter_map(|(vid, score)| by_vid.remove(&vid).map(|c| (c, score as f32)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_is_lowercase_hex() {
        let hex = sha256_hex("hello");
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        // RFC test vector for SHA-256 of "hello".
        assert_eq!(
            hex,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    /// A tiny helper that asserts the indexer's index/cache plumbing
    /// returns the same Arc on a repeat open — covers the cache hit path.
    #[tokio::test]
    async fn open_index_returns_cached_handle_on_repeat() {
        use crate::server::upstreams::UpstreamRegistry;
        use std::collections::HashMap;

        let db = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let upstreams = UpstreamRegistry::new(&HashMap::new()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let indexer = Indexer::new(
            db,
            upstreams,
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: dir.path().to_path_buf(),
                ..IndexerConfig::default()
            },
        );
        let a = indexer.open_index(1, "uuid-1", Some(4)).unwrap();
        let b = indexer.open_index(1, "uuid-1", Some(4)).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        // Discovery path: a fresh handle for collection 1 should accept
        // a `None` dim hint now that the file exists.
        let c = indexer.open_index(1, "uuid-1", None).unwrap();
        assert_eq!(c.dimensions(), 4);
    }
}
