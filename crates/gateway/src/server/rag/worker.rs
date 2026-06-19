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
    /// Central registry DB (`gateway.sqlite`) — holds the collection config
    /// and the `rag_collection_refs` rows. Per-ref content lives in each
    /// ref's own store folder (see `stores`).
    db: Pool,
    upstreams: Arc<UpstreamRegistry>,
    http: reqwest::Client,
    config: IndexerConfig,
    /// One [`CollectionIndex`] per **ref** (keyed by ref id), opened lazily
    /// on first search. Kept around so subsequent searches skip the
    /// metadata-read + mmap setup. Evicted on a zero-downtime swap so the
    /// next search reopens the ref's new store folder.
    indexes: Mutex<HashMap<i64, Arc<CollectionIndex>>>,
    /// One SQLite [`Pool`] per **ref** over its `rag.sqlite` store, keyed by
    /// ref id. Opened lazily, evicted on swap.
    stores: Mutex<HashMap<i64, Pool>>,
    /// Wakes the background loop immediately when a ref is (re-)queued, so
    /// a "Re-index" click doesn't wait out the poll interval.
    kick: tokio::sync::Notify,
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
                kick: tokio::sync::Notify::new(),
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

    /// Evict a ref's cached store pool + index handle so the next search
    /// reopens from the ref's current `data_uuid` folder. Called after a
    /// zero-downtime swap (the folder changed) and on teardown.
    fn evict_ref_caches(&self, ref_id: i64) {
        self.inner
            .indexes
            .lock()
            .expect("indexer cache mutex poisoned")
            .remove(&ref_id);
        self.inner
            .stores
            .lock()
            .expect("indexer store cache mutex poisoned")
            .remove(&ref_id);
    }

    /// `rm -rf` a store folder, best-effort (a missing folder is fine).
    fn discard_dir(&self, uuid: &str) {
        let dir = self.collection_dir(uuid);
        if let Err(err) = std::fs::remove_dir_all(&dir)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %err, dir = %dir.display(), "rag: failed to remove store folder");
        }
    }

    /// Tear down a ref's on-disk storage: evict its cached handles, then
    /// `rm -rf` its `<data_dir>/<uuid>/` folder. Call after deleting the
    /// ref row (or all refs of a collection being deleted).
    pub fn drop_ref_storage(&self, ref_id: i64, uuid: &str) {
        self.evict_ref_caches(ref_id);
        self.discard_dir(uuid);
    }

    /// (Re-)queue a ref for indexing and wake the worker immediately, so a
    /// "Re-index" click takes effect now rather than after the poll
    /// interval. The running build (if any) sees `status != indexing` at
    /// its next checkpoint and aborts, then this requeue is picked up.
    pub async fn request_reindex(&self, ref_id: i64) -> Result<(), crate::server::db::DbError> {
        rag_db::request_ref_reindex(&self.inner.db, ref_id).await?;
        self.inner.kick.notify_one();
        Ok(())
    }

    /// True if the in-flight build of `ref_id` has been superseded — the
    /// ref was re-queued (`status='pending'`) or deleted. Checked between
    /// embed batches so a re-index aborts the wasted work early; the final
    /// `swap_ref_index` (guarded by `status='indexing'`) is the backstop.
    async fn superseded(&self, ref_id: i64) -> Result<bool, WorkerError> {
        match rag_db::find_ref_by_id(&self.inner.db, ref_id).await? {
            None => Ok(true),
            Some(r) => Ok(r.status == rag_db::CollectionStatus::Pending),
        }
    }

    /// Startup recovery: re-queue refs left mid-build by a crash/restart,
    /// and reap orphaned store folders no ref points at (interrupted
    /// builds). Call once before [`spawn`].
    pub async fn recover_on_startup(&self) {
        match rag_db::reset_stalled_refs(&self.inner.db).await {
            Ok(n) if n > 0 => tracing::info!(refs = n, "rag: re-queued refs stalled at startup"),
            Ok(_) => {}
            Err(err) => tracing::warn!(error = %err, "rag: startup stalled-ref reset failed"),
        }
        // Reap store folders not referenced by any ref (leftover build dirs).
        let live: std::collections::HashSet<String> =
            match rag_db::all_ref_data_uuids(&self.inner.db).await {
                Ok(v) => v.into_iter().collect(),
                Err(err) => {
                    tracing::warn!(error = %err, "rag: could not list live store folders");
                    return;
                }
            };
        let Ok(entries) = std::fs::read_dir(&self.inner.config.data_dir) else {
            return;
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !live.contains(&name) {
                tracing::info!(dir = %name, "rag: reaping orphaned store folder");
                self.discard_dir(&name);
            }
        }
    }

    /// (Re-)index one ref. Builds the whole index fresh into a new store
    /// folder and atomically swaps the ref onto it — zero-downtime, since
    /// the ref keeps serving its previous index until the swap. Failures
    /// are recorded against the ref (guarded so a concurrent re-queue isn't
    /// clobbered).
    pub async fn index_ref(&self, ref_id: i64) -> Result<(), WorkerError> {
        match self.index_ref_inner(ref_id).await {
            Ok(()) => Ok(()),
            Err(err) => {
                let msg = err.to_string();
                let _ = rag_db::mark_ref_failed(&self.inner.db, ref_id, &msg).await;
                Err(err)
            }
        }
    }

    async fn index_ref_inner(&self, ref_id: i64) -> Result<(), WorkerError> {
        let Some(rref) = rag_db::find_ref_by_id(&self.inner.db, ref_id).await? else {
            return Ok(()); // ref deleted before we reached it
        };
        let Some(collection) =
            rag_db::find_collection_by_id(&self.inner.db, rref.collection_id).await?
        else {
            return Ok(()); // collection deleted
        };
        let old_uuid = rref.data_uuid.clone();
        // Always build into a *fresh* folder so the live store keeps serving
        // searches until we atomically swap onto the new one.
        let build_uuid = uuid::Uuid::new_v4().to_string();

        match self.build_ref(&collection, &rref, &build_uuid).await {
            // Swapped: drop cached handles so searches reopen the new folder,
            // then reap the old store.
            Ok(true) => {
                self.evict_ref_caches(ref_id);
                if old_uuid != build_uuid {
                    self.discard_dir(&old_uuid);
                }
                Ok(())
            }
            // Superseded by a re-queue / delete — throw the build away; the
            // live index is untouched.
            Ok(false) => {
                self.discard_dir(&build_uuid);
                Ok(())
            }
            Err(err) => {
                self.discard_dir(&build_uuid);
                Err(err)
            }
        }
    }

    /// Clone → chunk → embed into `build_uuid`'s fresh store, then
    /// atomically swap the ref onto it. `Ok(true)` = swapped (now live);
    /// `Ok(false)` = the build was superseded (re-queued / deleted) and the
    /// caller should discard it. The build uses *local* store + index
    /// handles, never the cached (live) ones, so concurrent searches keep
    /// hitting the old index until the swap.
    async fn build_ref(
        &self,
        collection: &rag_db::Collection,
        rref: &rag_db::CollectionRef,
        build_uuid: &str,
    ) -> Result<bool, WorkerError> {
        let ref_id = rref.id;

        rag_db::set_ref_status(&self.inner.db, ref_id, rag_db::CollectionStatus::Cloning).await?;
        let clone_dir = self.clone_path(build_uuid);
        // Aggregate collections give each source its own repo URL; versioned
        // ones inherit the collection's. The PAT is the collection's (one
        // credential covers its sources).
        let head = git::clone_or_update(
            rref.effective_git_url(collection),
            &rref.git_ref,
            collection.pat.as_deref(),
            &clone_dir,
        )
        .await?;

        if self.superseded(ref_id).await? {
            return Ok(false);
        }
        rag_db::set_ref_status(&self.inner.db, ref_id, rag_db::CollectionStatus::Indexing).await?;

        // Fresh, uncached store + index for this build.
        let store = crate::server::db::open_collection_store(
            &self.collection_dir(build_uuid).join("rag.sqlite"),
        )
        .await?;
        let index_path = self.index_path(build_uuid);

        let filter = Filter::new(
            &collection.include_globs,
            &collection.exclude_globs,
            self.inner.config.max_file_bytes,
        );
        let walked = walk::walk(&clone_dir, &filter)?;

        let mut next_vector_id = 1i64;
        let mut dimensions: Option<usize> = None;
        let mut index: Option<CollectionIndex> = None;

        for file in &walked {
            let content = match std::fs::read(&file.abs_path) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => continue, // binary — skip
                },
                Err(_) => continue,
            };
            let pieces: Vec<ChunkPiece> = chunk::chunk_text(
                &content,
                collection.chunk_size as usize,
                collection.chunk_overlap as usize,
            );
            if pieces.is_empty() {
                continue;
            }
            let hash = sha256_hex(&content);
            let file_id = rag_db::upsert_file(&store, collection.id, &file.rel_path, &hash).await?;

            for batch in pieces.chunks(self.inner.config.embed_batch_size) {
                // Abort early if a re-queue / delete superseded this build,
                // so we don't burn embedding calls on a doomed run.
                if self.superseded(ref_id).await? {
                    return Ok(false);
                }
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
                if index.is_none() {
                    dimensions = Some(dim);
                    index = Some(CollectionIndex::open_or_create(&index_path, dim)?);
                }
                if dimensions != Some(dim) {
                    return Err(WorkerError::Index(IndexError::BadVectorLen {
                        expected: dimensions.unwrap_or(0),
                        got: dim,
                    }));
                }
                let idx = index.as_ref().expect("index opened above");

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

        if let Some(idx) = &index {
            idx.save()?;
        }
        // Flush + close the build store before the swap points searches at it.
        store.close().await;

        // Atomic swap, guarded by `status='indexing'`: if a re-queue flipped
        // the ref to `pending` while we built, this affects 0 rows and we
        // report "superseded" so the caller discards the build.
        let swapped = rag_db::swap_ref_index(&self.inner.db, ref_id, build_uuid, &head).await? == 1;
        Ok(swapped)
    }
}

/// Spawn the background loop. Runs forever until the gateway shuts down.
/// Each pass indexes every `pending` ref (oldest-queued first), serially.
/// It then sleeps until the next poll tick *or* an explicit kick (a
/// "Re-index" click), whichever comes first, so re-indexes start promptly.
/// Failures are logged + recorded against the ref; the loop never panics.
pub fn spawn(indexer: Indexer) {
    let inner = indexer.clone();
    tokio::spawn(async move {
        let interval = inner.config().poll_interval;
        loop {
            if let Err(err) = drain_once(&inner).await {
                tracing::warn!(error = %err, "rag indexer pass failed");
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = inner.inner.kick.notified() => {}
            }
        }
    });
}

async fn drain_once(indexer: &Indexer) -> Result<(), WorkerError> {
    let pending = rag_db::list_pending_refs(&indexer.inner.db).await?;
    for r in pending {
        if let Err(err) = indexer.index_ref(r.id).await {
            tracing::warn!(ref_id = r.id, error = %err, "rag: indexing failed");
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
    rref: &rag_db::CollectionRef,
    query_text: &str,
    query_vec: &[f32],
    k: usize,
) -> Result<Vec<(rag_db::Chunk, f32)>, WorkerError> {
    if k == 0 {
        return Ok(Vec::new());
    }

    // Store + index live in this ref's own folder, cached by ref id.
    let store = indexer.collection_store(rref.id, &rref.data_uuid).await?;
    let pool = (k * CANDIDATE_MULTIPLIER).max(MIN_CANDIDATES);

    // Dense side. A missing on-disk index (ref never finished its first
    // build) is not an error here — fall back to lexical-only.
    let dense: Vec<i64> = match indexer.open_index(rref.id, &rref.data_uuid, None) {
        Ok(index) => index
            .search(query_vec, pool)?
            .into_iter()
            .map(|(vid, _)| vid)
            .collect(),
        Err(IndexError::Open { .. }) => Vec::new(),
        Err(other) => return Err(other.into()),
    };

    // Lexical side (BM25 over chunk text) — from this ref's store.
    let lexical = rag_db::lexical_search(&store, rref.collection_id, query_text, pool).await?;

    let fused = reciprocal_rank_fusion(&[&dense, &lexical], k);
    if fused.is_empty() {
        return Ok(Vec::new());
    }

    let vids: Vec<i64> = fused.iter().map(|(vid, _)| *vid).collect();
    let chunks = rag_db::chunks_by_vector_ids(&store, rref.collection_id, &vids).await?;
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
