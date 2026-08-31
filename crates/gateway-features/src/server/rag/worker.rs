// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Background indexer worker.
//!
//! One [`Indexer`] per gateway instance, `Arc`-shared between the
//! background loop (which drains `status='pending'` rows) and the
//! search-tool path (which opens the same on-disk index files to answer
//! queries). The indexer is deliberately serial *per collection*: the
//! pipeline (clone → walk → diff → chunk → embed → insert) holds the
//! collection's lifecycle row in `cloning` / `indexing` while it runs,
//! so a re-queue request only takes effect on the next pass — there's no
//! concurrent re-index of the same collection.
//!
//! *Across* collections it is parallel: each drain pass groups pending
//! refs by collection and runs up to `clone_concurrency` collections at
//! once, so one slow clone can't head-of-line block every other
//! collection behind it. A single global [`Semaphore`] additionally caps
//! concurrent `git clone`s (including the fan-out within an aggregate
//! build, which clones all its sources at once) so parallelism can't
//! swamp the network. Embedding load is bounded by the same knob.
//!
//! The shape mirrors `server::geoip::update::spawn`: a long-lived tokio
//! task that wakes on an interval or an explicit kick, scans the DB, and
//! runs the pending work for that pass.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use thiserror::Error;

use crate::server::embeddings::{self, EmbedError};
use crate::server::rag::chunk;
use crate::server::rag::extract;
use crate::server::rag::git::{self, GitError};
use crate::server::rag::index::{CollectionIndex, IndexError};
use crate::server::rag::profile;
use crate::server::rag::rerank;
use crate::server::rag::sha256_hex;
use crate::server::rag::source;
use crate::server::rag::sync;
use crate::server::rag::walk::{self, Filter};
use gateway_core::server::crypto::Crypto;
use gateway_core::server::db::Pool;
use gateway_core::server::db::rag as rag_db;
use gateway_core::server::db::rag_documents as docs_db;
use gateway_core::server::upstreams::UpstreamRegistry;

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
    /// Cross-encoder model for reranking search results. `None` uses the
    /// first model the `rerank` pool advertises; with no such pool the step
    /// is skipped entirely.
    pub rerank_model: Option<String>,
    /// How many fused candidates the reranker is given. Larger means the
    /// cross-encoder has more chance to surface something fusion ranked low,
    /// at one model call's cost.
    pub rerank_candidates: usize,
    /// How much of a document the profile-extraction pass sees, head + tail.
    /// `0` means the module default. Invoice totals live at the bottom, so
    /// the budget is spent on both ends rather than only the first pages.
    pub extraction_max_input_chars: usize,
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
    /// Upper bound on concurrent git clones *and* on collections indexed in
    /// parallel. Lets independent repos/collections make progress instead of
    /// serializing behind one slow clone. `0` is clamped to `1` (fully
    /// serial). See [`gateway_core::server::config::RagConfig::clone_concurrency`].
    pub clone_concurrency: usize,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data/rag"),
            max_file_bytes: 1_000_000,
            embed_batch_size: 32,
            poll_interval: Duration::from_secs(30),
            clone_concurrency: 4,
            rerank_model: None,
            rerank_candidates: 50,
            extraction_max_input_chars: 24_000,
        }
    }
}

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("db: {0}")]
    Db(#[from] gateway_core::server::db::DbError),
    #[error("git: {0}")]
    Git(#[from] GitError),
    #[error("embedding: {0}")]
    Embed(#[from] EmbedError),
    #[error("vector index: {0}")]
    Index(#[from] IndexError),
    #[error("filesystem: {0}")]
    Io(#[from] std::io::Error),
    #[error("source: {0}")]
    Source(#[from] source::ProviderError),
    #[error("collection {id} not found")]
    NotFound { id: i64 },
}

/// The build phase a failure belongs to — recorded on the log entry so the
/// admin sees *where* it broke. Derived from the error variant: git failures
/// happen while cloning, everything else while indexing.
fn failure_phase(err: &WorkerError) -> &'static str {
    match err {
        // Reaching the source is the same phase for both source kinds: a
        // clone that fails and a directory listing that fails are the same
        // thing to an operator reading the timeline.
        WorkerError::Git(_) | WorkerError::Source(_) => "cloning",
        _ => "indexing",
    }
}

/// Translate a raw indexing error into an actionable, admin-facing message.
///
/// The raw error is precise but cryptic — e.g. `git clone exited with status
/// 128: fatal: Remote branch foo not found in upstream origin`. This maps the
/// common failure modes (bad ref, auth, missing repo, unreachable host,
/// embedding model) to a plain explanation plus a hint, and appends the raw
/// `git`/upstream detail in brackets so nothing is lost for deeper debugging.
/// Messages are English to match the rest of the admin UI.
fn friendly_error(
    err: &WorkerError,
    rref: &rag_db::CollectionRef,
    collection: &rag_db::Collection,
) -> String {
    let url = rref.effective_git_url(collection);
    match err {
        // Passed through verbatim. A provider error is written for operators
        // where it is raised, by the only code that knows which credential,
        // path or endpoint is at fault. Re-wording it here would blur that —
        // and would drag provider-specific knowledge into the worker, which
        // is precisely what the source abstraction exists to prevent.
        WorkerError::Source(err) => err.to_string(),
        WorkerError::Git(GitError::NonZero { stderr, .. }) => {
            let lower = stderr.to_lowercase();
            // Order matters: "remote branch X not found" also contains the
            // substring "not found", so the ref check must precede the
            // generic repository-not-found check.
            if (lower.contains("remote branch") && lower.contains("not found"))
                || lower.contains("couldn't find remote ref")
                || lower.contains("could not find remote ref")
            {
                format!(
                    "Branch/tag/commit '{}' does not exist in the repository {url}. \
                     Check the ref name — branches and tags are case-sensitive. [git: {stderr}]",
                    rref.git_ref
                )
            } else if lower.contains("authentication failed")
                || lower.contains("could not read username")
                || lower.contains("invalid username or password")
                || lower.contains("terminal prompts disabled")
                || lower.contains("permission denied")
                || lower.contains("403")
            {
                format!(
                    "Authentication failed for {url}. If the repository is private, set a valid \
                     access token (PAT) on the collection and make sure it can read this repo. \
                     [git: {stderr}]"
                )
            } else if lower.contains("repository not found")
                || lower.contains("does not appear to be a git repository")
                || lower.contains("not found")
            {
                format!(
                    "Repository not found at {url}. Check the URL is correct and the token can \
                     see it. [git: {stderr}]"
                )
            } else if lower.contains("could not resolve host")
                || lower.contains("unable to access")
                || lower.contains("connection")
                || lower.contains("timed out")
                || lower.contains("network")
            {
                format!(
                    "Could not reach the repository host for {url}. Check network/DNS access from \
                     the gateway. [git: {stderr}]"
                )
            } else {
                format!("Git error fetching {url}: {stderr}")
            }
        }
        WorkerError::Git(GitError::Spawn { .. }) => format!(
            "Could not run `git` on the gateway host — is git installed and on PATH? [{err}]"
        ),
        WorkerError::Git(GitError::Mkdir { path, .. }) => format!(
            "Could not prepare the clone-cache directory {} — check filesystem permissions and \
             free space. [{err}]",
            path.display()
        ),
        WorkerError::Git(GitError::BadUrl { .. }) => {
            format!("Invalid git URL for this source: {err}. Fix the repository URL.")
        }
        WorkerError::Git(GitError::BadOutput { .. }) => format!("Unexpected git output: {err}"),
        WorkerError::Embed(_) => format!(
            "Embedding failed using model '{}'. Check that this embedding model is configured and \
             reachable from the gateway. [{err}]",
            collection.embedding_model
        ),
        WorkerError::Index(_) => format!(
            "Vector index error: {err}. This usually means the embedding model's vector size \
             changed — remove and re-add the ref to rebuild from scratch."
        ),
        WorkerError::Io(_) => format!(
            "Filesystem error during indexing: {err}. Check the gateway's RAG data directory \
             permissions and free space."
        ),
        WorkerError::Db(_) => format!("Database error during indexing: {err}"),
        WorkerError::NotFound { .. } => err.to_string(),
    }
}

/// One file a build will index, and where its bytes come from.
///
/// The two variants exist because a git clone has already materialised the
/// tree on disk — re-fetching those files would be pure waste — while a
/// remote source hands over bytes on demand. Everything downstream of
/// [`Indexer::read_item`] is identical for both.
#[derive(Debug, Clone)]
pub enum BuildItem {
    Disk {
        rel_path: String,
        abs_path: std::path::PathBuf,
    },
    Remote(source::RemoteEntry),
}

impl BuildItem {
    pub fn rel_path(&self) -> &str {
        match self {
            BuildItem::Disk { rel_path, .. } => rel_path,
            BuildItem::Remote(entry) => &entry.rel_path,
        }
    }
}

/// What indexing one item produced.
///
/// `Skipped` and `Failed` are deliberately distinct: the first is permanent
/// (nothing here can read a `.zip`) and the second is transient (the server
/// was down). Only the second must stop a pass from being recorded as
/// authoritative.
enum ItemOutcome {
    Indexed(usize),
    /// Nothing indexable came out, and *why* — grouped by the caller into
    /// `skip_summary`. The reason is the whole value here: a corpus of scans
    /// with OCR switched off is indistinguishable from an empty collection
    /// without it.
    Skipped(String),
    Failed,
}

/// Outcome of reading one item. `Unsupported` is separate from `Failed`
/// because they mean different things to an operator: the first is "turn on
/// the OCR backend / the sandbox", the second is "something is broken".
enum ItemRead {
    Ok(extract::ExtractedDoc),
    Unsupported(String),
    Failed(String),
}

/// Result of a `build_ref` run. `Swapped` carries the stats the log records;
/// `Superseded` means a re-queue/delete won the race and the build was thrown
/// away with the live index untouched.
enum BuildOutcome {
    Swapped {
        files: usize,
        chunks: usize,
        commit: String,
        /// The store folder that is now live.
        ///
        /// A full rebuild writes a fresh one and the old folder is reaped; an
        /// incremental pass updates the existing folder in place and reports
        /// it unchanged. Without this the caller cannot tell the two apart,
        /// and would delete the very store the incremental pass just wrote.
        live_uuid: String,
        /// Whether every file this pass set out to read was actually read.
        ///
        /// A *transient* failure — a fetch that 503'd, an OCR backend that
        /// was down — means the corpus this pass saw is not the corpus that
        /// exists. Recording directory versions from it would let the next
        /// sync prune straight past a document that was never indexed, and
        /// nothing would revisit it until something else in that folder
        /// changed. `is_complete()` only covers directories that failed to
        /// *list*; this covers files that failed to *read*.
        all_items_read: bool,
    },
    Superseded,
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
    /// Caps concurrent `git clone`s across the whole indexer (all collections
    /// and all sources within an aggregate build share it). Sized to
    /// `config.clone_concurrency`, so one slow clone can't starve the rest.
    clone_sem: Arc<Semaphore>,
    /// Caps how many collections index in parallel in one drain pass. Refs
    /// *within* a collection still run serially (the per-collection invariant),
    /// but independent collections proceed concurrently. Same size as
    /// `clone_sem` so embedding load stays bounded.
    job_sem: Arc<Semaphore>,
    /// Every remote file provider this binary knows about. Registering a
    /// provider here is the whole extension point — the build path below
    /// asks for capabilities, never for a product name.
    providers: source::ProviderRegistry,
    /// At-rest key for opening a collection's sealed provider secrets.
    /// `None` in tests that only exercise git collections; a remote source
    /// then fails with a message saying so rather than a decrypt panic.
    crypto: Option<Arc<Crypto>>,
    /// Bytes → text. Degrades rather than fails: with no OCR backend and no
    /// sandbox it still reads every text file, and says why it skipped the
    /// rest.
    extractor: extract::DocumentExtractor,
}

impl Indexer {
    pub fn new(
        db: Pool,
        upstreams: Arc<UpstreamRegistry>,
        http: reqwest::Client,
        mut config: IndexerConfig,
        crypto: Option<Arc<Crypto>>,
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
        // `0` would be a zero-permit semaphore (deadlock); clamp to serial.
        let permits = config.clone_concurrency.max(1);
        Self {
            inner: Arc::new(IndexerInner {
                db,
                upstreams,
                http,
                config,
                indexes: Mutex::new(HashMap::new()),
                stores: Mutex::new(HashMap::new()),
                kick: tokio::sync::Notify::new(),
                clone_sem: Arc::new(Semaphore::new(permits)),
                job_sem: Arc::new(Semaphore::new(permits)),
                providers: source::ProviderRegistry::with_builtins(),
                crypto,
                extractor: extract::DocumentExtractor::new(None, None),
            }),
        }
    }

    pub fn config(&self) -> &IndexerConfig {
        &self.inner.config
    }

    pub fn db(&self) -> &Pool {
        &self.inner.db
    }

    /// Wire in the document readers: an OCR backend for scans and images,
    /// and an office extractor for docx/pptx/xlsx.
    ///
    /// Separate from `new` because both live above this crate's concerns —
    /// the office extractor is implemented in `gateway-runtime`, which sits
    /// on top of this one — and because every existing caller (tests
    /// included) wants the degraded, text-only ladder.
    pub fn with_document_readers(
        mut self,
        ocr: Option<crate::server::ocr::OcrService>,
        office: Option<Arc<dyn extract::OfficeExtractor>>,
    ) -> Self {
        let extractor = extract::DocumentExtractor::new(ocr, office);
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.extractor = extractor,
            // Only reachable if a clone was taken before wiring, which the
            // boot path does not do. Loud rather than silently text-only.
            None => tracing::error!(
                "rag: document readers configured after the indexer was shared; \
                 scans and office files will not be read"
            ),
        }
        self
    }

    /// Queue a **full** rebuild of a ref, discarding incremental state.
    ///
    /// Use whenever a change on *our* side invalidates what is indexed — the
    /// extraction profile, chunking, globs, or the embedding model. A plain
    /// re-index would take the incremental path, find nothing changed at the
    /// source, and leave the corpus exactly as it was.
    pub async fn request_full_rebuild(
        &self,
        ref_id: i64,
    ) -> Result<(), gateway_core::server::db::DbError> {
        rag_db::request_full_rebuild(&self.inner.db, ref_id).await?;
        self.inner.kick.notify_one();
        Ok(())
    }

    /// Every remote source provider this binary knows about.
    ///
    /// The admin UI renders its source picker and credential form from this,
    /// so registering a provider is the only step needed to make it
    /// configurable — no page code changes.
    pub fn providers(&self) -> &source::ProviderRegistry {
        &self.inner.providers
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
    ) -> Result<Pool, gateway_core::server::db::DbError> {
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
        let pool = gateway_core::server::db::open_collection_store(&path).await?;
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
    pub async fn request_reindex(
        &self,
        ref_id: i64,
    ) -> Result<(), gateway_core::server::db::DbError> {
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

    /// Append one entry to a ref's indexing log, then prune the log to its
    /// newest [`Self::LOG_KEEP`] rows. Best-effort: a logging failure must
    /// never fail or abort an index, so errors are swallowed (and logged to
    /// tracing). The log is a diagnostic aid, not part of the build's
    /// correctness.
    async fn log_event(&self, entry: rag_db::NewLogEntry) {
        let ref_id = entry.ref_id;
        if let Err(err) = rag_db::insert_log_entry(&self.inner.db, &entry).await {
            tracing::warn!(ref_id, error = %err, "rag: could not write index-log entry");
            return;
        }
        if let Err(err) = rag_db::prune_log_entries(&self.inner.db, ref_id, Self::LOG_KEEP).await {
            tracing::warn!(ref_id, error = %err, "rag: could not prune index log");
        }
    }

    /// How many log entries to keep per ref. Enough to see the last several
    /// builds (each build writes ~2-3 entries) without growing unbounded.
    const LOG_KEEP: i64 = 50;

    /// (Re-)index one ref. Builds the whole index fresh into a new store
    /// folder and atomically swaps the ref onto it — zero-downtime, since
    /// the ref keeps serving its previous index until the swap. Failures
    /// are recorded against the ref (guarded so a concurrent re-queue isn't
    /// clobbered) and appended to the ref's indexing log.
    pub async fn index_ref(&self, ref_id: i64) -> Result<(), WorkerError> {
        match self.index_ref_inner(ref_id).await {
            Ok(()) => Ok(()),
            Err(err) => {
                // `index_ref_inner` already recorded a context-aware failure
                // (friendly message + log entry) when it had the ref loaded.
                // This is the fallback for errors raised *before* that point
                // (e.g. the central DB is unreadable): the guarded
                // `mark_ref_failed` is a no-op if the inner path already
                // flipped the ref to `error`, so there's no double-write.
                let msg = err.to_string();
                let _ = rag_db::mark_ref_failed(&self.inner.db, ref_id, &msg).await;
                tracing::warn!(ref_id, error = %err, "rag: indexing ref failed");
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
        // Aggregate collections keep ONE unified index, held by the primary
        // ref (its build folds in every source). The other source rows are
        // config only — never built. Park them as `ready` so the poll loop
        // doesn't keep re-picking them; the unified index is what's searched.
        if collection.search_mode == rag_db::SearchMode::Aggregate && !rref.is_primary {
            rag_db::set_ref_status(&self.inner.db, ref_id, rag_db::CollectionStatus::Ready).await?;
            return Ok(());
        }
        let old_uuid = rref.data_uuid.clone();
        // Always build into a *fresh* folder so the live store keeps serving
        // searches until we atomically swap onto the new one.
        let build_uuid = uuid::Uuid::new_v4().to_string();

        let started = Instant::now();
        match self.build_ref(&collection, &rref, &build_uuid).await {
            // Swapped: drop cached handles so searches reopen the new folder,
            // then reap the old store.
            Ok(BuildOutcome::Swapped {
                files,
                chunks,
                commit,
                live_uuid,
                ..
            }) => {
                // Reap the previous folder only when the build actually moved
                // to a new one. An incremental pass reports the same folder,
                // and discarding it would delete the live store.
                if old_uuid != live_uuid {
                    self.evict_ref_caches(ref_id);
                    self.discard_dir(&old_uuid);
                }
                let dur = started.elapsed().as_millis() as i64;
                self.log_event(rag_db::NewLogEntry {
                    ref_id,
                    collection_id: collection.id,
                    level: rag_db::LogLevel::Info,
                    phase: "ready".into(),
                    message: format!(
                        "Indexed {files} file(s), {chunks} chunk(s) at {} in {dur} ms",
                        commit.chars().take(8).collect::<String>()
                    ),
                    commit_sha: Some(commit),
                    files: Some(files as i64),
                    chunks: Some(chunks as i64),
                    duration_ms: Some(dur),
                })
                .await;
                Ok(())
            }
            // Superseded by a re-queue / delete — throw the build away; the
            // live index is untouched. Record it so the timeline explains why
            // a build "vanished" without a ready/error outcome.
            Ok(BuildOutcome::Superseded) => {
                self.discard_dir(&build_uuid);
                self.log_event(rag_db::NewLogEntry {
                    ref_id,
                    collection_id: collection.id,
                    level: rag_db::LogLevel::Info,
                    phase: "queued".into(),
                    message: "Build superseded by a newer re-index request; discarded.".into(),
                    commit_sha: None,
                    files: None,
                    chunks: None,
                    duration_ms: None,
                })
                .await;
                Ok(())
            }
            Err(err) => {
                self.discard_dir(&build_uuid);
                let msg = friendly_error(&err, &rref, &collection);
                let phase = failure_phase(&err);
                // Record the failure against the ref (guarded) and on its log.
                let _ = rag_db::mark_ref_failed(&self.inner.db, ref_id, &msg).await;
                self.log_event(rag_db::NewLogEntry {
                    ref_id,
                    collection_id: collection.id,
                    level: rag_db::LogLevel::Error,
                    phase: phase.into(),
                    message: msg,
                    commit_sha: None,
                    files: None,
                    chunks: None,
                    duration_ms: Some(started.elapsed().as_millis() as i64),
                })
                .await;
                Err(err)
            }
        }
    }

    /// Clone → chunk → embed into `build_uuid`'s fresh store, then
    /// atomically swap the ref onto it. [`BuildOutcome::Swapped`] = now live
    /// (carries the file/chunk counts for the log); [`BuildOutcome::Superseded`]
    /// = the build was superseded (re-queued / deleted) and the caller should
    /// discard it. The build uses *local* store + index handles, never the
    /// cached (live) ones, so concurrent searches keep hitting the old index
    /// until the swap.
    async fn build_ref(
        &self,
        collection: &rag_db::Collection,
        rref: &rag_db::CollectionRef,
        build_uuid: &str,
    ) -> Result<BuildOutcome, WorkerError> {
        let ref_id = rref.id;

        rag_db::set_ref_status(&self.inner.db, ref_id, rag_db::CollectionStatus::Cloning).await?;
        // Timeline entry so the admin sees the build started even while it's
        // still fetching (the status badge also flips to "cloning").
        self.log_event(rag_db::NewLogEntry {
            ref_id,
            collection_id: collection.id,
            level: rag_db::LogLevel::Info,
            phase: "cloning".into(),
            message: if !collection.source.is_git() {
                format!("Listing files from the {} source…", collection.source.kind)
            } else if collection.search_mode == rag_db::SearchMode::Aggregate {
                "Cloning sources…".to_string()
            } else {
                format!(
                    "Cloning '{}' from {}…",
                    rref.git_ref,
                    rref.effective_git_url(collection)
                )
            },
            commit_sha: None,
            files: None,
            chunks: None,
            duration_ms: None,
        })
        .await;
        let clone_dir = self.clone_path(build_uuid);
        let filter = Filter::new(
            &collection.include_globs,
            &collection.exclude_globs,
            self.inner.config.max_file_bytes,
        );

        // A non-git source delegates enumeration to its provider entirely:
        // the walker returns the same (files, marker) pair a clone does, so
        // everything below this point is shared. `git` keeps its own path
        // because a clone materialises the tree on disk, which is cheaper to
        // read than re-fetching each file.
        if !collection.source.is_git() {
            // A ref that has already built once updates its live store in
            // place: re-fetching and re-embedding an unchanged corpus every
            // night is the difference between a feature an operator runs and
            // one they turn off. The first build still goes through the
            // fresh-folder path, so there is nothing to be half-way through.
            // A ref whose extractor set has changed since it was built must
            // go the long way round: the files it skipped for want of OCR or
            // the sandbox are readable now, and an incremental diff would
            // find them unchanged at the source and never look again.
            let extractors_changed = rref.extractor_fingerprint.as_deref()
                != Some(self.inner.extractor.fingerprint().as_str());
            if !rref.needs_full_rebuild() && !extractors_changed {
                return self.build_ref_incremental(collection, rref, &filter).await;
            }
            if extractors_changed {
                tracing::info!(
                    ref_id,
                    was = ?rref.extractor_fingerprint,
                    now = %self.inner.extractor.fingerprint(),
                    "rag: document extractors changed, rebuilding rather than diffing"
                );
            }
            let (items, head, snapshot, provider) = self
                .gather_remote(collection, rref, &filter, &Default::default())
                .await?;
            if self.superseded(ref_id).await? {
                return Ok(BuildOutcome::Superseded);
            }
            let outcome = self
                .index_items(collection, rref, build_uuid, items, head, Some(provider))
                .await?;
            // Remember directory versions only after the build is live AND
            // the walk saw the whole tree. Storing them from a partial walk
            // would let a later sync prune a subtree whose files were never
            // indexed — the corpus would quietly lose documents and look
            // healthy doing it.
            let read_everything = matches!(
                outcome,
                BuildOutcome::Swapped {
                    all_items_read: true,
                    ..
                }
            );
            if read_everything && snapshot.is_complete() {
                rag_db::set_ref_sync_state(&self.inner.db, ref_id, &snapshot.dir_versions).await?;
            }
            return Ok(outcome);
        }

        // Gather the files to index plus a commit marker. Two shapes:
        //   * Versioned ref → clone its one repo; index it as-is.
        //   * Aggregate primary ref → this ref IS the collection's single
        //     unified index. Clone EVERY source repo into `clone/<label>/`
        //     and index the combined tree under that prefix, so the whole
        //     collection is one searchable corpus (global dense + lexical
        //     ranking) with self-describing paths like
        //     `pve-manager/src/PVE/HA/NodeStatus.pm`.
        let (walked, head) = if collection.search_mode == rag_db::SearchMode::Aggregate {
            let sources = rag_db::list_refs(&self.inner.db, collection.id).await?;
            // Clone every source concurrently, bounded by `clone_sem`, so one
            // slow repo doesn't hold up the rest of the corpus. The unified
            // index still needs all sources before it can build, but the clones
            // now overlap — total clone time is the slowest single clone, not
            // their sum. `JoinSet` aborts any in-flight clones if we return
            // early (error / supersede), so nothing is left running detached.
            let mut set: JoinSet<(usize, String, PathBuf, Result<String, GitError>)> =
                JoinSet::new();
            for (i, src) in sources.iter().enumerate() {
                let label = src.source_label(collection);
                let sub = clone_dir.join(&label);
                let url = src.effective_git_url(collection).to_string();
                let git_ref = src.git_ref.clone();
                let pat = collection.pat.clone();
                let sem = Arc::clone(&self.inner.clone_sem);
                set.spawn(async move {
                    // Permit frees a clone slot the moment this clone finishes.
                    let _permit = sem.acquire_owned().await;
                    let sha = git::clone_or_update(&url, &git_ref, pat.as_deref(), &sub).await;
                    (i, label, sub, sha)
                });
            }
            let mut cloned: Vec<(usize, String, PathBuf, String)> =
                Vec::with_capacity(sources.len());
            while let Some(joined) = set.join_next().await {
                let (i, label, sub, sha) = joined.map_err(|e| {
                    WorkerError::Io(std::io::Error::other(format!(
                        "clone task failed to join: {e}"
                    )))
                })?;
                cloned.push((i, label, sub, sha?));
            }
            // A re-queue/delete during the (possibly long) clone fan-out
            // supersedes this build — bail before the expensive walk + embed.
            if self.superseded(ref_id).await? {
                return Ok(BuildOutcome::Superseded);
            }
            // Restore source order (clones complete out of order) so the commit
            // marker is stable across runs regardless of clone timing.
            cloned.sort_by_key(|(i, ..)| *i);
            let mut files: Vec<walk::WalkedFile> = Vec::new();
            let mut commits: Vec<String> = Vec::new();
            for (_, label, sub, sha) in &cloned {
                commits.push(format!("{label}:{sha}"));
                for mut wf in walk::walk(sub, &filter)? {
                    wf.rel_path = format!("{label}/{}", wf.rel_path);
                    files.push(wf);
                }
            }
            // Deterministic order across runs (and a stable commit marker that
            // changes whenever any source's head moves).
            files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
            (files, sha256_hex(&commits.join("\n")))
        } else {
            let sha = {
                // One clone slot, shared with every other collection's clones.
                let _permit = self
                    .inner
                    .clone_sem
                    .acquire()
                    .await
                    .expect("clone semaphore is never closed");
                git::clone_or_update(
                    rref.effective_git_url(collection),
                    &rref.git_ref,
                    collection.pat.as_deref(),
                    &clone_dir,
                )
                .await?
            };
            if self.superseded(ref_id).await? {
                return Ok(BuildOutcome::Superseded);
            }
            (walk::walk(&clone_dir, &filter)?, sha)
        };

        let items: Vec<BuildItem> = walked
            .into_iter()
            .map(|w| BuildItem::Disk {
                rel_path: w.rel_path,
                abs_path: w.abs_path,
            })
            .collect();
        self.index_items(collection, rref, build_uuid, items, head, None)
            .await
    }

    /// Enumerate a non-git source through its provider.
    ///
    /// Returns the files to index, a marker that changes iff the remote
    /// changed, the snapshot (so the caller can decide whether it may be
    /// trusted), and the live provider handle the fetch step needs.
    async fn gather_remote(
        &self,
        collection: &rag_db::Collection,
        rref: &rag_db::CollectionRef,
        filter: &Filter,
        prior: &source::tree::DirVersions,
    ) -> Result<
        (
            Vec<BuildItem>,
            String,
            source::tree::TreeSnapshot,
            Arc<dyn source::FileProvider>,
        ),
        WorkerError,
    > {
        let provider = self.build_provider(collection)?;
        let opts = source::tree::WalkOptions {
            max_file_bytes: self.inner.config.max_file_bytes,
            concurrency: self.inner.config.clone_concurrency.max(1),
            ..Default::default()
        };
        // A full rebuild passes an empty `prior` — it writes into a fresh
        // folder, so a pruned subtree would contribute no files and its
        // documents would vanish. An incremental pass passes the stored
        // versions and carries pruned subtrees over untouched, which is what
        // makes an unchanged corpus cost one request per branch.
        let snapshot = source::tree::walk(Arc::clone(&provider), prior, filter, &opts).await?;

        for failed in &snapshot.failed {
            self.log_event(rag_db::NewLogEntry {
                ref_id: rref.id,
                collection_id: collection.id,
                level: rag_db::LogLevel::Warn,
                phase: "cloning".into(),
                message: format!(
                    "Could not list `{}`: {} — its files are not in this build.",
                    if failed.rel_path.is_empty() {
                        "/"
                    } else {
                        failed.rel_path.as_str()
                    },
                    failed.message
                ),
                commit_sha: None,
                files: None,
                chunks: None,
                duration_ms: None,
            })
            .await;
        }
        // Nothing listed at all is a broken source, not an empty one. Raised
        // as a failure so the ref goes `error` with the reason instead of
        // going `ready` with zero documents.
        if snapshot.files.is_empty() && !snapshot.failed.is_empty() {
            return Err(WorkerError::Source(source::ProviderError::Config(format!(
                "no directory could be listed ({} failed); first error: {}",
                snapshot.failed.len(),
                snapshot.failed[0].message
            ))));
        }
        if snapshot.oversized > 0 || snapshot.filtered_out > 0 {
            self.log_event(rag_db::NewLogEntry {
                ref_id: rref.id,
                collection_id: collection.id,
                level: rag_db::LogLevel::Info,
                phase: "indexing".into(),
                message: format!(
                    "Listed {} file(s); skipped {} over the size limit and {} not matching the \
                     include/exclude globs.",
                    snapshot.files.len(),
                    snapshot.oversized,
                    snapshot.filtered_out
                ),
                commit_sha: None,
                files: Some(snapshot.files.len() as i64),
                chunks: None,
                duration_ms: None,
            })
            .await;
        }

        let marker = snapshot.marker();
        let items = snapshot
            .files
            .iter()
            .cloned()
            .map(BuildItem::Remote)
            .collect();
        Ok((items, marker, snapshot, provider))
    }

    /// Fetch one item's bytes and run them through the extraction ladder.
    ///
    /// The ladder (`extract::DocumentExtractor`) is what turns a PDF, a scan
    /// or an office file into text; before it existed, everything that was
    /// not UTF-8 was dropped here without a log line. Which rung applies is
    /// decided by type, not by source, so a git repo full of PDFs reads
    /// exactly as a WebDAV folder full of them does.
    async fn read_item(
        &self,
        item: &BuildItem,
        provider: Option<&Arc<dyn source::FileProvider>>,
    ) -> ItemRead {
        let (bytes, mime) = match item {
            BuildItem::Disk { abs_path, .. } => match std::fs::read(abs_path) {
                Ok(b) => (b, None),
                Err(e) => return ItemRead::Failed(e.to_string()),
            },
            BuildItem::Remote(entry) => {
                let Some(provider) = provider else {
                    return ItemRead::Failed("no provider for a remote item".into());
                };
                match provider
                    .fetch(entry, self.inner.config.max_file_bytes)
                    .await
                {
                    Ok(b) => (b, entry.mime.clone()),
                    Err(e) => return ItemRead::Failed(e.to_string()),
                }
            }
        };
        match self
            .inner
            .extractor
            .extract(item.rel_path(), mime.as_deref(), bytes)
            .await
        {
            extract::Extracted::Ok(doc) if doc.is_empty() => {
                // A document that extracted to nothing (a blank scan, an
                // empty file) is not an error, but indexing zero chunks for
                // it and saying nothing would look like a silent drop.
                ItemRead::Unsupported("extracted no text".into())
            }
            extract::Extracted::Ok(doc) => ItemRead::Ok(doc),
            extract::Extracted::Unsupported { reason } => ItemRead::Unsupported(reason),
            extract::Extracted::Failed(err) => ItemRead::Failed(err),
        }
    }

    /// The collection's extraction profile, if it has one.
    ///
    /// A configured-but-missing profile is a config error worth naming: the
    /// alternative is a corpus indexed without fields and an operator
    /// wondering why the query tool finds nothing.
    async fn resolve_profile(
        &self,
        collection: &rag_db::Collection,
        rref: &rag_db::CollectionRef,
    ) -> Result<Option<docs_db::Profile>, WorkerError> {
        let Some(id) = collection.profile_id else {
            return Ok(None);
        };
        if let Some(p) = docs_db::find_profile(&self.inner.db, id).await? {
            return Ok(Some(p));
        }
        self.log_event(rag_db::NewLogEntry {
            ref_id: rref.id,
            collection_id: collection.id,
            level: rag_db::LogLevel::Warn,
            phase: "indexing".into(),
            message: format!(
                "Extraction profile {id} no longer exists; indexing without document \
                 fields. Pick a profile on the collection to restore structured queries."
            ),
            commit_sha: None,
            files: None,
            chunks: None,
            duration_ms: None,
        })
        .await;
        Ok(None)
    }

    /// Read, extract, chunk, embed and store one item. Returns the number of
    /// chunks written, or `None` when the item produced nothing indexable.
    ///
    /// Shared by the full rebuild and the incremental pass so the two cannot
    /// drift — the failure that would produce is a corpus whose incrementally
    /// updated documents are subtly unlike its rebuilt ones.
    #[allow(clippy::too_many_arguments)]
    async fn index_one_item(
        &self,
        collection: &rag_db::Collection,
        store: &Pool,
        index: &CollectionIndex,
        profile: Option<&docs_db::Profile>,
        item: &BuildItem,
        provider: Option<&Arc<dyn source::FileProvider>>,
        next_vector_id: &mut i64,
        replaces: Option<i64>,
    ) -> Result<ItemOutcome, WorkerError> {
        let doc = match self.read_item(item, provider).await {
            ItemRead::Ok(doc) => doc,
            ItemRead::Unsupported(reason) => return Ok(ItemOutcome::Skipped(reason)),
            // A fetch that 503'd or an OCR backend that was down is a
            // *transient* nothing, not a permanent one. Reported separately
            // so the caller can decline to record this pass as authoritative
            // — otherwise the file's directory version would be stored and
            // the next sync would prune right past it.
            ItemRead::Failed(err) => {
                tracing::debug!(path = %item.rel_path(), error = %err, "rag: item read failed");
                return Ok(ItemOutcome::Failed);
            }
        };
        // Only now that the replacement content is in hand is it safe to drop
        // what is being replaced. Dropping first and then failing the read
        // would leave the file row with zero chunks and nothing to notice it.
        if let Some(old) = replaces {
            // The whole row, not just its chunks. A file that moved *and*
            // changed arrives here with a new `rel_path`, so the `upsert_file`
            // below finds no conflict and inserts a second row — leaving the
            // old one behind with zero chunks but an intact `rag_documents`
            // join, which `rag_query_documents` and `rag_list_documents`
            // happily keep reporting at the path the file no longer has. The
            // two rows also share a `remote_id`, so the next `sync::plan`
            // collides on identity and can re-upsert the file every pass.
            //
            // Dropping the row cascades its document away; both are rebuilt
            // from the content just read.
            self.drop_file(store, index, old).await?;
        }
        let paginated = doc.extractor != extract::Extractor::Text;
        let size = collection.chunk_size as usize;
        let overlap = collection.chunk_overlap as usize;
        let text = doc.pages.join("\n");
        let pieces = if paginated {
            chunk::chunk_pages(&doc.pages, size, overlap)
        } else {
            chunk::chunk_lines(&text, size, overlap)
        };
        if pieces.is_empty() {
            return Ok(ItemOutcome::Skipped("produced no text".to_string()));
        }
        let hash = sha256_hex(&text);
        let (web_url, remote_id, source_version) = match (item, provider) {
            (BuildItem::Remote(entry), Some(p)) => (
                p.web_url(entry),
                Some(entry.id.clone()),
                Some(entry.version.clone()),
            ),
            _ => (None, None, None),
        };
        let file_id = rag_db::upsert_file(
            store,
            collection.id,
            item.rel_path(),
            &hash,
            &rag_db::FileOrigin {
                web_url: web_url.as_deref(),
                remote_id: remote_id.as_deref(),
                source_version: source_version.as_deref(),
            },
        )
        .await?;

        let extraction = match profile {
            None => None,
            Some(p) => match profile::extract(
                &self.inner.http,
                &self.inner.upstreams,
                &self.inner.db,
                p,
                collection.extraction_model.as_deref(),
                &hash,
                &text,
                self.inner.config.extraction_max_input_chars,
            )
            .await
            {
                Ok(e) => {
                    let values = profile::to_field_values(p, &e.fields);
                    docs_db::upsert_document(
                        store,
                        file_id,
                        &docs_db::DocumentMeta {
                            title: e.fields.get("title").map(String::as_str),
                            summary: e.summary.as_deref(),
                            extractor: doc.extractor.as_str(),
                            pages_total: doc.pages_total.map(|n| n as i64),
                            pages_processed: doc.pages_processed.map(|n| n as i64),
                        },
                        &values,
                    )
                    .await?;
                    Some(e)
                }
                Err(err) => {
                    tracing::debug!(
                        path = %item.rel_path(), error = %err,
                        "rag: profile extraction failed"
                    );
                    None
                }
            },
        };
        let context_header = extraction
            .as_ref()
            .map(|e| e.context_header(item.rel_path()));

        let mut written = 0usize;
        for batch in pieces.chunks(self.inner.config.embed_batch_size) {
            let inputs: Vec<String> = batch
                .iter()
                .map(|p| match context_header.as_deref() {
                    Some(header) => format!("{header}\n{}", p.content),
                    None => p.content.clone(),
                })
                .collect();
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
            if vectors[0].len() != index.dimensions() {
                return Err(WorkerError::Index(IndexError::BadVectorLen {
                    expected: index.dimensions(),
                    got: vectors[0].len(),
                }));
            }
            let mut new_chunks: Vec<rag_db::NewChunk> = Vec::with_capacity(batch.len());
            let mut to_index: Vec<(i64, &[f32])> = Vec::with_capacity(batch.len());
            for (piece, vec) in batch.iter().zip(vectors.iter()) {
                let vid = *next_vector_id;
                *next_vector_id += 1;
                let loc = if paginated {
                    rag_db::ChunkLoc::pages(piece.from as i64, piece.to as i64)
                } else {
                    rag_db::ChunkLoc::lines(piece.from as i64, piece.to as i64)
                };
                new_chunks.push(rag_db::NewChunk {
                    file_id,
                    chunk_index: piece.chunk_index as i64,
                    loc,
                    content: piece.content.clone(),
                    vector_id: vid,
                });
                to_index.push((vid, vec.as_slice()));
            }
            rag_db::insert_chunks(store, collection.id, &new_chunks).await?;
            for (vid, vec) in to_index {
                index.add(vid, vec)?;
            }
            written += new_chunks.len();
        }
        Ok(ItemOutcome::Indexed(written))
    }

    /// Update a ref's live store in place, doing only the work the source's
    /// changes require.
    ///
    /// This forfeits the fresh-folder-and-swap safety of a full rebuild, so
    /// three things replace it:
    ///
    ///   * **Per-file transactionality.** One file's delete-then-insert is
    ///     one SQLite transaction, bracketed by its vector removals and adds.
    ///     A crash leaves at most one file inconsistent.
    ///   * **The diff is the resume cursor.** Directory versions are stored
    ///     only after a fully successful pass, so an interrupted run costs
    ///     one extra walk and nothing else: the next diff sees the files it
    ///     already indexed as unchanged and skips them.
    ///   * **Deletions need an authoritative walk.** A folder that errored is
    ///     indistinguishable from a folder that was emptied, so a partial
    ///     walk deletes nothing (see `sync::plan`).
    async fn build_ref_incremental(
        &self,
        collection: &rag_db::Collection,
        rref: &rag_db::CollectionRef,
        filter: &Filter,
    ) -> Result<BuildOutcome, WorkerError> {
        let ref_id = rref.id;
        rag_db::set_ref_status(&self.inner.db, ref_id, rag_db::CollectionStatus::Cloning).await?;

        let (items, head, snapshot, provider) = self
            .gather_remote(collection, rref, filter, &rref.dir_versions)
            .await?;
        if self.superseded(ref_id).await? {
            return Ok(BuildOutcome::Superseded);
        }

        let store = self.collection_store(ref_id, &rref.data_uuid).await?;
        let existing = rag_db::list_files_for_collection(&store, collection.id).await?;
        let indexed: Vec<sync::IndexedState> = existing
            .iter()
            .map(|f| sync::IndexedState {
                file_id: f.id,
                path: f.path.clone(),
                remote_id: f.remote_id.clone(),
                source_version: f.source_version.clone(),
            })
            .collect();

        let entries: Vec<source::RemoteEntry> = items
            .iter()
            .filter_map(|i| match i {
                BuildItem::Remote(e) => Some(e.clone()),
                BuildItem::Disk { .. } => None,
            })
            .collect();
        let mut plan = sync::plan(
            &entries,
            &indexed,
            provider.capabilities().stable_ids,
            snapshot.is_complete(),
        );
        // A pruned subtree contributed no entries, so its files look absent.
        // They are not: nothing beneath that directory changed. Dropping this
        // would make the first cheap re-sync delete most of the corpus.
        let keep = sync::keep_pruned(&indexed, &snapshot.pruned);
        plan.deletions.retain(|id| !keep.contains(id));

        rag_db::set_ref_status(&self.inner.db, ref_id, rag_db::CollectionStatus::Indexing).await?;
        self.log_event(rag_db::NewLogEntry {
            ref_id,
            collection_id: collection.id,
            level: rag_db::LogLevel::Info,
            phase: "indexing".into(),
            message: format!("Incremental sync: {}.", plan.summary()),
            commit_sha: None,
            files: Some(plan.upserts.len() as i64),
            chunks: None,
            duration_ms: None,
        })
        .await;

        let index = self.open_index(ref_id, &rref.data_uuid, None)?;
        // Deletions first, so a file moving onto a path this pass is about to
        // free does not collide with the row still holding it.
        let mut removed_vectors = 0usize;
        for file_id in &plan.deletions {
            removed_vectors += self.drop_file(&store, &index, *file_id).await?;
        }
        // Then every move at once: applied one by one, two files swapping
        // paths would violate `UNIQUE (collection_id, path)` in either order
        // and fail the whole pass.
        rag_db::rename_files(&store, &plan.renames).await?;

        let mut indexed_files = 0usize;
        let mut added_chunks = 0usize;
        let mut item_failures = 0usize;
        // Grouped by reason, exactly as the full rebuild does — this is the
        // pass that runs nightly, so it is the one whose silence would last.
        let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
        if !plan.upserts.is_empty() {
            let mut next_vector_id = rag_db::max_vector_id(&store, collection.id)
                .await?
                .unwrap_or(0)
                + 1;
            let profile = self.resolve_profile(collection, rref).await?;
            for upsert in &plan.upserts {
                if self.superseded(ref_id).await? {
                    // Persist what was indexed so far before bailing: the
                    // store rows are already committed, and an index file
                    // that does not match them would answer with vectors
                    // whose chunks are gone.
                    index.save()?;
                    return Ok(BuildOutcome::Superseded);
                }
                let item = BuildItem::Remote(upsert.entry.clone());
                let outcome = match self
                    .index_one_item(
                        collection,
                        &store,
                        &index,
                        profile.as_ref(),
                        &item,
                        Some(&provider),
                        &mut next_vector_id,
                        upsert.replaces,
                    )
                    .await
                {
                    Ok(o) => o,
                    Err(err) => {
                        index.save()?;
                        return Err(err);
                    }
                };
                match outcome {
                    ItemOutcome::Indexed(n) => {
                        indexed_files += 1;
                        added_chunks += n;
                    }
                    ItemOutcome::Skipped(reason) => {
                        *skipped.entry(reason).or_default() += 1;
                    }
                    ItemOutcome::Failed => {
                        item_failures += 1;
                        *skipped.entry("extraction failed".to_string()).or_default() += 1;
                    }
                }
            }
        }
        index.save()?;

        // Only now, with every change applied, are the directory versions
        // safe to store: recording them earlier would let the next pass prune
        // a subtree whose files this pass never got to.
        let outcome = rag_db::swap_ref_index(
            &self.inner.db,
            ref_id,
            &rref.data_uuid,
            &head,
            &self.inner.extractor.fingerprint(),
        )
        .await?;
        if outcome != 1 {
            // A re-queue flipped the ref away from `indexing` while we
            // worked. The store is still consistent — every change was
            // applied per file — so the next pass simply diffs from here.
            return Ok(BuildOutcome::Superseded);
        }
        // Directory versions are only safe to record when this pass actually
        // read everything it set out to. `is_complete()` covers directories
        // that failed to *list*; `item_failures` covers files that failed to
        // *fetch or extract*. Recording either would let the next sync prune
        // past a document that was never indexed — and nothing would retry it
        // until something else in that folder changed.
        if snapshot.is_complete() && item_failures == 0 {
            let merged = sync::merged_dir_versions(&rref.dir_versions, &snapshot.dir_versions);
            rag_db::set_ref_sync_state(&self.inner.db, ref_id, &merged).await?;
        } else if item_failures > 0 {
            self.log_event(rag_db::NewLogEntry {
                ref_id,
                collection_id: collection.id,
                level: rag_db::LogLevel::Warn,
                phase: "ready".into(),
                message: format!(
                    "{item_failures} document(s) could not be read this pass; the next sync \
                     will re-walk and retry them rather than assume they are up to date."
                ),
                commit_sha: None,
                files: None,
                chunks: None,
                duration_ms: None,
            })
            .await;
        }
        let skipped_total: usize = skipped.values().sum();
        if skipped_total > 0 {
            self.log_event(rag_db::NewLogEntry {
                ref_id,
                collection_id: collection.id,
                level: rag_db::LogLevel::Warn,
                phase: "ready".into(),
                message: format!(
                    "{skipped_total} document(s) changed at the source but could not be read. {}",
                    skip_summary(&skipped)
                ),
                commit_sha: None,
                files: None,
                chunks: None,
                duration_ms: None,
            })
            .await;
        }
        self.log_event(rag_db::NewLogEntry {
            ref_id,
            collection_id: collection.id,
            level: rag_db::LogLevel::Info,
            phase: "ready".into(),
            message: format!(
                "Synced {indexed_files} document(s), {added_chunks} chunk(s) added, \
                 {removed_vectors} vector(s) removed; {} unchanged.",
                plan.unchanged
            ),
            commit_sha: Some(head.clone()),
            files: Some(indexed_files as i64),
            chunks: Some(added_chunks as i64),
            duration_ms: None,
        })
        .await;

        Ok(BuildOutcome::Swapped {
            files: indexed_files,
            chunks: added_chunks,
            commit: head,
            live_uuid: rref.data_uuid.clone(),
            all_items_read: item_failures == 0,
        })
    }

    /// Remove one file's chunks and their vectors. Returns how many vectors
    /// went.
    async fn drop_chunks(
        &self,
        store: &Pool,
        index: &CollectionIndex,
        file_id: i64,
    ) -> Result<usize, WorkerError> {
        let ids = rag_db::vector_ids_for_file(store, file_id).await?;
        for id in &ids {
            // A vector the index does not have is not an error: the store is
            // the record of truth and the index is derived from it.
            let _ = index.remove(*id)?;
        }
        rag_db::delete_chunks_for_file(store, file_id).await?;
        Ok(ids.len())
    }

    /// Remove a file entirely — chunks, vectors, and the row itself. The
    /// extracted document row goes with it via `ON DELETE CASCADE`.
    async fn drop_file(
        &self,
        store: &Pool,
        index: &CollectionIndex,
        file_id: i64,
    ) -> Result<usize, WorkerError> {
        let removed = self.drop_chunks(store, index, file_id).await?;
        rag_db::delete_file(store, file_id).await?;
        Ok(removed)
    }

    /// Instantiate the collection's provider, opening its sealed secrets.
    fn build_provider(
        &self,
        collection: &rag_db::Collection,
    ) -> Result<Arc<dyn source::FileProvider>, WorkerError> {
        let secrets = match collection.source.secrets.as_ref() {
            None => std::collections::BTreeMap::new(),
            Some(sealed) => {
                let crypto = self.inner.crypto.as_ref().ok_or_else(|| {
                    source::ProviderError::Config(
                        "this collection has stored credentials but the gateway has no at-rest \
                         encryption key configured, so they cannot be opened"
                            .into(),
                    )
                })?;
                let plain = crypto
                    .open_str(&sealed.nonce, &sealed.ciphertext)
                    .map_err(|e| {
                        source::ProviderError::Config(format!(
                            "stored credentials could not be decrypted ({e}) — re-enter them; \
                             this usually means the at-rest key changed"
                        ))
                    })?;
                serde_json::from_str(&plain).map_err(|e| {
                    source::ProviderError::Config(format!("stored credentials are corrupt: {e}"))
                })?
            }
        };
        let cfg = source::ProviderConfig::new(collection.source.config.clone(), secrets);
        Ok(self
            .inner
            .providers
            .build(&collection.source.kind, &cfg, self.inner.http.clone())?)
    }

    /// Chunk, embed and index `items` into a fresh store, then swap the ref
    /// onto it.
    ///
    /// Shared by every source: once enumeration has produced a list of items
    /// and a marker, nothing below here cares where they came from. That is
    /// what keeps adding a provider from touching the indexing path.
    async fn index_items(
        &self,
        collection: &rag_db::Collection,
        rref: &rag_db::CollectionRef,
        build_uuid: &str,
        items: Vec<BuildItem>,
        head: String,
        provider: Option<Arc<dyn source::FileProvider>>,
    ) -> Result<BuildOutcome, WorkerError> {
        let ref_id = rref.id;
        rag_db::set_ref_status(&self.inner.db, ref_id, rag_db::CollectionStatus::Indexing).await?;

        let profile = self.resolve_profile(collection, rref).await?;
        let mut extracted_docs = 0usize;
        let mut extraction_failures = 0usize;

        // Fresh, uncached store + index for this build.
        let store = gateway_core::server::db::open_collection_store(
            &self.collection_dir(build_uuid).join("rag.sqlite"),
        )
        .await?;
        let index_path = self.index_path(build_uuid);

        let mut next_vector_id = 1i64;
        let mut indexed_files = 0usize;
        let mut dimensions: Option<usize> = None;
        let mut index: Option<CollectionIndex> = None;

        let mut unreadable = 0usize;
        let mut item_failures = 0usize;
        let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
        let mut partial: Vec<String> = Vec::new();
        for file in &items {
            let doc = match self.read_item(file, provider.as_ref()).await {
                ItemRead::Ok(doc) => doc,
                // Nothing configured here can read this type. Counted and
                // grouped by reason rather than dropped in silence, so a
                // corpus of scans with OCR switched off says exactly that
                // instead of looking like an empty collection.
                ItemRead::Unsupported(reason) => {
                    unreadable += 1;
                    *skipped.entry(reason).or_default() += 1;
                    continue;
                }
                ItemRead::Failed(err) => {
                    tracing::debug!(path = %file.rel_path(), error = %err, "rag: item unreadable");
                    unreadable += 1;
                    // Transient: the corpus this pass saw is not the corpus
                    // that exists, so its directory versions must not be
                    // recorded as authoritative.
                    item_failures += 1;
                    *skipped.entry("extraction failed".to_string()).or_default() += 1;
                    continue;
                }
            };
            // A document read only in part must be visible as such: an
            // answer sourced from 8 of 40 pages is not an answer about the
            // document.
            if !doc.complete() {
                partial.push(format!("{} ({})", file.rel_path(), doc.coverage_note()));
            }
            // Plain text keeps line positions; anything that went through
            // extraction is positioned by page, because lines do not survive
            // a PDF and a page is what a person can open the original to.
            let paginated = doc.extractor != extract::Extractor::Text;
            let size = collection.chunk_size as usize;
            let overlap = collection.chunk_overlap as usize;
            let text = doc.pages.join("\n");
            let pieces = if paginated {
                chunk::chunk_pages(&doc.pages, size, overlap)
            } else {
                chunk::chunk_lines(&text, size, overlap)
            };
            if pieces.is_empty() {
                continue;
            }
            let hash = sha256_hex(&text);
            // The provider's own link to this file, so an answer can be
            // checked against the original instead of taken on trust.
            let (web_url, remote_id, source_version) = match (file, provider.as_ref()) {
                (BuildItem::Remote(entry), Some(p)) => (
                    p.web_url(entry),
                    Some(entry.id.clone()),
                    Some(entry.version.clone()),
                ),
                _ => (None, None, None),
            };
            let file_id = rag_db::upsert_file(
                &store,
                collection.id,
                file.rel_path(),
                &hash,
                &rag_db::FileOrigin {
                    web_url: web_url.as_deref(),
                    remote_id: remote_id.as_deref(),
                    source_version: source_version.as_deref(),
                },
            )
            .await?;
            indexed_files += 1;

            // The profile pass: fields + a summary, cached by content hash so
            // a rebuild re-embeds without re-running the model.
            let extraction = match profile.as_ref() {
                None => None,
                Some(p) => {
                    match profile::extract(
                        &self.inner.http,
                        &self.inner.upstreams,
                        &self.inner.db,
                        p,
                        collection.extraction_model.as_deref(),
                        &hash,
                        &text,
                        self.inner.config.extraction_max_input_chars,
                    )
                    .await
                    {
                        Ok(e) => {
                            extracted_docs += 1;
                            let values = profile::to_field_values(p, &e.fields);
                            let title = e.fields.get("title").map(String::as_str);
                            docs_db::upsert_document(
                                &store,
                                file_id,
                                &docs_db::DocumentMeta {
                                    title,
                                    summary: e.summary.as_deref(),
                                    extractor: doc.extractor.as_str(),
                                    pages_total: doc.pages_total.map(|n| n as i64),
                                    pages_processed: doc.pages_processed.map(|n| n as i64),
                                },
                                &values,
                            )
                            .await?;
                            Some(e)
                        }
                        // One document's extraction failing must not fail the
                        // build: the text is still worth indexing, it just
                        // will not answer structured queries.
                        Err(err) => {
                            extraction_failures += 1;
                            tracing::debug!(
                                path = %file.rel_path(), error = %err,
                                "rag: profile extraction failed"
                            );
                            None
                        }
                    }
                }
            };
            // Prepended to each chunk *before embedding*, never to the stored
            // text. A bare paragraph from page 2 of an invoice is
            // embedding-identical to the same paragraph in 400 others; this
            // header is what separates them in vector space.
            let context_header = extraction
                .as_ref()
                .map(|e| e.context_header(file.rel_path()));

            for batch in pieces.chunks(self.inner.config.embed_batch_size) {
                // Abort early if a re-queue / delete superseded this build,
                // so we don't burn embedding calls on a doomed run.
                if self.superseded(ref_id).await? {
                    return Ok(BuildOutcome::Superseded);
                }
                let inputs: Vec<String> = batch
                    .iter()
                    .map(|p| match context_header.as_deref() {
                        Some(header) => format!("{header}\n{}", p.content),
                        None => p.content.clone(),
                    })
                    .collect();
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
                let mut to_index: Vec<(i64, &[f32])> = Vec::with_capacity(batch.len());
                for (piece, vec) in batch.iter().zip(vectors.iter()) {
                    let vid = next_vector_id;
                    next_vector_id += 1;
                    let loc = if paginated {
                        rag_db::ChunkLoc::pages(piece.from as i64, piece.to as i64)
                    } else {
                        rag_db::ChunkLoc::lines(piece.from as i64, piece.to as i64)
                    };
                    new_chunks.push(rag_db::NewChunk {
                        file_id,
                        chunk_index: piece.chunk_index as i64,
                        loc,
                        content: piece.content.clone(),
                        vector_id: vid,
                    });
                    to_index.push((vid, vec.as_slice()));
                }
                rag_db::insert_chunks(&store, collection.id, &new_chunks).await?;
                for (vid, vec) in to_index {
                    idx.add(vid, vec)?;
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
        let swapped = rag_db::swap_ref_index(
            &self.inner.db,
            ref_id,
            build_uuid,
            &head,
            &self.inner.extractor.fingerprint(),
        )
        .await?
            == 1;
        if !swapped {
            // Lost the race: a re-queue flipped the ref away from `indexing`
            // while we built. Discard quietly; the live index is untouched.
            return Ok(BuildOutcome::Superseded);
        }
        let chunks = (next_vector_id - 1) as usize;
        // `next_vector_id` starts at 1 and increments per indexed chunk, so it
        // is still 1 iff nothing was indexed. An empty index that's silently
        // "ready" almost always means the include globs matched no files —
        // surface that instead of letting it look healthy.
        if chunks == 0 {
            tracing::warn!(
                ref_id,
                files = items.len(),
                "ref indexed 0 chunks — include globs likely match nothing"
            );
            let warning = if unreadable > 0 {
                format!(
                    "Indexed 0 files: {unreadable} file(s) were found but none could be read. \
                     {}",
                    skip_summary(&skipped)
                )
            } else {
                "Indexed 0 files — nothing matched the collection's include globs. Check the \
                 include patterns (e.g. add *.pm, *.js, *.adoc for non-Rust repos)."
                    .to_string()
            };
            // Keep the advisory on `last_error` (shown as the ref's headline)
            // AND on the timeline.
            let _ = rag_db::set_ref_warning(&self.inner.db, ref_id, &warning).await;
            self.log_event(rag_db::NewLogEntry {
                ref_id,
                collection_id: collection.id,
                level: rag_db::LogLevel::Warn,
                phase: "ready".into(),
                message: warning.clone(),
                commit_sha: Some(head.clone()),
                files: Some(0),
                chunks: Some(0),
                duration_ms: None,
            })
            .await;
        }
        if profile.is_some() && (extracted_docs > 0 || extraction_failures > 0) {
            let level = if extraction_failures > 0 {
                rag_db::LogLevel::Warn
            } else {
                rag_db::LogLevel::Info
            };
            self.log_event(rag_db::NewLogEntry {
                ref_id,
                collection_id: collection.id,
                level,
                phase: "ready".into(),
                message: format!(
                    "Extracted document fields from {extracted_docs} document(s); \
                     {extraction_failures} failed."
                ),
                commit_sha: None,
                files: Some(extracted_docs as i64),
                chunks: None,
                duration_ms: None,
            })
            .await;
        }

        // A build that indexed *something* can still have quietly dropped
        // half the corpus. Report it: "ready" next to 3000 unreadable scans
        // is the most expensive kind of silence in this system.
        if chunks > 0 && unreadable > 0 {
            self.log_event(rag_db::NewLogEntry {
                ref_id,
                collection_id: collection.id,
                level: rag_db::LogLevel::Warn,
                phase: "ready".into(),
                message: format!(
                    "Skipped {unreadable} of {} file(s). {}",
                    items.len(),
                    skip_summary(&skipped)
                ),
                commit_sha: None,
                files: Some(indexed_files as i64),
                chunks: None,
                duration_ms: None,
            })
            .await;
        }
        if !partial.is_empty() {
            // Naming a few is more actionable than a count alone, and the
            // full list would flood the timeline on a big corpus.
            let shown: Vec<&str> = partial.iter().take(5).map(String::as_str).collect();
            let more = partial.len().saturating_sub(shown.len());
            let suffix = if more > 0 {
                format!(" (and {more} more)")
            } else {
                String::new()
            };
            self.log_event(rag_db::NewLogEntry {
                ref_id,
                collection_id: collection.id,
                level: rag_db::LogLevel::Warn,
                phase: "ready".into(),
                message: format!(
                    "{} document(s) were only read in part: {}{suffix}",
                    partial.len(),
                    shown.join(", ")
                ),
                commit_sha: None,
                files: None,
                chunks: None,
                duration_ms: None,
            })
            .await;
        }

        Ok(BuildOutcome::Swapped {
            files: indexed_files,
            chunks,
            commit: head,
            live_uuid: build_uuid.to_string(),
            all_items_read: item_failures == 0,
        })
    }
}

/// Spawn the background loop. Runs forever until the gateway shuts down.
/// Each pass indexes every `pending` ref, running up to `clone_concurrency`
/// collections in parallel (refs *within* a collection stay serial). It then
/// sleeps until the next poll tick *or* an explicit kick (a "Re-index" click),
/// whichever comes first, so re-indexes start promptly. Failures are logged +
/// recorded against the ref; the loop never panics.
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
    // Group refs by collection. Refs of the *same* collection must run serially
    // (the per-collection invariant — and an aggregate collection has a single
    // unified index), but *different* collections index concurrently so one
    // slow clone can't block every other collection behind it. Preserve the
    // oldest-queued-first order across collections for fairness.
    let mut by_collection: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut order: Vec<i64> = Vec::new();
    for r in pending {
        let entry = by_collection.entry(r.collection_id).or_default();
        if entry.is_empty() {
            order.push(r.collection_id);
        }
        entry.push(r.id);
    }
    let mut set: JoinSet<()> = JoinSet::new();
    for cid in order {
        let ref_ids = by_collection.remove(&cid).unwrap_or_default();
        let idx = indexer.clone();
        let sem = Arc::clone(&indexer.inner.job_sem);
        set.spawn(async move {
            // Bounds how many collections index at once (embedding load).
            let _permit = sem.acquire_owned().await;
            for ref_id in ref_ids {
                if let Err(err) = idx.index_ref(ref_id).await {
                    tracing::warn!(ref_id, error = %err, "rag: indexing failed");
                }
            }
        });
    }
    // Finish the whole pass before the loop sleeps/kicks again.
    while set.join_next().await.is_some() {}
    Ok(())
}

/// Group per-file skip reasons into one operator-readable sentence.
///
/// Grouped rather than listed: a corpus of 3000 scans with OCR off produces
/// one reason, not 3000 log lines, and the reason is the thing to act on.
fn skip_summary(skipped: &BTreeMap<String, usize>) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    let mut parts: Vec<(usize, &String)> = skipped.iter().map(|(r, n)| (*n, r)).collect();
    // Commonest reason first — that is the one worth fixing.
    parts.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    let listed: Vec<String> = parts
        .iter()
        .take(3)
        .map(|(n, reason)| format!("{n} × {reason}"))
        .collect();
    format!("Reasons: {}.", listed.join("; "))
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
/// Extra widening of the candidate pool when a `path_glob` is in play.
///
/// The dense side can only filter *after* the kNN search, so a scope covering
/// a small slice of the corpus would otherwise come back thin — every
/// candidate discarded is a result the user asked for and didn't get. Widening
/// costs one larger kNN query and one `IN (…)` lookup, both cheap next to the
/// embedding call that already happened. It does not eliminate the recall
/// loss, which is why the tool documents path scoping as narrowing rather than
/// as a guarantee.
const PATH_FILTER_CANDIDATE_MULTIPLIER: usize = 5;

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
/// `path_glob` restricts the search to files whose indexed path matches
/// (SQLite GLOB syntax). Both sides honour it, but differently: the lexical
/// side filters in SQL, while the dense side has to generate kNN candidates
/// first (a vector index carries no metadata) and then ask the store which of
/// them are in scope. That costs recall on the dense side — the in-scope
/// matches have to be in the candidate pool at all — so the pool is widened
/// when a filter is present, and the filtering happens *before* fusion
/// truncates to `k` rather than after.
pub async fn search_chunks(
    indexer: &Indexer,
    rref: &rag_db::CollectionRef,
    query_text: &str,
    query_vec: &[f32],
    k: usize,
    path_glob: Option<&str>,
) -> Result<Vec<(rag_db::Chunk, f32)>, WorkerError> {
    if k == 0 {
        return Ok(Vec::new());
    }

    // Store + index live in this ref's own folder, cached by ref id.
    let store = indexer.collection_store(rref.id, &rref.data_uuid).await?;
    let mut pool = (k * CANDIDATE_MULTIPLIER).max(MIN_CANDIDATES);
    if path_glob.is_some() {
        pool *= PATH_FILTER_CANDIDATE_MULTIPLIER;
    }
    // A reranker can only promote what fusion handed it, so widen the net
    // when one is configured. The extra cost is one larger kNN query and a
    // few more rows — trivial next to the model call that follows.
    // Resolved once: the answer cannot change mid-search, and each call
    // allocates a model list and runs a pool health check.
    let rerank_model = rerank::model(
        &indexer.inner.upstreams,
        indexer.inner.config.rerank_model.as_deref(),
    );
    if rerank_model.is_some() {
        pool = pool.max(indexer.inner.config.rerank_candidates);
    }

    // Dense side. A missing on-disk index (ref never finished its first
    // build) is not an error here — fall back to lexical-only.
    let mut dense: Vec<i64> = match indexer.open_index(rref.id, &rref.data_uuid, None) {
        Ok(index) => index
            .search(query_vec, pool)?
            .into_iter()
            .map(|(vid, _)| vid)
            .collect(),
        Err(IndexError::Open { .. }) => Vec::new(),
        Err(other) => return Err(other.into()),
    };
    if let Some(glob) = path_glob
        && !dense.is_empty()
    {
        let in_scope =
            rag_db::vector_ids_matching_path(&store, rref.collection_id, &dense, glob).await?;
        // Retain, not rebuild: the kNN ranking is the input to fusion, so the
        // surviving candidates must keep their relative order.
        dense.retain(|vid| in_scope.contains(vid));
    }

    // Lexical side (BM25 over chunk text) — from this ref's store.
    let lexical =
        rag_db::lexical_search(&store, rref.collection_id, query_text, pool, path_glob).await?;

    // Fuse to the *candidate pool*, not to `k`: the reranker below can only
    // promote what it is handed, so truncating here would make the widened
    // pool pointless and leave the cross-encoder re-sorting the same `k`
    // items retrieval already chose. Without a reranker `pool` is the
    // ordinary candidate count and the final `truncate(k)` does the work.
    let fused = reciprocal_rank_fusion(&[&dense, &lexical], pool);
    if fused.is_empty() {
        return Ok(Vec::new());
    }

    let vids: Vec<i64> = fused.iter().map(|(vid, _)| *vid).collect();
    let chunks = rag_db::chunks_by_vector_ids(&store, rref.collection_id, &vids).await?;
    let mut by_vid: HashMap<i64, rag_db::Chunk> =
        chunks.into_iter().map(|c| (c.vector_id, c)).collect();
    // Re-join in fused order, carrying the RRF score; drop any vector id
    // whose chunk row didn't come back (index/db drift; rare).
    let mut hits: Vec<(rag_db::Chunk, f32)> = fused
        .into_iter()
        .filter_map(|(vid, score)| by_vid.remove(&vid).map(|c| (c, score as f32)))
        .collect();

    // Second opinion. Fusion scored the query and each passage separately;
    // a cross-encoder sees the pair. On a corpus of near-identical documents
    // that is often the only thing that can tell the right one from four
    // that look just like it.
    if let Some(model) = rerank_model {
        let documents: Vec<String> = hits.iter().map(|(c, _)| c.content.clone()).collect();
        match rerank::rerank(
            &indexer.inner.http,
            &indexer.inner.upstreams,
            &model,
            query_text,
            &documents,
        )
        .await
        {
            Ok(order) if !order.is_empty() => {
                let mut taken: Vec<Option<(rag_db::Chunk, f32)>> =
                    hits.into_iter().map(Some).collect();
                hits = order
                    .into_iter()
                    .filter_map(|(idx, score)| {
                        taken
                            .get_mut(idx)
                            .and_then(Option::take)
                            .map(|(c, _)| (c, score))
                    })
                    .collect();
            }
            Ok(_) => {}
            // Degraded ordering beats no answer: a reranker that is down
            // must not take search down with it.
            Err(err) => tracing::warn!(
                error = %err,
                "rag: reranking failed; returning the fused ranking"
            ),
        }
    }

    hits.truncate(k);
    Ok(hits)
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
        use gateway_core::server::upstreams::UpstreamRegistry;
        use std::collections::HashMap;

        let db = gateway_core::server::db::open(std::path::Path::new(":memory:"))
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
            None,
        );
        let a = indexer.open_index(1, "uuid-1", Some(4)).unwrap();
        let b = indexer.open_index(1, "uuid-1", Some(4)).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        // Discovery path: a fresh handle for collection 1 should accept
        // a `None` dim hint now that the file exists.
        let c = indexer.open_index(1, "uuid-1", None).unwrap();
        assert_eq!(c.dimensions(), 4);
    }

    // --- friendly_error mapping + end-to-end failure surfacing ---------------

    /// Build a `Collection` + primary `CollectionRef` in a fresh in-memory DB
    /// and an `Indexer` over a scratch data dir. Returns everything the
    /// failure-path tests need.
    async fn indexer_with_ref(
        git_url: &str,
        git_ref: &str,
        include_globs: Vec<String>,
    ) -> (
        Indexer,
        Pool,
        rag_db::Collection,
        rag_db::CollectionRef,
        tempfile::TempDir,
    ) {
        use gateway_core::server::upstreams::UpstreamRegistry;
        use std::collections::HashMap;

        let db = gateway_core::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let mut new = rag_db::NewCollection {
            name: "fix".into(),
            description: None,
            git_url: git_url.into(),
            git_ref: git_ref.into(),
            pat: None,
            source: Default::default(),
            profile_id: None,
            extraction_model: None,
            embedding_model: "embed-model".into(),
            include_globs,
            exclude_globs: vec![],
            chunk_size: 800,
            chunk_overlap: 100,
            search_mode: rag_db::SearchMode::Versioned,
        };
        new.search_mode = rag_db::SearchMode::Versioned;
        let collection = rag_db::create_collection(&db, &new).await.unwrap();
        let rref = rag_db::add_ref(&db, collection.id, git_ref, None, true)
            .await
            .unwrap();
        let upstreams = UpstreamRegistry::new(&HashMap::new()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let indexer = Indexer::new(
            db.clone(),
            upstreams,
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: dir.path().to_path_buf(),
                ..IndexerConfig::default()
            },
            None,
        );
        (indexer, db, collection, rref, dir)
    }

    /// A throwaway git repo with one commit on `main`. `None` if `git` isn't
    /// on PATH (CI without git → test skips rather than fails).
    fn fixture_repo() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .output()
        };
        let init = run(&["init", "-q", "-b", "main", "."]).ok()?;
        if !init.status.success() {
            return None;
        }
        for args in [
            ["config", "user.email", "t@example.invalid"],
            ["config", "user.name", "t"],
            ["config", "commit.gpgsign", "false"],
        ] {
            run(&args).ok()?;
        }
        std::fs::write(path.join("README.md"), b"hello world\n").unwrap();
        run(&["add", "."]).ok()?;
        let commit = run(&["commit", "-q", "-m", "init"]).ok()?;
        commit.status.success().then_some(dir)
    }

    #[test]
    fn friendly_error_maps_missing_branch_to_actionable_text() {
        // Hand-built ref/collection (no DB needed for the pure mapping fn).
        let collection = sample_collection("https://example.invalid/repo.git");
        let rref = sample_ref(&collection, "release-99");
        let err = WorkerError::Git(GitError::NonZero {
            command: "clone",
            status: 128,
            stderr: "fatal: Remote branch release-99 not found in upstream origin".into(),
        });
        let msg = friendly_error(&err, &rref, &collection);
        assert!(msg.contains("release-99"), "{msg}");
        assert!(msg.contains("does not exist"), "{msg}");
        assert!(msg.contains("example.invalid/repo.git"), "{msg}");
        assert_eq!(failure_phase(&err), "cloning");
    }

    #[test]
    fn friendly_error_maps_auth_failure() {
        let collection = sample_collection("https://example.invalid/private.git");
        let rref = sample_ref(&collection, "main");
        let err = WorkerError::Git(GitError::NonZero {
            command: "clone",
            status: 128,
            stderr: "fatal: Authentication failed for 'https://example.invalid/private.git/'"
                .into(),
        });
        let msg = friendly_error(&err, &rref, &collection);
        assert!(
            msg.to_lowercase().contains("authentication failed"),
            "{msg}"
        );
        assert!(msg.contains("access token") || msg.contains("PAT"), "{msg}");
    }

    fn sample_collection(git_url: &str) -> rag_db::Collection {
        let now = jiff::Timestamp::now();
        rag_db::Collection {
            id: 1,
            data_uuid: Some("u".into()),
            name: "c".into(),
            description: None,
            git_url: git_url.into(),
            git_ref: "main".into(),
            pat: None,
            source: Default::default(),
            profile_id: None,
            extraction_model: None,
            sync_hook_set: false,
            connected_account: None,
            connected_by: None,
            connected_at: None,
            embedding_model: "embed-model".into(),
            include_globs: vec!["**/*".into()],
            exclude_globs: vec![],
            chunk_size: 800,
            chunk_overlap: 100,
            search_mode: rag_db::SearchMode::Versioned,
            status: rag_db::CollectionStatus::Pending,
            allowed_groups: Vec::new(),
            last_indexed_at: None,
            last_indexed_commit: None,
            last_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_ref(collection: &rag_db::Collection, git_ref: &str) -> rag_db::CollectionRef {
        let now = jiff::Timestamp::now();
        rag_db::CollectionRef {
            id: 1,
            collection_id: collection.id,
            git_ref: git_ref.into(),
            git_url: None,
            is_primary: true,
            data_uuid: "u".into(),
            status: rag_db::CollectionStatus::Pending,
            last_indexed_at: None,
            last_indexed_commit: None,
            last_error: None,
            dir_versions: Default::default(),
            delta_cursor: None,
            force_full_rebuild: false,
            extractor_fingerprint: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn missing_branch_marks_ref_error_and_logs() {
        let Some(src) = fixture_repo() else {
            eprintln!("git not on PATH — skipping");
            return;
        };
        let url = src.path().to_string_lossy().to_string();
        let (indexer, db, _c, rref, _dir) =
            indexer_with_ref(&url, "no-such-branch", vec!["**/*".into()]).await;

        // The clone fails on the missing branch. index_ref returns Err, but
        // the failure must be RECORDED — that's the whole point of the fix.
        let res = indexer.index_ref(rref.id).await;
        assert!(res.is_err(), "indexing a missing branch should error");

        let after = rag_db::find_ref_by_id(&db, rref.id).await.unwrap().unwrap();
        assert_eq!(after.status, rag_db::CollectionStatus::Error);
        let err = after.last_error.expect("last_error must be set");
        assert!(err.contains("no-such-branch"), "{err}");
        assert!(err.contains("does not exist"), "{err}");

        // And it lands on the timeline as an error in the cloning phase.
        let log = rag_db::list_log_entries(&db, rref.id, 10).await.unwrap();
        assert!(
            log.iter()
                .any(|e| e.level == rag_db::LogLevel::Error && e.phase == "cloning"),
            "expected an error log entry, got {log:?}"
        );
    }

    #[tokio::test]
    async fn empty_glob_match_indexes_zero_and_warns() {
        let Some(src) = fixture_repo() else {
            eprintln!("git not on PATH — skipping");
            return;
        };
        let url = src.path().to_string_lossy().to_string();
        // Globs that match nothing in the fixture → 0 chunks, no embedding
        // calls, swap succeeds, advisory recorded.
        let (indexer, db, _c, rref, _dir) =
            indexer_with_ref(&url, "main", vec!["*.nomatch".into()]).await;

        indexer.index_ref(rref.id).await.unwrap();

        let after = rag_db::find_ref_by_id(&db, rref.id).await.unwrap().unwrap();
        assert_eq!(after.status, rag_db::CollectionStatus::Ready);
        assert!(
            after
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("Indexed 0 files"),
            "expected 0-files advisory, got {:?}",
            after.last_error
        );
        let log = rag_db::list_log_entries(&db, rref.id, 10).await.unwrap();
        assert!(
            log.iter().any(|e| e.level == rag_db::LogLevel::Warn),
            "expected a warn log entry, got {log:?}"
        );
    }

    /// One drain pass must process EVERY pending collection, not just the
    /// first. Regression for the head-of-line blocking fix: refs are grouped by
    /// collection and the groups run in parallel, but the pass still awaits all
    /// of them, so both collections here end up built. Empty include globs keep
    /// this embedding-free (0 chunks), so it runs without a mock upstream.
    #[tokio::test]
    async fn drain_once_processes_every_pending_collection() {
        use gateway_core::server::upstreams::UpstreamRegistry;
        use std::collections::HashMap;

        let Some(src) = fixture_repo() else {
            eprintln!("git not on PATH — skipping");
            return;
        };
        let url = src.path().to_string_lossy().to_string();
        let db = gateway_core::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let indexer = Indexer::new(
            db.clone(),
            UpstreamRegistry::new(&HashMap::new()).unwrap(),
            reqwest::Client::new(),
            IndexerConfig {
                data_dir: dir.path().to_path_buf(),
                clone_concurrency: 2,
                ..IndexerConfig::default()
            },
            None,
        );

        let mut ref_ids = Vec::new();
        for name in ["c1", "c2"] {
            let new = rag_db::NewCollection {
                name: name.into(),
                description: None,
                git_url: url.clone(),
                git_ref: "main".into(),
                pat: None,
                source: Default::default(),
                profile_id: None,
                extraction_model: None,
                embedding_model: "embed-model".into(),
                include_globs: vec!["*.nomatch".into()],
                exclude_globs: vec![],
                chunk_size: 800,
                chunk_overlap: 100,
                search_mode: rag_db::SearchMode::Versioned,
            };
            let c = rag_db::create_collection(&db, &new).await.unwrap();
            let r = rag_db::add_ref(&db, c.id, "main", None, true)
                .await
                .unwrap();
            ref_ids.push(r.id);
        }

        drain_once(&indexer).await.unwrap();

        for id in ref_ids {
            let after = rag_db::find_ref_by_id(&db, id).await.unwrap().unwrap();
            assert_eq!(
                after.status,
                rag_db::CollectionStatus::Ready,
                "ref {id} was not processed by the drain pass"
            );
        }
    }
}
