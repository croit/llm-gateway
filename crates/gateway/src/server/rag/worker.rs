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
    db: Pool,
    upstreams: Arc<UpstreamRegistry>,
    http: reqwest::Client,
    config: IndexerConfig,
    /// One [`CollectionIndex`] per collection, opened lazily on first
    /// search/insert. Kept around so subsequent operations skip the
    /// metadata-read + mmap setup.
    indexes: Mutex<HashMap<i64, Arc<CollectionIndex>>>,
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
    /// search tool uses this to vectorise a user query before reaching
    /// `search_chunks`.
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

    /// Path on disk for `collection_id`'s usearch file.
    fn index_path(&self, collection_id: i64) -> PathBuf {
        self.inner
            .config
            .data_dir
            .join(format!("{collection_id}.usearch"))
    }

    /// Path on disk for `collection_id`'s clone cache.
    fn clone_path(&self, collection_id: i64) -> PathBuf {
        self.inner
            .config
            .data_dir
            .join("cache")
            .join(format!("{collection_id}"))
    }

    /// Lookup-or-open the in-memory index handle for `collection_id`.
    /// `dimensions` is required for the first call — subsequent calls
    /// can pass `None` (we use the loaded index's dim).
    pub fn open_index(
        &self,
        collection_id: i64,
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
        let path = self.index_path(collection_id);
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

        rag_db::set_collection_status(
            &self.inner.db,
            collection.id,
            rag_db::CollectionStatus::Cloning,
        )
        .await?;
        let clone_dir = self.clone_path(collection.id);
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
        let prior = rag_db::list_files_for_collection(&self.inner.db, collection.id).await?;
        let mut prior_by_path: HashMap<String, rag_db::IndexedFile> =
            prior.into_iter().map(|f| (f.path.clone(), f)).collect();

        let mut next_vector_id = rag_db::max_vector_id(&self.inner.db, collection.id)
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
                let old_vids =
                    rag_db::chunk_vector_ids_for_file(&self.inner.db, prior_file.id).await?;
                rag_db::delete_chunks_for_file(&self.inner.db, prior_file.id).await?;
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

            let file_id =
                rag_db::upsert_file(&self.inner.db, collection.id, &file.rel_path, &hash).await?;

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
                        let opened = self.open_index(collection.id, Some(dim))?;
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
                rag_db::insert_chunks(&self.inner.db, collection.id, &new_chunks).await?;
                for (vid, vec) in to_index {
                    idx.add(vid, &vec)?;
                }
            }
        }

        // Files left in `prior_by_path` are deletions on this pass — drop
        // their chunks/vectors so a tombstoned file disappears from search.
        for (_, gone) in prior_by_path.drain() {
            let old_vids = rag_db::chunk_vector_ids_for_file(&self.inner.db, gone.id).await?;
            rag_db::delete_chunks_for_file(&self.inner.db, gone.id).await?;
            rag_db::delete_file(&self.inner.db, gone.id).await?;
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

/// Convenience: search the in-memory index for `collection_id`,
/// preserving the vector_id <-> chunk join. Public so the future
/// `rag_search` tool can reach into the indexer directly without
/// rebuilding the index cache.
pub async fn search_chunks(
    indexer: &Indexer,
    collection_id: i64,
    query_vec: &[f32],
    k: usize,
) -> Result<Vec<(rag_db::Chunk, f32)>, WorkerError> {
    let index = match indexer.open_index(collection_id, None) {
        Ok(i) => i,
        // No on-disk index yet → collection never finished indexing.
        Err(IndexError::Open { .. }) => return Ok(Vec::new()),
        Err(other) => return Err(other.into()),
    };
    let hits = index.search(query_vec, k)?;
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    let vids: Vec<i64> = hits.iter().map(|(v, _)| *v).collect();
    let chunks = rag_db::chunks_by_vector_ids(&indexer.inner.db, collection_id, &vids).await?;
    // Re-join in `hits` order, dropping any vector ids whose chunk row
    // didn't come back (index/db drift; rare).
    let mut by_vid: HashMap<i64, rag_db::Chunk> =
        chunks.into_iter().map(|c| (c.vector_id, c)).collect();
    Ok(hits
        .into_iter()
        .filter_map(|(vid, score)| by_vid.remove(&vid).map(|c| (c, score)))
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
        let a = indexer.open_index(1, Some(4)).unwrap();
        let b = indexer.open_index(1, Some(4)).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        // Discovery path: a fresh handle for collection 1 should accept
        // a `None` dim hint now that the file exists.
        let c = indexer.open_index(1, None).unwrap();
        assert_eq!(c.dimensions(), 4);
    }
}
