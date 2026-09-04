// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! RAG persistence — collections, indexed files, chunk metadata.
//!
//! Schema lives in `migrations/0013_rag.sql`. The vector data itself is
//! outside SQLite (per-collection `data/rag/<id>.usearch` files); this
//! module owns the metadata side and the indexer's status transitions.
//!
//! Conventions:
//!   * `i64` everywhere SQLite uses `INTEGER PRIMARY KEY`.
//!   * RFC 3339 strings for timestamps, parsed/rendered via `jiff::Timestamp`.
//!   * Glob arrays travel through the DB as JSON-encoded text — keeps the
//!     schema simple and lets callers thread `Vec<String>` end-to-end.

use std::collections::BTreeMap;

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{DbError, Pool};
use crate::server::crypto::{Crypto, Sealed};

/// Lifecycle of a collection from the indexer's point of view. The chat
/// surface only ever searches `Ready` collections; everything else is in
/// some intermediate state the admin UI surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionStatus {
    /// New row, or re-index requested. The indexer will pick it up.
    Pending,
    /// Indexer is currently cloning the repo.
    Cloning,
    /// Repo is cloned; chunks are being embedded.
    Indexing,
    /// Last indexing run succeeded; searchable.
    Ready,
    /// Last indexing run failed; see `last_error`. Won't retry until
    /// status is flipped back to `Pending`.
    Error,
}

impl CollectionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CollectionStatus::Pending => "pending",
            CollectionStatus::Cloning => "cloning",
            CollectionStatus::Indexing => "indexing",
            CollectionStatus::Ready => "ready",
            CollectionStatus::Error => "error",
        }
    }

    /// Lenient parse — any stray DB value lands in `Error` so a single
    /// bad row can't keep the admin page from rendering.
    fn from_db(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "cloning" => Self::Cloning,
            "indexing" => Self::Indexing,
            "ready" => Self::Ready,
            _ => Self::Error,
        }
    }
}

/// How `rag_search` resolves a query when the caller names no `ref`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// Refs are versions of the *same* repo (e.g. Ceph reef vs squid).
    /// Default search uses the collection's primary ref — versions are not
    /// mixed.
    Versioned,
    /// Sources are *different* repos forming one body of knowledge (e.g.
    /// all Proxmox repos). Default search fans out across every searchable
    /// source and merges the hits into one ranking.
    Aggregate,
}

impl SearchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchMode::Versioned => "versioned",
            SearchMode::Aggregate => "aggregate",
        }
    }

    /// Lenient parse — an unknown value falls back to `Versioned`, the
    /// pre-multi-source behaviour, so a stray row can't break rendering.
    fn from_db(s: &str) -> Self {
        match s {
            "aggregate" => Self::Aggregate,
            _ => Self::Versioned,
        }
    }
}

/// Which provider reaches a collection's files, and how it is configured.
///
/// The config is an opaque string map on purpose: its schema belongs to the
/// provider (`ProviderFactory::config_fields`), not to this table, so
/// registering a new provider never needs a migration. See
/// `migrations/0058_fileshare_rag.sql`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpec {
    /// `git` (the original behaviour) or a registered remote provider such
    /// as `webdav`.
    pub kind: String,
    pub config: BTreeMap<String, String>,
    /// Sealed with the at-rest key. This layer never sees the plaintext —
    /// callers open it with `Crypto`, as `backends.api_key_ct` works.
    pub secrets: Option<Sealed>,
}

impl Default for SourceSpec {
    fn default() -> Self {
        Self {
            kind: "git".to_string(),
            config: BTreeMap::new(),
            secrets: None,
        }
    }
}

impl SourceSpec {
    pub fn is_git(&self) -> bool {
        self.kind == "git"
    }
}

/// One configured codebase. Fields mirror the migration; see `0013_rag.sql`
/// for the why-each-column commentary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub id: i64,
    /// Names this collection's on-disk store folder
    /// (`<rag.data_dir>/<data_uuid>/`). `None` only for rows created
    /// before the per-collection-store migration; the indexer assigns one
    /// on the next index pass via [`assign_data_uuid`].
    pub data_uuid: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub git_url: String,
    pub git_ref: String,
    pub pat: Option<String>,
    /// Which provider reaches this collection's files, and how.
    pub source: SourceSpec,
    /// Extraction profile applied to each document, or `None` for no
    /// extraction. `None` is the right default: a code collection has no
    /// fields worth pulling out, and the pass costs one LLM call per file.
    pub profile_id: Option<i64>,
    /// Chat model for the extraction pass. `None` uses the first model the
    /// chat pool advertises.
    pub extraction_model: Option<String>,
    /// Whether this collection has a sync-trigger hook. The token itself is
    /// never read back — only whether one is set.
    pub sync_hook_set: bool,
    /// For an OAuth source: the account the corpus is read as, who granted
    /// that access, and when. Everyone who can search the collection sees
    /// what this account can see, so it is recorded rather than left to a
    /// "Test connection" toast. `None` for a source with typed credentials.
    pub connected_account: Option<String>,
    pub connected_by: Option<String>,
    pub connected_at: Option<String>,
    pub embedding_model: String,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub chunk_size: i64,
    pub chunk_overlap: i64,
    /// How `rag_search` resolves a ref-less query. See [`SearchMode`].
    pub search_mode: SearchMode,
    pub status: CollectionStatus,
    /// Gateway-group names allowed to list + search this collection. Empty =
    /// unrestricted (every user with the RAG tools). Managed on the `/rag` edit
    /// form. See `migrations/0046_rag_allowed_groups.sql` and
    /// `Resolver::resource_allowed`.
    pub allowed_groups: Vec<String>,
    pub last_indexed_at: Option<Timestamp>,
    pub last_indexed_commit: Option<String>,
    pub last_error: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Parameters for creating a fresh collection. Splits caller-supplied
/// state from indexer-owned state (status, last_*) so callers can't
/// accidentally fabricate a "ready" row.
#[derive(Debug, Clone)]
pub struct NewCollection {
    pub name: String,
    pub description: Option<String>,
    pub git_url: String,
    pub git_ref: String,
    pub pat: Option<String>,
    pub source: SourceSpec,
    pub profile_id: Option<i64>,
    pub extraction_model: Option<String>,
    pub embedding_model: String,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub chunk_size: i64,
    pub chunk_overlap: i64,
    pub search_mode: SearchMode,
}

fn parse_ts(s: &str, column: &'static str) -> Result<Timestamp, DbError> {
    s.parse().map_err(|e: jiff::Error| DbError::Decode {
        column,
        source: e.into(),
    })
}

fn decode_globs(s: &str, column: &'static str) -> Result<Vec<String>, DbError> {
    serde_json::from_str(s).map_err(|e| DbError::Decode {
        column,
        source: anyhow::Error::from(e),
    })
}

fn map_collection_row(row: &SqliteRow) -> Result<Collection, DbError> {
    let last_indexed_at: Option<String> = row.try_get("last_indexed_at")?;
    let last_indexed_at = last_indexed_at
        .map(|s| parse_ts(&s, "last_indexed_at"))
        .transpose()?;
    let include_globs_json: String = row.try_get("include_globs_json")?;
    let exclude_globs_json: String = row.try_get("exclude_globs_json")?;
    let created_at_s: String = row.try_get("created_at")?;
    let updated_at_s: String = row.try_get("updated_at")?;
    let status_s: String = row.try_get("status")?;
    let search_mode_s: String = row.try_get("search_mode")?;
    Ok(Collection {
        id: row.try_get("id")?,
        data_uuid: row.try_get("data_uuid")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        git_url: row.try_get("git_url")?,
        git_ref: row.try_get("git_ref")?,
        pat: row.try_get("pat")?,
        source: SourceSpec {
            kind: row.try_get("source_kind")?,
            config: {
                let json: String = row.try_get("source_config_json")?;
                serde_json::from_str(&json).map_err(|e| DbError::Decode {
                    column: "source_config_json",
                    source: anyhow::Error::from(e),
                })?
            },
            secrets: {
                let ct: Option<Vec<u8>> = row.try_get("source_secrets_ct")?;
                let nonce: Option<Vec<u8>> = row.try_get("source_secrets_nonce")?;
                // Half a sealed value is not a sealed value. A row with only
                // one column set is corruption, and reading it as "no
                // secrets" would hand the provider an empty password and
                // produce a 401 that looks like a wrong credential.
                match (ct, nonce) {
                    (Some(ciphertext), Some(nonce)) => Some(Sealed { nonce, ciphertext }),
                    (None, None) => None,
                    _ => {
                        return Err(DbError::Decode {
                            column: "source_secrets_ct",
                            source: anyhow::anyhow!(
                                "ciphertext and nonce must both be present or both absent"
                            ),
                        });
                    }
                }
            },
        },
        sync_hook_set: row
            .try_get::<Option<String>, _>("sync_token_hash")?
            .is_some(),
        connected_account: row.try_get("connected_account")?,
        connected_by: row.try_get("connected_by")?,
        connected_at: row.try_get("connected_at")?,
        profile_id: row.try_get("profile_id")?,
        extraction_model: row.try_get("extraction_model")?,
        embedding_model: row.try_get("embedding_model")?,
        include_globs: decode_globs(&include_globs_json, "include_globs_json")?,
        exclude_globs: decode_globs(&exclude_globs_json, "exclude_globs_json")?,
        chunk_size: row.try_get("chunk_size")?,
        chunk_overlap: row.try_get("chunk_overlap")?,
        search_mode: SearchMode::from_db(&search_mode_s),
        status: CollectionStatus::from_db(&status_s),
        allowed_groups: {
            let json: String = row.try_get("allowed_groups")?;
            serde_json::from_str(&json).map_err(|e| DbError::Decode {
                column: "allowed_groups",
                source: anyhow::Error::from(e),
            })?
        },
        last_indexed_at,
        last_indexed_commit: row.try_get("last_indexed_commit")?,
        last_error: row.try_get("last_error")?,
        created_at: parse_ts(&created_at_s, "created_at")?,
        updated_at: parse_ts(&updated_at_s, "updated_at")?,
    })
}

const COLLECTION_COLUMNS: &str = "id, data_uuid, name, description, git_url, git_ref, pat, \
     source_kind, source_config_json, source_secrets_ct, source_secrets_nonce, \
     profile_id, extraction_model, sync_token_hash, embedding_model, include_globs_json, exclude_globs_json, chunk_size, chunk_overlap, \
     search_mode, status, allowed_groups, last_indexed_at, last_indexed_commit, last_error, \
     connected_account, connected_by, connected_at, \
     created_at, updated_at";

pub async fn create_collection(pool: &Pool, new: &NewCollection) -> Result<Collection, DbError> {
    let now = Timestamp::now();
    let now_s = now.to_string();
    let include_json = serde_json::to_string(&new.include_globs).map_err(|e| DbError::Decode {
        column: "include_globs_json",
        source: e.into(),
    })?;
    let exclude_json = serde_json::to_string(&new.exclude_globs).map_err(|e| DbError::Decode {
        column: "exclude_globs_json",
        source: e.into(),
    })?;
    // Allocate the store-folder id up front so a freshly created
    // collection already knows where its per-collection data will live.
    let data_uuid = uuid::Uuid::new_v4().to_string();
    let source_config_json =
        serde_json::to_string(&new.source.config).map_err(|e| DbError::Decode {
            column: "source_config_json",
            source: e.into(),
        })?;
    let id: i64 = sqlx::query_scalar(
        r#"INSERT INTO rag_collections
           (data_uuid, name, description, git_url, git_ref, pat,
            source_kind, source_config_json, source_secrets_ct, source_secrets_nonce,
            profile_id, extraction_model, embedding_model,
            include_globs_json, exclude_globs_json, chunk_size, chunk_overlap,
            search_mode, status, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?)
           RETURNING id"#,
    )
    .bind(&data_uuid)
    .bind(&new.name)
    .bind(&new.description)
    .bind(&new.git_url)
    .bind(&new.git_ref)
    .bind(&new.pat)
    .bind(&new.source.kind)
    .bind(&source_config_json)
    .bind(new.source.secrets.as_ref().map(|s| s.ciphertext.clone()))
    .bind(new.source.secrets.as_ref().map(|s| s.nonce.clone()))
    .bind(new.profile_id)
    .bind(&new.extraction_model)
    .bind(&new.embedding_model)
    .bind(&include_json)
    .bind(&exclude_json)
    .bind(new.chunk_size)
    .bind(new.chunk_overlap)
    .bind(new.search_mode.as_str())
    .bind(&now_s)
    .bind(&now_s)
    .fetch_one(pool)
    .await?;
    find_collection_by_id(pool, id)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound.into())
}

pub async fn list_collections(pool: &Pool) -> Result<Vec<Collection>, DbError> {
    let q = format!("SELECT {COLLECTION_COLUMNS} FROM rag_collections ORDER BY created_at DESC");
    let rows = sqlx::query(&q).fetch_all(pool).await?;
    rows.iter().map(map_collection_row).collect()
}

pub async fn find_collection_by_id(pool: &Pool, id: i64) -> Result<Option<Collection>, DbError> {
    let q = format!("SELECT {COLLECTION_COLUMNS} FROM rag_collections WHERE id = ?");
    let row = sqlx::query(&q).bind(id).fetch_optional(pool).await?;
    row.as_ref().map(map_collection_row).transpose()
}

pub async fn find_collection_by_name(
    pool: &Pool,
    name: &str,
) -> Result<Option<Collection>, DbError> {
    let q = format!("SELECT {COLLECTION_COLUMNS} FROM rag_collections WHERE name = ?");
    let row = sqlx::query(&q).bind(name).fetch_optional(pool).await?;
    row.as_ref().map(map_collection_row).transpose()
}

/// Set a collection's status. Indexer-only; the admin API uses
/// [`request_reindex`] to bump back to `Pending` rather than calling this
/// directly so timestamps stay consistent.
pub async fn set_collection_status(
    pool: &Pool,
    id: i64,
    status: CollectionStatus,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query("UPDATE rag_collections SET status = ?, updated_at = ? WHERE id = ?")
        .bind(status.as_str())
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Indexer-side: a successful run lands here. Sets status to `Ready`,
/// stamps `last_indexed_at`, records the resolved commit, and clears
/// any prior `last_error`.
pub async fn mark_indexed(pool: &Pool, id: i64, commit_sha: &str) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"UPDATE rag_collections
           SET status = 'ready', last_indexed_at = ?, last_indexed_commit = ?,
               last_error = NULL, updated_at = ?
           WHERE id = ?"#,
    )
    .bind(&now)
    .bind(commit_sha)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Indexer-side: a failed run lands here. Status → `Error`, message
/// stored verbatim. Stays in `Error` until an admin reset.
pub async fn mark_failed(pool: &Pool, id: i64, message: &str) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"UPDATE rag_collections
           SET status = 'error', last_error = ?, updated_at = ?
           WHERE id = ?"#,
    )
    .bind(message)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Admin-side: re-queue a collection for indexing regardless of its
/// current status. Clears the prior error so the UI can show "queued"
/// without sticky failure text bleeding through.
pub async fn request_reindex(pool: &Pool, id: i64) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"UPDATE rag_collections
           SET status = 'pending', last_error = NULL, updated_at = ?
           WHERE id = ?"#,
    )
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Replace a collection's gateway-group access list (empty = unrestricted).
/// Set from the `/rag` edit form. Group names are stored verbatim as a JSON
/// array; the resolver treats a name no user holds as "no access", so a stale
/// name for a deleted group just stops granting access.
pub async fn set_allowed_groups(pool: &Pool, id: i64, groups: &[String]) -> Result<(), DbError> {
    let json = serde_json::to_string(groups).map_err(|e| DbError::Decode {
        column: "allowed_groups",
        source: e.into(),
    })?;
    let now = Timestamp::now().to_string();
    sqlx::query("UPDATE rag_collections SET allowed_groups = ?, updated_at = ? WHERE id = ?")
        .bind(&json)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_collection(pool: &Pool, id: i64) -> Result<bool, DbError> {
    let affected = sqlx::query("DELETE FROM rag_collections WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

impl SourceSpec {
    /// The stored secrets as plaintext; empty when there are none, or when
    /// the blob will not open.
    ///
    /// Treating an unopenable blob as empty is deliberate: a rotated at-rest
    /// key must not wedge the admin form or the consent flow, and re-entering
    /// the credential (or reconnecting) is exactly the recovery. Callers that
    /// need to tell "no secrets" from "cannot decrypt" — the indexer, which
    /// must fail loudly rather than index nothing — open the blob themselves.
    pub fn open_secrets(&self, crypto: &Crypto) -> BTreeMap<String, String> {
        self.secrets
            .as_ref()
            .and_then(|s| crypto.open_str(&s.nonce, &s.ciphertext).ok())
            .and_then(|plain| serde_json::from_str::<BTreeMap<String, String>>(&plain).ok())
            .unwrap_or_default()
    }
}

/// Record which account an OAuth source was connected as.
///
/// Written by the consent callback alongside the refresh token, so the two
/// cannot disagree about whose access the corpus is indexed with.
pub async fn set_connected_account(
    pool: &Pool,
    id: i64,
    account: Option<&str>,
    by: &str,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE rag_collections
         SET connected_account = ?, connected_by = ?, connected_at = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(account)
    .bind(by)
    .bind(Timestamp::now().to_string())
    .bind(Timestamp::now().to_string())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Replace a collection's sealed source secrets.
///
/// Used by the OAuth consent callback, which is the one writer that adds a
/// credential nobody typed. Takes the whole sealed blob rather than one key
/// because the blob is a single AEAD ciphertext: there is no way to update a
/// field inside it without opening and resealing the lot, and the caller has
/// already done exactly that.
pub async fn set_source_secrets(
    pool: &Pool,
    id: i64,
    sealed: Option<&Sealed>,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE rag_collections
         SET source_secrets_ct = ?, source_secrets_nonce = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(sealed.map(|s| s.ciphertext.clone()))
    .bind(sealed.map(|s| s.nonce.clone()))
    .bind(Timestamp::now().to_string())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Backfill the store-folder id for a pre-migration row that has none.
/// Called by the indexer the first time it touches such a collection.
pub async fn assign_data_uuid(pool: &Pool, id: i64, data_uuid: &str) -> Result<(), DbError> {
    sqlx::query("UPDATE rag_collections SET data_uuid = ? WHERE id = ?")
        .bind(data_uuid)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Re-queue a ref for a **full** rebuild, discarding the incremental state.
///
/// An incremental pass only re-indexes what the *source* changed. That is
/// exactly wrong when what changed is on our side — the extraction profile,
/// the chunk size, the include globs, the embedding model. The diff would
/// find every file unchanged and the corpus would keep answering with the
/// old shape, or worse (a new embedding model against vectors produced by
/// the old one) answer confidently from an incomparable vector space.
///
/// Does moving a collection from `before` to `after` invalidate what is
/// already indexed?
///
/// Derived by comparing the two rows rather than re-listed by each writer.
/// Every surface that can edit a collection — the web form, `PATCH
/// /api/v0/rag/collections/{id}`, anything added later — has to re-queue on
/// exactly the same conditions, and a hand-copied predicate drifts: the API
/// handler could already swap `embedding_model` without re-queueing, leaving
/// the corpus answering confidently out of an incomparable vector space.
///
/// Only the fields that change how documents are *reached*, *read*, or
/// *embedded* count. Credentials are deliberately excluded: rotating a
/// password reaches the same files, and forcing a full rebuild of a large
/// corpus on a routine rotation would be its own bug.
pub fn index_shape_changed(before: &Collection, after: &Collection) -> bool {
    before.source.kind != after.source.kind
        || before.source.config != after.source.config
        || before.git_url != after.git_url
        || before.git_ref != after.git_ref
        || before.profile_id != after.profile_id
        || before.extraction_model != after.extraction_model
        || before.embedding_model != after.embedding_model
        || before.chunk_size != after.chunk_size
        || before.chunk_overlap != after.chunk_overlap
        || before.include_globs != after.include_globs
        || before.exclude_globs != after.exclude_globs
}

/// Clearing `dir_versions` and setting `force_full_rebuild` sends the next
/// build down the fresh-folder path, which rebuilds everything and swaps
/// atomically — the live store keeps serving until it does.
///
/// `last_indexed_commit` is deliberately left alone: it is what
/// `is_searchable` reads, and clearing it would take the corpus offline for
/// the whole rebuild it is still perfectly able to answer from.
pub async fn request_full_rebuild(pool: &Pool, ref_id: i64) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"UPDATE rag_collection_refs
           SET status = 'pending', last_error = NULL,
               dir_versions_json = '{}',
               force_full_rebuild = 1, updated_at = ?
           WHERE id = ?"#,
    )
    .bind(&now)
    .bind(ref_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Hash a sync token for storage and lookup.
///
/// The token itself is never stored, for the same reason a webhook secret
/// is not: a leaked database must not hand out working trigger URLs.
pub fn hash_sync_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Give a collection a fresh sync token, returning the plaintext.
///
/// Shown to the operator once, at the moment they create it — after that
/// only the hash exists, and rotating is the only way to get a new one.
pub async fn rotate_sync_token(pool: &Pool, id: i64) -> Result<String, DbError> {
    use rand::Rng as _;
    let token: String = {
        const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let mut rng = rand::rng();
        (0..40)
            .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
            .collect()
    };
    sqlx::query("UPDATE rag_collections SET sync_token_hash = ?, updated_at = ? WHERE id = ?")
        .bind(hash_sync_token(&token))
        .bind(Timestamp::now().to_string())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(token)
}

/// Remove a collection's sync token, disabling its hook.
pub async fn clear_sync_token(pool: &Pool, id: i64) -> Result<(), DbError> {
    sqlx::query("UPDATE rag_collections SET sync_token_hash = NULL, updated_at = ? WHERE id = ?")
        .bind(Timestamp::now().to_string())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The collection a sync token belongs to, or `None`.
///
/// Looked up by hash so an attacker with database read access still cannot
/// mint a working URL.
pub async fn find_by_sync_token(pool: &Pool, token: &str) -> Result<Option<Collection>, DbError> {
    let q = format!("SELECT {COLLECTION_COLUMNS} FROM rag_collections WHERE sync_token_hash = ?");
    let row = sqlx::query(&q)
        .bind(hash_sync_token(token))
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(map_collection_row).transpose()
}

// ---- collection refs (branches / tags / commits) -------------------------
//
// A collection's source is configured once; each branch / tag / commit it
// indexes is a `rag_collection_refs` row, built and searched independently
// (its own `data_uuid` store folder, its own status + last-indexed commit).
// Exactly one ref per collection is `is_primary` — the one `rag_search`
// uses when the caller doesn't name a ref.

/// One indexed ref of a collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionRef {
    pub id: i64,
    pub collection_id: i64,
    pub git_ref: String,
    /// This source's own repo URL, or `None` to use the collection's
    /// `git_url`. Set for aggregate collections where each source is a
    /// different repo; `None` for versioned collections (refs of one repo).
    pub git_url: Option<String>,
    pub is_primary: bool,
    /// Names this ref's on-disk store folder (`<rag.data_dir>/<data_uuid>/`).
    pub data_uuid: String,
    pub status: CollectionStatus,
    pub last_indexed_at: Option<Timestamp>,
    pub last_indexed_commit: Option<String>,
    pub last_error: Option<String>,
    /// Directory versions from the last completed walk, for subtree pruning
    /// on the next one. A cache: losing it costs one full walk, never
    /// correctness. Always empty for git sources and for providers that do
    /// not report `subtree_pruning`.
    pub dir_versions: BTreeMap<String, String>,
    /// Provider-native change-feed cursor, for providers with a delta API.
    /// Set by `request_full_rebuild`; cleared by a successful swap.
    pub force_full_rebuild: bool,
    /// Which extractors were available when this ref was last built. `None`
    /// for a ref built before the column existed. See
    /// `migrations/0058_fileshare_rag.sql`.
    pub extractor_fingerprint: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl CollectionRef {
    /// A ref is searchable once it has completed at least one full index
    /// (its `data_uuid` then points at a complete store). Stays searchable
    /// on that store even while a later re-index rebuilds in a fresh folder.
    pub fn is_searchable(&self) -> bool {
        self.last_indexed_commit.is_some()
    }

    /// Whether the next build must go down the fresh-folder path rather than
    /// updating the live store in place.
    ///
    /// Separate from [`Self::is_searchable`] on purpose: asking for a rebuild
    /// must not make the corpus unsearchable while it runs. The rebuild is
    /// atomic — the live store answers until the swap.
    pub fn needs_full_rebuild(&self) -> bool {
        self.force_full_rebuild || !self.is_searchable()
    }

    /// The URL the indexer should clone for this source: its own `git_url`
    /// override if set, else the collection's `git_url`.
    pub fn effective_git_url<'a>(&'a self, collection: &'a Collection) -> &'a str {
        self.git_url.as_deref().unwrap_or(&collection.git_url)
    }

    /// Short human label for the source, derived from its effective URL —
    /// the last path segment without a `.git` suffix (e.g. `qemu-server`).
    /// Used to tell aggregate hits apart by originating repo.
    pub fn source_label(&self, collection: &Collection) -> String {
        repo_basename(self.effective_git_url(collection))
    }
}

/// Last path segment of a git URL, minus any `.git` suffix. Falls back to
/// the trimmed input when there's no separator.
pub fn repo_basename(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(".git");
    if last.is_empty() {
        trimmed.to_string()
    } else {
        last.to_string()
    }
}

const REF_COLUMNS: &str = "id, collection_id, git_ref, git_url, is_primary, data_uuid, status, \
     last_indexed_at, last_indexed_commit, last_error, dir_versions_json, \
     force_full_rebuild, extractor_fingerprint, \
     created_at, updated_at";

fn map_ref_row(row: &SqliteRow) -> Result<CollectionRef, DbError> {
    let last_indexed_at: Option<String> = row.try_get("last_indexed_at")?;
    let last_indexed_at = last_indexed_at
        .map(|s| parse_ts(&s, "last_indexed_at"))
        .transpose()?;
    let created_at_s: String = row.try_get("created_at")?;
    let updated_at_s: String = row.try_get("updated_at")?;
    let status_s: String = row.try_get("status")?;
    let is_primary: i64 = row.try_get("is_primary")?;
    Ok(CollectionRef {
        id: row.try_get("id")?,
        collection_id: row.try_get("collection_id")?,
        git_ref: row.try_get("git_ref")?,
        git_url: row.try_get("git_url")?,
        is_primary: is_primary != 0,
        data_uuid: row.try_get("data_uuid")?,
        status: CollectionStatus::from_db(&status_s),
        last_indexed_at,
        last_indexed_commit: row.try_get("last_indexed_commit")?,
        last_error: row.try_get("last_error")?,
        dir_versions: {
            let json: String = row.try_get("dir_versions_json")?;
            // A cache, so a decode failure degrades to "walk everything"
            // rather than failing the build.
            serde_json::from_str(&json).unwrap_or_default()
        },
        force_full_rebuild: row.try_get::<i64, _>("force_full_rebuild")? != 0,
        extractor_fingerprint: row.try_get("extractor_fingerprint")?,
        created_at: parse_ts(&created_at_s, "created_at")?,
        updated_at: parse_ts(&updated_at_s, "updated_at")?,
    })
}

/// All refs of a collection, primary first then oldest-first.
pub async fn list_refs(pool: &Pool, collection_id: i64) -> Result<Vec<CollectionRef>, DbError> {
    let q = format!(
        "SELECT {REF_COLUMNS} FROM rag_collection_refs WHERE collection_id = ? \
         ORDER BY is_primary DESC, created_at ASC, id ASC"
    );
    let rows = sqlx::query(&q).bind(collection_id).fetch_all(pool).await?;
    rows.iter().map(map_ref_row).collect()
}

/// Every searchable source of a collection — those that have completed at
/// least one index (`last_indexed_commit IS NOT NULL`). Used by aggregate
/// search to fan out across all repos of the collection.
pub async fn searchable_refs(
    pool: &Pool,
    collection_id: i64,
) -> Result<Vec<CollectionRef>, DbError> {
    let q = format!(
        "SELECT {REF_COLUMNS} FROM rag_collection_refs \
         WHERE collection_id = ? AND last_indexed_commit IS NOT NULL \
         ORDER BY created_at ASC, id ASC"
    );
    let rows = sqlx::query(&q).bind(collection_id).fetch_all(pool).await?;
    rows.iter().map(map_ref_row).collect()
}

pub async fn find_ref(
    pool: &Pool,
    collection_id: i64,
    git_ref: &str,
) -> Result<Option<CollectionRef>, DbError> {
    let q = format!(
        "SELECT {REF_COLUMNS} FROM rag_collection_refs WHERE collection_id = ? AND git_ref = ?"
    );
    let row = sqlx::query(&q)
        .bind(collection_id)
        .bind(git_ref)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(map_ref_row).transpose()
}

pub async fn find_ref_by_id(pool: &Pool, ref_id: i64) -> Result<Option<CollectionRef>, DbError> {
    let q = format!("SELECT {REF_COLUMNS} FROM rag_collection_refs WHERE id = ?");
    let row = sqlx::query(&q).bind(ref_id).fetch_optional(pool).await?;
    row.as_ref().map(map_ref_row).transpose()
}

/// The collection's primary ref (the search default), if any.
pub async fn primary_ref(
    pool: &Pool,
    collection_id: i64,
) -> Result<Option<CollectionRef>, DbError> {
    let q = format!(
        "SELECT {REF_COLUMNS} FROM rag_collection_refs \
         WHERE collection_id = ? AND is_primary = 1"
    );
    let row = sqlx::query(&q)
        .bind(collection_id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(map_ref_row).transpose()
}

/// Add a ref/source to a collection. Allocates its store-folder id.
/// `git_url` is the source's own repo (aggregate collections) or `None` to
/// inherit the collection's `git_url` (versioned collections). If
/// `is_primary`, demotes the current primary first (the partial unique
/// index allows only one). New refs start `pending` for the indexer.
pub async fn add_ref(
    pool: &Pool,
    collection_id: i64,
    git_ref: &str,
    git_url: Option<&str>,
    is_primary: bool,
) -> Result<CollectionRef, DbError> {
    let now = Timestamp::now().to_string();
    let data_uuid = uuid::Uuid::new_v4().to_string();
    let mut tx = pool.begin().await?;
    if is_primary {
        sqlx::query("UPDATE rag_collection_refs SET is_primary = 0 WHERE collection_id = ?")
            .bind(collection_id)
            .execute(&mut *tx)
            .await?;
    }
    let id: i64 = sqlx::query_scalar(
        r#"INSERT INTO rag_collection_refs
           (collection_id, git_ref, git_url, is_primary, data_uuid, status, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, 'pending', ?, ?)
           RETURNING id"#,
    )
    .bind(collection_id)
    .bind(git_ref)
    .bind(git_url)
    .bind(is_primary as i64)
    .bind(&data_uuid)
    .bind(&now)
    .bind(&now)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    find_ref_by_id(pool, id)
        .await?
        .ok_or_else(|| sqlx::Error::RowNotFound.into())
}

/// Make `ref_id` the collection's primary, demoting whatever was primary.
pub async fn set_primary(pool: &Pool, ref_id: i64) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    let mut tx = pool.begin().await?;
    let collection_id: Option<i64> =
        sqlx::query_scalar("SELECT collection_id FROM rag_collection_refs WHERE id = ?")
            .bind(ref_id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(cid) = collection_id {
        sqlx::query("UPDATE rag_collection_refs SET is_primary = 0 WHERE collection_id = ?")
            .bind(cid)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE rag_collection_refs SET is_primary = 1, updated_at = ? WHERE id = ?")
            .bind(&now)
            .bind(ref_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Update a source ref's clone target: its optional per-source `git_url`
/// (`None` → inherit the collection's URL, as versioned refs do) and its
/// `git_ref` (branch/tag/commit). Does *not* re-queue — the caller re-queues
/// afterwards so the new URL/ref actually gets fetched. Returns the ref's
/// collection id (so the caller can patch the right row), or `None` if the ref
/// no longer exists.
pub async fn update_ref(
    pool: &Pool,
    ref_id: i64,
    git_url: Option<&str>,
    git_ref: &str,
) -> Result<Option<i64>, DbError> {
    let now = Timestamp::now().to_string();
    let collection_id: Option<i64> = sqlx::query_scalar(
        "UPDATE rag_collection_refs SET git_url = ?, git_ref = ?, updated_at = ? \
         WHERE id = ? RETURNING collection_id",
    )
    .bind(git_url)
    .bind(git_ref)
    .bind(&now)
    .bind(ref_id)
    .fetch_optional(pool)
    .await?;
    Ok(collection_id)
}

/// Delete a ref. Returns its `data_uuid` so the caller can reap the
/// on-disk store folder. If the deleted ref was the collection's primary
/// and other refs remain, one of them is promoted so the collection never
/// ends up primary-less (which would break `rag_search`'s default-to-primary
/// resolution). A searchable ref (one that has been indexed) is preferred;
/// otherwise the oldest remaining ref is picked.
pub async fn delete_ref(pool: &Pool, ref_id: i64) -> Result<Option<String>, DbError> {
    let now = Timestamp::now().to_string();
    let mut tx = pool.begin().await?;
    let row: Option<(Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT data_uuid, collection_id, is_primary FROM rag_collection_refs WHERE id = ?",
    )
    .bind(ref_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((data_uuid, collection_id, was_primary)) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    sqlx::query("DELETE FROM rag_collection_refs WHERE id = ?")
        .bind(ref_id)
        .execute(&mut *tx)
        .await?;
    if was_primary == 1 {
        // Promote a survivor: prefer one that is already searchable, then
        // fall back to the oldest. NULLs sort last under `IS NOT NULL DESC`.
        let next: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM rag_collection_refs WHERE collection_id = ? \
             ORDER BY (last_indexed_commit IS NOT NULL) DESC, id ASC LIMIT 1",
        )
        .bind(collection_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(next_id) = next {
            sqlx::query(
                "UPDATE rag_collection_refs SET is_primary = 1, updated_at = ? WHERE id = ?",
            )
            .bind(&now)
            .bind(next_id)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(data_uuid)
}

/// Indexer-only: set a ref's lifecycle status.
pub async fn set_ref_status(
    pool: &Pool,
    ref_id: i64,
    status: CollectionStatus,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query("UPDATE rag_collection_refs SET status = ?, updated_at = ? WHERE id = ?")
        .bind(status.as_str())
        .bind(&now)
        .bind(ref_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Indexer-only: a successful (re-)index lands here. Atomically swaps the
/// ref onto the freshly-built `new_data_uuid` store, stamps the commit, and
/// flips to `ready` — but only `WHERE status='indexing'`, so a concurrent
/// re-index that flipped the ref back to `pending` makes this a no-op (the
/// stale build is then discarded by the caller). Returns rows affected
/// (0 = superseded, the swap did not happen).
pub async fn swap_ref_index(
    pool: &Pool,
    ref_id: i64,
    new_data_uuid: &str,
    commit_sha: &str,
    extractor_fingerprint: &str,
) -> Result<u64, DbError> {
    let now = Timestamp::now().to_string();
    let affected = sqlx::query(
        r#"UPDATE rag_collection_refs
           SET data_uuid = ?, last_indexed_commit = ?, last_indexed_at = ?,
               status = 'ready', last_error = NULL, force_full_rebuild = 0,
               extractor_fingerprint = ?, updated_at = ?
           WHERE id = ? AND status = 'indexing'"#,
    )
    .bind(new_data_uuid)
    .bind(commit_sha)
    .bind(&now)
    .bind(extractor_fingerprint)
    .bind(&now)
    .bind(ref_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}

/// Indexer-only: record what the last walk learned about the remote tree,
/// so the next one can skip unchanged subtrees.
///
/// Called only after a walk that saw the whole tree
/// (`TreeSnapshot::is_complete`). Storing versions from a partial walk would
/// let the next sync prune a subtree whose contents we never actually read.
pub async fn set_ref_sync_state(
    pool: &Pool,
    ref_id: i64,
    dir_versions: &BTreeMap<String, String>,
) -> Result<(), DbError> {
    let json = serde_json::to_string(dir_versions).map_err(|e| DbError::Decode {
        column: "dir_versions_json",
        source: e.into(),
    })?;
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"UPDATE rag_collection_refs
           SET dir_versions_json = ?, updated_at = ?
           WHERE id = ?"#,
    )
    .bind(&json)
    .bind(&now)
    .bind(ref_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Indexer-only: a failed run. Status → `error` (message stored), but only
/// while the ref is in a mid-build state (`cloning` or `indexing`) so it
/// can't clobber a re-index that re-queued the ref (→ `pending`). The ref
/// keeps its prior `data_uuid` + commit, so a previously completed index
/// stays searchable. Returns rows affected.
///
/// Both build phases are covered deliberately: a bad git ref (e.g. a branch
/// that doesn't exist) fails during `cloning`, *before* the `indexing`
/// transition. Guarding on `indexing` alone silently dropped those failures
/// — the ref stayed stuck on `cloning` with an empty `last_error` and the
/// admin had no way to see why nothing happened.
pub async fn mark_ref_failed(pool: &Pool, ref_id: i64, message: &str) -> Result<u64, DbError> {
    let now = Timestamp::now().to_string();
    let affected = sqlx::query(
        r#"UPDATE rag_collection_refs
           SET status = 'error', last_error = ?, updated_at = ?
           WHERE id = ? AND status IN ('cloning', 'indexing')"#,
    )
    .bind(message)
    .bind(&now)
    .bind(ref_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}

/// Record a non-fatal advisory on a ref without changing its status —
/// e.g. "indexed 0 files". Surfaced in the admin UI like an error so a
/// likely misconfiguration (include globs that match nothing) is visible
/// instead of a silently-empty "ready" source.
pub async fn set_ref_warning(pool: &Pool, ref_id: i64, message: &str) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query("UPDATE rag_collection_refs SET last_error = ?, updated_at = ? WHERE id = ?")
        .bind(message)
        .bind(&now)
        .bind(ref_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Admin-side: (re-)queue a ref for indexing. Flips to `pending` and clears
/// the prior error. Keeps `data_uuid` + commit so the existing index stays
/// searchable until the rebuild swaps in. A running build sees the status
/// is no longer `indexing` at its next checkpoint and aborts.
pub async fn request_ref_reindex(pool: &Pool, ref_id: i64) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"UPDATE rag_collection_refs
           SET status = 'pending', last_error = NULL, updated_at = ?
           WHERE id = ?"#,
    )
    .bind(&now)
    .bind(ref_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Refs the indexer should pick up, oldest-queued first.
pub async fn list_pending_refs(pool: &Pool) -> Result<Vec<CollectionRef>, DbError> {
    let q = format!(
        "SELECT {REF_COLUMNS} FROM rag_collection_refs WHERE status = 'pending' \
         ORDER BY updated_at ASC, id ASC"
    );
    let rows = sqlx::query(&q).fetch_all(pool).await?;
    rows.iter().map(map_ref_row).collect()
}

/// Startup recovery: any ref left mid-build (`cloning`/`indexing`) by a
/// crash or restart is orphaned — no worker resumes it. Flip them back to
/// `pending` so they re-run. Returns how many were reset.
pub async fn reset_stalled_refs(pool: &Pool) -> Result<u64, DbError> {
    let now = Timestamp::now().to_string();
    let affected = sqlx::query(
        "UPDATE rag_collection_refs SET status = 'pending', updated_at = ? \
         WHERE status IN ('cloning', 'indexing')",
    )
    .bind(&now)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}

/// Every `data_uuid` currently referenced by a ref — the set of live store
/// folders. Used to reap orphaned build folders left by interrupted runs.
pub async fn all_ref_data_uuids(pool: &Pool) -> Result<Vec<String>, DbError> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT data_uuid FROM rag_collection_refs")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

// ---- per-ref indexing log -------------------------------------------------
//
// One row per notable indexing event for a ref, so the admin UI can show a
// timeline ("cloning…", "indexed 312 files", "branch not found") instead of
// the single overwritten `last_error`. Lives in the central DB next to the
// refs themselves. Append-only from the worker; pruned per ref to cap growth.

/// Severity of a log entry — drives the colour the admin UI renders it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }

    /// Lenient parse — an unknown value lands in `Info` so one odd row can't
    /// break the log render.
    fn from_db(s: &str) -> Self {
        match s {
            "warn" => Self::Warn,
            "error" => Self::Error,
            _ => Self::Info,
        }
    }
}

/// One recorded indexing event for a ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexLogEntry {
    pub id: i64,
    pub ref_id: i64,
    pub collection_id: i64,
    pub created_at: Timestamp,
    pub level: LogLevel,
    /// Build phase the event belongs to: `queued` | `cloning` | `indexing`
    /// | `ready` | `error`. A free string (not an enum) so a future phase
    /// doesn't need a schema change.
    pub phase: String,
    pub message: String,
    pub commit_sha: Option<String>,
    pub files: Option<i64>,
    pub chunks: Option<i64>,
    pub duration_ms: Option<i64>,
}

/// A log event awaiting insertion. The worker fills one of these at each
/// notable point in a build and calls [`insert_log_entry`].
#[derive(Debug, Clone)]
pub struct NewLogEntry {
    pub ref_id: i64,
    pub collection_id: i64,
    pub level: LogLevel,
    pub phase: String,
    pub message: String,
    pub commit_sha: Option<String>,
    pub files: Option<i64>,
    pub chunks: Option<i64>,
    pub duration_ms: Option<i64>,
}

fn map_log_row(row: &SqliteRow) -> Result<IndexLogEntry, DbError> {
    let created_at_s: String = row.try_get("created_at")?;
    let level_s: String = row.try_get("level")?;
    Ok(IndexLogEntry {
        id: row.try_get("id")?,
        ref_id: row.try_get("ref_id")?,
        collection_id: row.try_get("collection_id")?,
        created_at: parse_ts(&created_at_s, "created_at")?,
        level: LogLevel::from_db(&level_s),
        phase: row.try_get("phase")?,
        message: row.try_get("message")?,
        commit_sha: row.try_get("commit_sha")?,
        files: row.try_get("files")?,
        chunks: row.try_get("chunks")?,
        duration_ms: row.try_get("duration_ms")?,
    })
}

const LOG_COLUMNS: &str = "id, ref_id, collection_id, created_at, level, phase, message, \
     commit_sha, files, chunks, duration_ms";

/// Append one event to a ref's indexing log. Returns the new row id.
pub async fn insert_log_entry(pool: &Pool, entry: &NewLogEntry) -> Result<i64, DbError> {
    let now = Timestamp::now().to_string();
    let id: i64 = sqlx::query_scalar(
        r#"INSERT INTO rag_index_log
           (ref_id, collection_id, created_at, level, phase, message,
            commit_sha, files, chunks, duration_ms)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
           RETURNING id"#,
    )
    .bind(entry.ref_id)
    .bind(entry.collection_id)
    .bind(&now)
    .bind(entry.level.as_str())
    .bind(&entry.phase)
    .bind(&entry.message)
    .bind(&entry.commit_sha)
    .bind(entry.files)
    .bind(entry.chunks)
    .bind(entry.duration_ms)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// A ref's most recent log entries, newest first, capped at `limit`.
pub async fn list_log_entries(
    pool: &Pool,
    ref_id: i64,
    limit: i64,
) -> Result<Vec<IndexLogEntry>, DbError> {
    let q = format!(
        "SELECT {LOG_COLUMNS} FROM rag_index_log WHERE ref_id = ? ORDER BY id DESC LIMIT ?"
    );
    let rows = sqlx::query(&q)
        .bind(ref_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    rows.iter().map(map_log_row).collect()
}

/// The most recent per-source file count each ref reported, keyed by ref id.
///
/// The aggregate build records what every source contributed (see the walk
/// loop in `rag::worker`), which is the only place that number exists — the
/// refs table carries a *status*, and a source can be `ready` while having
/// put nothing in the index. This is what lets the UI and
/// `rag_list_collections` say "32 sources, 3 of them empty" instead of "32
/// sources ready".
pub async fn latest_source_files(
    pool: &Pool,
    collection_id: i64,
) -> Result<std::collections::HashMap<i64, i64>, DbError> {
    // The newest row per ref that actually carried a file count. `MAX(id)`
    // picks the latest build; older rows for the same ref are ignored.
    let rows = sqlx::query(
        "SELECT ref_id, files FROM rag_index_log \
         WHERE id IN ( \
             SELECT MAX(id) FROM rag_index_log \
             WHERE collection_id = ? AND files IS NOT NULL \
             GROUP BY ref_id \
         )",
    )
    .bind(collection_id)
    .fetch_all(pool)
    .await?;
    let mut out = std::collections::HashMap::with_capacity(rows.len());
    for r in &rows {
        out.insert(
            r.try_get::<i64, _>("ref_id")?,
            r.try_get::<i64, _>("files")?,
        );
    }
    Ok(out)
}

/// Trim a ref's log to its newest `keep` rows. Called after each insert so
/// a long-lived, frequently-reindexed ref doesn't grow its log unbounded.
/// Returns how many rows were pruned.
pub async fn prune_log_entries(pool: &Pool, ref_id: i64, keep: i64) -> Result<u64, DbError> {
    let affected = sqlx::query(
        r#"DELETE FROM rag_index_log
           WHERE ref_id = ?
             AND id NOT IN (
                 SELECT id FROM rag_index_log WHERE ref_id = ? ORDER BY id DESC LIMIT ?
             )"#,
    )
    .bind(ref_id)
    .bind(ref_id)
    .bind(keep)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}

// ---- file-side metadata ---------------------------------------------------

/// One indexed source file. `content_hash` is the diff key: the indexer
/// compares it on re-pull and skips re-chunking files whose hash matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    pub id: i64,
    pub collection_id: i64,
    pub path: String,
    pub content_hash: String,
    pub indexed_at: Timestamp,
    /// Deep link into the source's own UI, so an answer can be checked
    /// against the original. `None` for sources with no per-file URL.
    pub web_url: Option<String>,
    /// Provider-native stable id, where the source has one.
    pub remote_id: Option<String>,
    /// The source's change token when this file was indexed. Compared for
    /// equality on the next sync; never parsed.
    pub source_version: Option<String>,
}

fn map_file_row(row: &SqliteRow) -> Result<IndexedFile, DbError> {
    let indexed_at_s: String = row.try_get("indexed_at")?;
    Ok(IndexedFile {
        web_url: row.try_get("web_url")?,
        remote_id: row.try_get("remote_id")?,
        source_version: row.try_get("source_version")?,
        id: row.try_get("id")?,
        collection_id: row.try_get("collection_id")?,
        path: row.try_get("path")?,
        content_hash: row.try_get("content_hash")?,
        indexed_at: parse_ts(&indexed_at_s, "indexed_at")?,
    })
}

pub async fn list_files_for_collection(
    pool: &Pool,
    collection_id: i64,
) -> Result<Vec<IndexedFile>, DbError> {
    let rows = sqlx::query(
        r#"SELECT id, collection_id, path, content_hash, indexed_at, web_url,
                  remote_id, source_version
           FROM rag_files WHERE collection_id = ?"#,
    )
    .bind(collection_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(map_file_row).collect()
}

/// Upsert a file row. Returns the file's id. Used by the indexer after
/// it has decided this file needs (re-)embedding — paired with a
/// [`delete_chunks_for_file`] so the old chunks/vectors are reaped first.
/// Where a file came from, for the row `upsert_file` writes.
///
/// Grouped rather than passed as four `Option<&str>` in a row, which is an
/// invitation to swap two of them silently.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileOrigin<'a> {
    pub web_url: Option<&'a str>,
    pub remote_id: Option<&'a str>,
    /// The source's change token. Stored so the next sync can tell this file
    /// apart from a changed one without re-fetching it.
    pub source_version: Option<&'a str>,
}

pub async fn upsert_file(
    pool: &Pool,
    collection_id: i64,
    path: &str,
    content_hash: &str,
    origin: &FileOrigin<'_>,
) -> Result<i64, DbError> {
    let now = Timestamp::now().to_string();
    let id: i64 = sqlx::query_scalar(
        r#"INSERT INTO rag_files
             (collection_id, path, content_hash, indexed_at, web_url, remote_id, source_version)
           VALUES (?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT (collection_id, path) DO UPDATE
               SET content_hash   = excluded.content_hash,
                   indexed_at     = excluded.indexed_at,
                   web_url        = excluded.web_url,
                   remote_id      = excluded.remote_id,
                   source_version = excluded.source_version
           RETURNING id"#,
    )
    .bind(collection_id)
    .bind(path)
    .bind(content_hash)
    .bind(&now)
    .bind(origin.web_url)
    .bind(origin.remote_id)
    .bind(origin.source_version)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Apply a batch of path moves atomically.
///
/// `rag_files` is `UNIQUE (collection_id, path)`, so moves cannot simply be
/// applied one at a time: two files that swap paths collide whichever order
/// you pick, and a file moving onto a path another file is about to vacate
/// collides too. A single failed `UPDATE` propagates and takes the whole
/// incremental pass with it.
///
/// So every row is first parked on a path nothing can collide with, then
/// placed. The park path is keyed by `file_id` (unique by construction) and
/// starts with a control character, which no real path from any provider
/// contains — WebDAV paths come from hrefs and the Drive provider strips
/// control characters in `sanitize_segment`. Both phases run in one
/// transaction, so a crash between them cannot leave parked rows behind.
pub async fn rename_files(pool: &Pool, moves: &[(i64, String)]) -> Result<(), DbError> {
    if moves.is_empty() {
        return Ok(());
    }
    let now = Timestamp::now().to_string();
    let mut tx = pool.begin().await?;
    for (file_id, _) in moves {
        sqlx::query("UPDATE rag_files SET path = ? WHERE id = ?")
            .bind(format!("\u{1}parking/{file_id}"))
            .bind(file_id)
            .execute(&mut *tx)
            .await?;
    }
    for (file_id, new_path) in moves {
        sqlx::query("UPDATE rag_files SET path = ?, indexed_at = ? WHERE id = ?")
            .bind(new_path)
            .bind(&now)
            .bind(file_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The vector ids belonging to one file, so they can be removed from the
/// usearch index when the file is replaced or deleted. Without this, a
/// deleted document's vectors keep answering searches.
pub async fn vector_ids_for_file(pool: &Pool, file_id: i64) -> Result<Vec<i64>, DbError> {
    let rows: Vec<i64> = sqlx::query_scalar("SELECT vector_id FROM rag_chunks WHERE file_id = ?")
        .bind(file_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Delete a file and everything derived from it. Chunks and extracted
/// document rows go with it via `ON DELETE CASCADE`.
pub async fn delete_file(pool: &Pool, file_id: i64) -> Result<(), DbError> {
    sqlx::query("DELETE FROM rag_files WHERE id = ?")
        .bind(file_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---- chunk-side metadata --------------------------------------------------

/// One chunk: provenance for a single vector. `vector_id` is the integer
/// key into the per-collection usearch index file. `file_path` is
/// materialised at fetch time (JOIN against `rag_files`) so the search
/// tool can render `path:start-end` without a second round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub id: i64,
    pub collection_id: i64,
    pub file_id: i64,
    pub file_path: String,
    /// The source's own link to this file, when it has one — what turns a
    /// citation into something the reader can open and verify.
    pub web_url: Option<String>,
    pub chunk_index: i64,
    pub loc: ChunkLoc,
    pub content: String,
    pub vector_id: i64,
}

/// The unit a chunk's position is measured in.
///
/// Source files are cited by line. A PDF, a scan or an office document is
/// cited by page: line numbers do not survive extraction and a page is what
/// a person can actually open the original to and check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocKind {
    Line,
    Page,
}

impl LocKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LocKind::Line => "line",
            LocKind::Page => "page",
        }
    }

    /// Unknown values read as `Line`, which is what every pre-existing row
    /// is. Stores are regenerable, so this is a decoding convenience rather
    /// than a compatibility promise.
    pub fn from_db(s: &str) -> Self {
        match s {
            "page" => LocKind::Page,
            _ => LocKind::Line,
        }
    }
}

/// Where a chunk sits in its document. Inclusive on both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkLoc {
    pub kind: LocKind,
    pub from: i64,
    pub to: i64,
}

impl ChunkLoc {
    pub fn lines(from: i64, to: i64) -> Self {
        Self {
            kind: LocKind::Line,
            from,
            to,
        }
    }

    pub fn pages(from: i64, to: i64) -> Self {
        Self {
            kind: LocKind::Page,
            from,
            to,
        }
    }

    /// Human- and model-readable citation, e.g. `lines 12-30` or `page 4`.
    /// One string rather than raw numbers, because this is what a model
    /// quotes back to the user and a bare pair of integers invites it to
    /// invent the unit.
    pub fn label(&self) -> String {
        let unit = self.kind.as_str();
        if self.from == self.to {
            format!("{unit} {}", self.from)
        } else {
            format!("{unit}s {}-{}", self.from, self.to)
        }
    }
}

/// A chunk awaiting insertion. The indexer fills these out then calls
/// [`insert_chunks`] in batches alongside the vector upsert.
#[derive(Debug, Clone)]
pub struct NewChunk {
    pub file_id: i64,
    pub chunk_index: i64,
    pub loc: ChunkLoc,
    pub content: String,
    pub vector_id: i64,
}

fn map_chunk_row(row: &SqliteRow) -> Result<Chunk, DbError> {
    Ok(Chunk {
        id: row.try_get("id")?,
        collection_id: row.try_get("collection_id")?,
        file_id: row.try_get("file_id")?,
        file_path: row.try_get("file_path")?,
        web_url: row.try_get("web_url").ok().flatten(),
        chunk_index: row.try_get("chunk_index")?,
        loc: {
            let kind: String = row.try_get("loc_kind")?;
            ChunkLoc {
                kind: LocKind::from_db(&kind),
                from: row.try_get("loc_from")?,
                to: row.try_get("loc_to")?,
            }
        },
        content: row.try_get("content")?,
        vector_id: row.try_get("vector_id")?,
    })
}

/// Insert a batch of chunks in one transaction. Empty input is a no-op.
pub async fn insert_chunks(
    pool: &Pool,
    collection_id: i64,
    chunks: &[NewChunk],
) -> Result<(), DbError> {
    if chunks.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for c in chunks {
        sqlx::query(
            r#"INSERT INTO rag_chunks
               (collection_id, file_id, chunk_index, loc_kind, loc_from, loc_to,
                content, vector_id)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(collection_id)
        .bind(c.file_id)
        .bind(c.chunk_index)
        .bind(c.loc.kind.as_str())
        .bind(c.loc.from)
        .bind(c.loc.to)
        .bind(&c.content)
        .bind(c.vector_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn delete_chunks_for_file(pool: &Pool, file_id: i64) -> Result<(), DbError> {
    sqlx::query("DELETE FROM rag_chunks WHERE file_id = ?")
        .bind(file_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Largest `vector_id` seen in `collection_id`, or `None` for an empty
/// collection. The indexer uses this to pick the next id to allocate so
/// vector ids stay monotonic across re-indexes — usearch tolerates
/// reused ids in principle, but monotonic-only keeps debugging sane.
pub async fn max_vector_id(pool: &Pool, collection_id: i64) -> Result<Option<i64>, DbError> {
    let max: Option<i64> =
        sqlx::query_scalar("SELECT MAX(vector_id) FROM rag_chunks WHERE collection_id = ?")
            .bind(collection_id)
            .fetch_one(pool)
            .await?;
    Ok(max)
}

/// Lexical (BM25) search over chunk text for one collection via the
/// `rag_chunks_fts` index. Returns matching `vector_id`s best-first
/// (smaller bm25 = better). The raw query is sanitised into a bag of
/// OR-ed alphanumeric tokens (see [`fts_match_query`]) so arbitrary user
/// prose can never trip FTS5's MATCH operator grammar, and so recall
/// stays wide — BM25 ranking sorts out precision. An empty/too-short
/// query yields no hits.
/// `path_glob` (SQLite GLOB syntax, matched against the file's indexed path)
/// narrows the search to part of the corpus. It is applied **inside** the
/// query rather than to the results: filtering afterwards would let the
/// `LIMIT` fill up with matches from elsewhere and return nothing for a
/// perfectly good path-scoped search.
pub async fn lexical_search(
    pool: &Pool,
    collection_id: i64,
    query: &str,
    limit: usize,
    path_glob: Option<&str>,
) -> Result<Vec<i64>, DbError> {
    let match_query = fts_match_query(query);
    if match_query.is_empty() {
        return Ok(Vec::new());
    }
    // The path lives on `rag_files`, so the join (and the extra bind) only
    // appear when a filter was asked for — the unfiltered query stays exactly
    // as cheap as it was.
    let sql = if path_glob.is_some() {
        r#"SELECT c.vector_id
           FROM rag_chunks_fts
           JOIN rag_chunks c ON c.id = rag_chunks_fts.rowid
           JOIN rag_files f ON f.id = c.file_id
           WHERE rag_chunks_fts MATCH ?1 AND c.collection_id = ?2
                 AND f.path GLOB ?4
           ORDER BY bm25(rag_chunks_fts) ASC
           LIMIT ?3"#
    } else {
        r#"SELECT c.vector_id
           FROM rag_chunks_fts
           JOIN rag_chunks c ON c.id = rag_chunks_fts.rowid
           WHERE rag_chunks_fts MATCH ?1 AND c.collection_id = ?2
           ORDER BY bm25(rag_chunks_fts) ASC
           LIMIT ?3"#
    };
    let mut query = sqlx::query_scalar(sql)
        .bind(&match_query)
        .bind(collection_id)
        .bind(limit as i64);
    if let Some(glob) = path_glob {
        query = query.bind(glob.to_string());
    }
    let rows: Vec<i64> = query.fetch_all(pool).await?;
    Ok(rows)
}

/// Which of `vector_ids` belong to files whose path matches `path_glob`.
///
/// The companion to the filter above, for the dense side: the vector index
/// holds no metadata, so a path-scoped search has to take the kNN candidates
/// and ask the store which of them are in scope. Returns a set (order comes
/// from the caller's candidate ranking, which this must not disturb).
pub async fn vector_ids_matching_path(
    pool: &Pool,
    collection_id: i64,
    vector_ids: &[i64],
    path_glob: &str,
) -> Result<std::collections::HashSet<i64>, DbError> {
    if vector_ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let placeholders = vec!["?"; vector_ids.len()].join(",");
    let sql = format!(
        "SELECT c.vector_id \
         FROM rag_chunks c JOIN rag_files f ON f.id = c.file_id \
         WHERE c.collection_id = ? AND f.path GLOB ? \
               AND c.vector_id IN ({placeholders})"
    );
    let mut query = sqlx::query_scalar(&sql).bind(collection_id).bind(path_glob);
    for vid in vector_ids {
        query = query.bind(vid);
    }
    let rows: Vec<i64> = query.fetch_all(pool).await?;
    Ok(rows.into_iter().collect())
}

/// Stream chunk rows for a full scan (the `rag_grep` regex path), oldest file
/// first so results come back in a stable, path-grouped order.
///
/// `after_id` is a keyset cursor: pass the last `Chunk::id` from the previous
/// batch. Keyset rather than OFFSET because the scan is bounded by a *time*
/// budget as well as a row budget — the caller stops when either runs out, and
/// re-scanning skipped rows on every page would make the row budget lie.
pub async fn scan_chunks(
    pool: &Pool,
    collection_id: i64,
    path_glob: Option<&str>,
    after_id: i64,
    limit: usize,
) -> Result<Vec<Chunk>, DbError> {
    let sql = format!(
        "SELECT c.id, c.collection_id, c.file_id, f.path AS file_path, \
                c.chunk_index, c.loc_kind, c.loc_from, c.loc_to, c.content, c.vector_id, \
                f.web_url AS web_url \
         FROM rag_chunks c JOIN rag_files f ON f.id = c.file_id \
         WHERE c.collection_id = ? AND c.id > ?{} \
         ORDER BY c.id ASC LIMIT ?",
        if path_glob.is_some() {
            " AND f.path GLOB ?"
        } else {
            ""
        }
    );
    let mut query = sqlx::query(&sql).bind(collection_id).bind(after_id);
    if let Some(glob) = path_glob {
        query = query.bind(glob);
    }
    let rows = query.bind(limit as i64).fetch_all(pool).await?;
    rows.iter().map(map_chunk_row).collect()
}

/// Turn arbitrary user text into a safe FTS5 MATCH expression: lowercase
/// alphanumeric tokens, de-duplicated, length >= 2, joined with ` OR `.
/// Splitting on every non-alphanumeric char means `osd_op_timeout`
/// becomes `osd OR op OR timeout`, which matches both the underscored
/// identifier and a spaced-out "osd op timeout" query. Tokens are
/// alphanumeric by construction, so there's nothing for FTS5 to
/// misinterpret as a column filter, NEAR clause, or quote.
fn fts_match_query(query: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut tokens: Vec<String> = Vec::new();
    for raw in query.split(|c: char| !c.is_alphanumeric()) {
        if raw.len() < 2 {
            continue;
        }
        let tok = raw.to_lowercase();
        if seen.insert(tok.clone()) {
            tokens.push(tok);
        }
    }
    tokens.join(" OR ")
}

/// Resolve a batch of `(collection_id, vector_id)` hits from a usearch
/// search back into chunk rows so the tool can surface provenance.
/// Preserves caller order; missing rows are dropped silently (they would
/// be index/db drift — rare and not worth failing the call).
pub async fn chunks_by_vector_ids(
    pool: &Pool,
    collection_id: i64,
    vector_ids: &[i64],
) -> Result<Vec<Chunk>, DbError> {
    if vector_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; vector_ids.len()].join(",");
    let q = format!(
        "SELECT c.id, c.collection_id, c.file_id, f.path AS file_path, \
                c.chunk_index, c.loc_kind, c.loc_from, c.loc_to, c.content, c.vector_id, \
                f.web_url AS web_url \
         FROM rag_chunks c \
         JOIN rag_files f ON f.id = c.file_id \
         WHERE c.collection_id = ? AND c.vector_id IN ({placeholders})"
    );
    let mut query = sqlx::query(&q).bind(collection_id);
    for vid in vector_ids {
        query = query.bind(vid);
    }
    let rows = query.fetch_all(pool).await?;
    let mut by_vid: std::collections::HashMap<i64, Chunk> = std::collections::HashMap::new();
    for row in &rows {
        let c = map_chunk_row(row)?;
        by_vid.insert(c.vector_id, c);
    }
    Ok(vector_ids
        .iter()
        .filter_map(|vid| by_vid.remove(vid))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::open;
    use std::path::Path;

    async fn fresh() -> Pool {
        open(Path::new(":memory:")).await.unwrap()
    }

    fn sample_new() -> NewCollection {
        NewCollection {
            name: "gateway".into(),
            description: Some("the gateway repo".into()),
            git_url: "https://example.invalid/repo.git".into(),
            git_ref: "main".into(),
            pat: None,
            source: Default::default(),
            profile_id: None,
            extraction_model: None,
            embedding_model: "embed-model".into(),
            include_globs: vec!["*.rs".into()],
            exclude_globs: vec!["target/".into()],
            chunk_size: 800,
            chunk_overlap: 100,
            search_mode: SearchMode::Versioned,
        }
    }

    #[tokio::test]
    async fn create_and_round_trip_collection() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        assert_eq!(c.name, "gateway");
        assert_eq!(c.status, CollectionStatus::Pending);
        assert_eq!(c.include_globs, vec!["*.rs"]);
        assert_eq!(c.exclude_globs, vec!["target/"]);

        let by_id = find_collection_by_id(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(by_id, c);
        let by_name = find_collection_by_name(&pool, "gateway")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_name, c);
    }

    #[tokio::test]
    async fn unique_name_constraint_rejects_duplicates() {
        let pool = fresh().await;
        create_collection(&pool, &sample_new()).await.unwrap();
        let err = create_collection(&pool, &sample_new()).await.unwrap_err();
        assert!(
            matches!(err, DbError::Query(_)),
            "expected Query(UNIQUE failure), got {err:?}"
        );
    }

    #[tokio::test]
    async fn lifecycle_transitions_clear_and_set_last_error() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();

        set_collection_status(&pool, c.id, CollectionStatus::Indexing)
            .await
            .unwrap();
        mark_failed(&pool, c.id, "git auth failed").await.unwrap();
        let after_fail = find_collection_by_id(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(after_fail.status, CollectionStatus::Error);
        assert_eq!(after_fail.last_error.as_deref(), Some("git auth failed"));

        request_reindex(&pool, c.id).await.unwrap();
        let after_requeue = find_collection_by_id(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(after_requeue.status, CollectionStatus::Pending);
        assert!(after_requeue.last_error.is_none());

        mark_indexed(&pool, c.id, "abc123").await.unwrap();
        let after_ok = find_collection_by_id(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(after_ok.status, CollectionStatus::Ready);
        assert_eq!(after_ok.last_indexed_commit.as_deref(), Some("abc123"));
        assert!(after_ok.last_indexed_at.is_some());
        assert!(after_ok.last_error.is_none());
    }

    /// A standalone per-collection content store (files/chunks/FTS live
    /// here now, not in the central DB). Returns the pool plus the
    /// tempdir, which the caller must keep alive for the pool's lifetime.
    async fn store() -> (Pool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::server::db::open_collection_store(&dir.path().join("rag.sqlite"))
            .await
            .unwrap();
        (pool, dir)
    }

    #[tokio::test]
    async fn upsert_file_returns_stable_id_and_updates_hash() {
        let (store, _dir) = store().await;
        let id1 = upsert_file(&store, 1, "src/main.rs", "hash-a", &Default::default())
            .await
            .unwrap();
        let id2 = upsert_file(&store, 1, "src/main.rs", "hash-b", &Default::default())
            .await
            .unwrap();
        assert_eq!(id1, id2, "upsert must keep the same row id");
        let files = list_files_for_collection(&store, 1).await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content_hash, "hash-b");
    }

    #[tokio::test]
    async fn chunks_round_trip_with_vector_id_join() {
        let (store, _dir) = store().await;
        let f = upsert_file(&store, 1, "src/lib.rs", "h", &Default::default())
            .await
            .unwrap();
        insert_chunks(
            &store,
            1,
            &[
                NewChunk {
                    file_id: f,
                    chunk_index: 0,
                    loc: ChunkLoc::lines(1, 10),
                    content: "first".into(),
                    vector_id: 10,
                },
                NewChunk {
                    file_id: f,
                    chunk_index: 1,
                    loc: ChunkLoc::lines(11, 20),
                    content: "second".into(),
                    vector_id: 11,
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(max_vector_id(&store, 1).await.unwrap(), Some(11));

        // Caller-order preserved; missing ids dropped.
        let resolved = chunks_by_vector_ids(&store, 1, &[11, 999, 10])
            .await
            .unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].content, "second");
        assert_eq!(resolved[1].content, "first");
    }

    #[tokio::test]
    async fn lexical_search_ranks_exact_token_hit() {
        // FTS5 lexical side: an identifier query should match the chunk
        // whose underscored symbol tokenizes to the same terms.
        let (store, _dir) = store().await;
        let f = upsert_file(&store, 1, "global.yaml.in", "h", &Default::default())
            .await
            .unwrap();
        insert_chunks(
            &store,
            1,
            &[
                NewChunk {
                    file_id: f,
                    chunk_index: 0,
                    loc: ChunkLoc::lines(1, 1),
                    content: "name: osd_op_timeout desc: timeout for osd ops".into(),
                    vector_id: 1,
                },
                NewChunk {
                    file_id: f,
                    chunk_index: 1,
                    loc: ChunkLoc::lines(2, 2),
                    content: "crush choose_total_tries placement retries".into(),
                    vector_id: 2,
                },
            ],
        )
        .await
        .unwrap();
        let hits = lexical_search(&store, 1, "osd op timeout", 10, None)
            .await
            .unwrap();
        assert_eq!(
            hits.first(),
            Some(&1),
            "exact-token chunk should rank first"
        );
    }

    #[tokio::test]
    async fn delete_collection_removes_registry_row() {
        // Content lives in a separate per-collection store now, so there's
        // no FK cascade to assert here — deleting the registry row just
        // unregisters the collection (the indexer reaps the folder
        // separately via `drop_collection_storage`).
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        assert!(find_collection_by_id(&pool, c.id).await.unwrap().is_some());
        assert!(
            c.data_uuid.is_some(),
            "create must allocate a store folder id"
        );
        assert!(delete_collection(&pool, c.id).await.unwrap());
        assert!(find_collection_by_id(&pool, c.id).await.unwrap().is_none());
        assert!(
            !delete_collection(&pool, c.id).await.unwrap(),
            "deleting a missing row is a clean false"
        );
    }

    #[tokio::test]
    async fn refs_add_list_primary_and_delete() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let reef = add_ref(&pool, c.id, "reef", None, true).await.unwrap();
        assert!(reef.is_primary);
        assert!(!reef.is_searchable(), "a fresh ref has no completed index");
        let squid = add_ref(&pool, c.id, "squid", None, false).await.unwrap();
        assert!(!squid.is_primary);
        assert_eq!(
            primary_ref(&pool, c.id).await.unwrap().unwrap().git_ref,
            "reef"
        );
        let refs = list_refs(&pool, c.id).await.unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].git_ref, "reef", "primary lists first");

        // Re-point primary; exactly one primary survives.
        set_primary(&pool, squid.id).await.unwrap();
        assert_eq!(
            primary_ref(&pool, c.id).await.unwrap().unwrap().git_ref,
            "squid"
        );
        let primaries = list_refs(&pool, c.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.is_primary)
            .count();
        assert_eq!(primaries, 1);

        assert_eq!(
            find_ref(&pool, c.id, "reef").await.unwrap().unwrap().id,
            reef.id
        );
        let uuid = delete_ref(&pool, reef.id).await.unwrap();
        assert_eq!(uuid.as_deref(), Some(reef.data_uuid.as_str()));
        assert_eq!(list_refs(&pool, c.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deleting_primary_promotes_a_survivor() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let primary = add_ref(&pool, c.id, "primary", None, true).await.unwrap();
        let fresh_ref = add_ref(&pool, c.id, "fresh", None, false).await.unwrap();
        let indexed = add_ref(&pool, c.id, "indexed", None, false).await.unwrap();
        // Make `indexed` searchable so it is the preferred promotion target
        // over the never-indexed `fresh` ref.
        set_ref_status(&pool, indexed.id, CollectionStatus::Indexing)
            .await
            .unwrap();
        swap_ref_index(
            &pool,
            indexed.id,
            "u-indexed",
            "sha",
            "ocr=false,office=false",
        )
        .await
        .unwrap();

        // Deleting the primary must hand primacy to the searchable survivor,
        // never leave the collection without a primary.
        delete_ref(&pool, primary.id).await.unwrap();
        let now_primary = primary_ref(&pool, c.id).await.unwrap();
        assert_eq!(
            now_primary.as_ref().map(|r| r.git_ref.as_str()),
            Some("indexed"),
            "searchable survivor is promoted"
        );
        assert!(fresh_ref.id != now_primary.unwrap().id);

        // Deleting down to the last ref leaves no primary, but that is the
        // empty case (no survivor to promote), not a primary-less collection
        // with refs still present.
        delete_ref(&pool, indexed.id).await.unwrap();
        delete_ref(&pool, fresh_ref.id).await.unwrap();
        assert!(list_refs(&pool, c.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ref_swap_and_reindex_are_status_guarded() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let r = add_ref(&pool, c.id, "reef", None, true).await.unwrap();

        // Swap is a no-op unless the ref is mid-index.
        assert_eq!(
            swap_ref_index(&pool, r.id, "u1", "sha1", "ocr=false,office=false")
                .await
                .unwrap(),
            0
        );

        set_ref_status(&pool, r.id, CollectionStatus::Indexing)
            .await
            .unwrap();
        assert_eq!(
            swap_ref_index(&pool, r.id, "u1", "sha1", "ocr=false,office=false")
                .await
                .unwrap(),
            1
        );
        let after = find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
        assert_eq!(after.status, CollectionStatus::Ready);
        assert_eq!(after.data_uuid, "u1");
        assert!(after.is_searchable());

        // A re-queue must not be clobbered by a stale build finishing.
        request_ref_reindex(&pool, r.id).await.unwrap();
        assert_eq!(
            swap_ref_index(&pool, r.id, "u2", "sha2", "ocr=false,office=false")
                .await
                .unwrap(),
            0,
            "swap must not overwrite a re-queued ref"
        );
        let after = find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
        assert_eq!(after.data_uuid, "u1", "old index stays live");
        assert!(after.is_searchable());
    }

    #[test]
    fn repo_basename_strips_path_and_git_suffix() {
        assert_eq!(
            repo_basename("https://github.com/proxmox/qemu-server.git"),
            "qemu-server"
        );
        assert_eq!(
            repo_basename("https://github.com/proxmox/pve-manager"),
            "pve-manager"
        );
        assert_eq!(
            repo_basename("git@github.com:proxmox/pve-docs.git"),
            "pve-docs"
        );
        assert_eq!(repo_basename("file:///tmp/repo/"), "repo");
    }

    #[tokio::test]
    async fn aggregate_sources_share_a_ref_but_differ_by_url() {
        let pool = fresh().await;
        let mut new = sample_new();
        new.search_mode = SearchMode::Aggregate;
        new.git_url = "https://fallback.invalid/default.git".into();
        let c = create_collection(&pool, &new).await.unwrap();
        assert_eq!(c.search_mode, SearchMode::Aggregate);

        // Two different repos on the SAME branch must coexist — the old
        // UNIQUE(collection_id, git_ref) is replaced by a url-aware index.
        let a = add_ref(
            &pool,
            c.id,
            "master",
            Some("https://x/pve-manager.git"),
            false,
        )
        .await
        .unwrap();
        let b = add_ref(
            &pool,
            c.id,
            "master",
            Some("https://x/qemu-server.git"),
            false,
        )
        .await
        .unwrap();
        assert_eq!(a.source_label(&c), "pve-manager");
        assert_eq!(b.source_label(&c), "qemu-server");
        assert_eq!(a.effective_git_url(&c), "https://x/pve-manager.git");

        // The same (url, ref) twice is still rejected (idempotent bulk-add).
        assert!(
            add_ref(
                &pool,
                c.id,
                "master",
                Some("https://x/pve-manager.git"),
                false
            )
            .await
            .is_err(),
            "duplicate source must be rejected"
        );

        // A source with no url inherits the collection's url.
        let inherit = add_ref(&pool, c.id, "dev", None, false).await.unwrap();
        assert_eq!(inherit.effective_git_url(&c), c.git_url.as_str());

        // Nothing is searchable until an index completes.
        assert!(searchable_refs(&pool, c.id).await.unwrap().is_empty());
        set_ref_status(&pool, a.id, CollectionStatus::Indexing)
            .await
            .unwrap();
        swap_ref_index(&pool, a.id, "uuid-a", "sha-a", "ocr=false,office=false")
            .await
            .unwrap();
        let searchable = searchable_refs(&pool, c.id).await.unwrap();
        assert_eq!(searchable.len(), 1);
        assert_eq!(searchable[0].id, a.id);
    }

    #[tokio::test]
    async fn update_ref_changes_url_and_ref() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let r = add_ref(&pool, c.id, "master", Some("https://x/old-repo.git"), true)
            .await
            .unwrap();

        // Point the source at a different repo + branch.
        let cid = update_ref(&pool, r.id, Some("https://x/new-repo.git"), "main")
            .await
            .unwrap();
        assert_eq!(cid, Some(c.id));
        let updated = find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
        assert_eq!(updated.effective_git_url(&c), "https://x/new-repo.git");
        assert_eq!(updated.git_ref, "main");

        // Clearing the URL (None) falls back to inheriting the collection URL.
        update_ref(&pool, r.id, None, "main").await.unwrap();
        let inherited = find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
        assert_eq!(inherited.effective_git_url(&c), c.git_url.as_str());

        // A missing ref updates nothing.
        assert_eq!(update_ref(&pool, 9999, None, "x").await.unwrap(), None);
    }

    #[tokio::test]
    async fn set_ref_warning_records_advisory_without_changing_status() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let r = add_ref(&pool, c.id, "main", None, true).await.unwrap();
        set_ref_status(&pool, r.id, CollectionStatus::Indexing)
            .await
            .unwrap();
        swap_ref_index(&pool, r.id, &r.data_uuid, "sha", "ocr=false,office=false")
            .await
            .unwrap();
        set_ref_warning(&pool, r.id, "Indexed 0 files — check globs")
            .await
            .unwrap();
        let after = find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
        // Still ready/searchable, but the advisory is visible.
        assert_eq!(after.status, CollectionStatus::Ready);
        assert!(after.is_searchable());
        assert_eq!(
            after.last_error.as_deref(),
            Some("Indexed 0 files — check globs")
        );
    }

    #[tokio::test]
    async fn mark_ref_failed_records_during_cloning_phase() {
        // Regression: a branch that doesn't exist fails while the ref is in
        // `cloning` (before the `indexing` transition). The failure must be
        // recorded — previously the `WHERE status='indexing'` guard dropped
        // it, leaving the ref stuck on `cloning` with an empty `last_error`
        // and the admin blind to the cause.
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let r = add_ref(&pool, c.id, "no-such-branch", None, true)
            .await
            .unwrap();

        set_ref_status(&pool, r.id, CollectionStatus::Cloning)
            .await
            .unwrap();
        let affected = mark_ref_failed(&pool, r.id, "branch not found")
            .await
            .unwrap();
        assert_eq!(affected, 1, "clone-phase failure must be recorded");

        let after = find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
        assert_eq!(after.status, CollectionStatus::Error);
        assert_eq!(after.last_error.as_deref(), Some("branch not found"));
    }

    #[tokio::test]
    async fn mark_ref_failed_does_not_clobber_a_requeue() {
        // A re-queue flips the ref to `pending`; a late-arriving failure from
        // the superseded build must NOT overwrite that (the new build owns
        // the ref now).
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let r = add_ref(&pool, c.id, "main", None, true).await.unwrap();

        request_ref_reindex(&pool, r.id).await.unwrap(); // → pending
        let affected = mark_ref_failed(&pool, r.id, "stale failure").await.unwrap();
        assert_eq!(affected, 0, "must not clobber a pending re-queue");
        let after = find_ref_by_id(&pool, r.id).await.unwrap().unwrap();
        assert_eq!(after.status, CollectionStatus::Pending);
        assert!(after.last_error.is_none());
    }

    #[tokio::test]
    async fn reset_stalled_refs_requeues_orphans() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let r = add_ref(&pool, c.id, "reef", None, true).await.unwrap();
        set_ref_status(&pool, r.id, CollectionStatus::Cloning)
            .await
            .unwrap();
        assert_eq!(reset_stalled_refs(&pool).await.unwrap(), 1);
        assert_eq!(
            find_ref_by_id(&pool, r.id).await.unwrap().unwrap().status,
            CollectionStatus::Pending
        );
    }

    /// `latest_source_files` is what lets a UI (and the model) tell "this
    /// source is ready" from "this source put nothing in the index" — the
    /// distinction a collection reporting `32 sources ready` cannot make on
    /// its own, and the one that let a whole repo go missing unnoticed.
    #[tokio::test]
    async fn latest_source_files_reports_the_newest_count_per_ref() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let a = add_ref(&pool, c.id, "a", None, true).await.unwrap();
        let b = add_ref(&pool, c.id, "b", None, false).await.unwrap();

        let entry = |ref_id, files: Option<i64>| NewLogEntry {
            ref_id,
            collection_id: c.id,
            level: LogLevel::Info,
            phase: "indexing".into(),
            message: "contributed".into(),
            commit_sha: None,
            files,
            chunks: None,
            duration_ms: None,
        };
        // An older build for `a`, then a newer one — the newer must win.
        insert_log_entry(&pool, &entry(a.id, Some(500)))
            .await
            .unwrap();
        insert_log_entry(&pool, &entry(a.id, Some(312)))
            .await
            .unwrap();
        // `b` contributed nothing at its last build.
        insert_log_entry(&pool, &entry(b.id, Some(0)))
            .await
            .unwrap();
        // A row with no file count must not overwrite a real one.
        insert_log_entry(&pool, &entry(a.id, None)).await.unwrap();

        let map = latest_source_files(&pool, c.id).await.unwrap();
        assert_eq!(map.get(&a.id), Some(&312), "newest count wins: {map:?}");
        assert_eq!(
            map.get(&b.id),
            Some(&0),
            "zero is a reportable value, not an absence: {map:?}"
        );
    }

    /// A ref that has never reported a count is absent from the map — callers
    /// render nothing rather than claiming zero, which would libel a source
    /// indexed before this tracking existed.
    #[tokio::test]
    async fn a_ref_that_never_reported_is_absent_rather_than_zero() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let r = add_ref(&pool, c.id, "main", None, true).await.unwrap();
        let map = latest_source_files(&pool, c.id).await.unwrap();
        assert!(!map.contains_key(&r.id), "{map:?}");
    }

    #[tokio::test]
    async fn index_log_inserts_lists_newest_first_and_prunes() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let r = add_ref(&pool, c.id, "main", None, true).await.unwrap();

        // A small spread of events.
        insert_log_entry(
            &pool,
            &NewLogEntry {
                ref_id: r.id,
                collection_id: c.id,
                level: LogLevel::Info,
                phase: "cloning".into(),
                message: "cloning main".into(),
                commit_sha: None,
                files: None,
                chunks: None,
                duration_ms: None,
            },
        )
        .await
        .unwrap();
        insert_log_entry(
            &pool,
            &NewLogEntry {
                ref_id: r.id,
                collection_id: c.id,
                level: LogLevel::Error,
                phase: "error".into(),
                message: "branch not found".into(),
                commit_sha: None,
                files: None,
                chunks: None,
                duration_ms: Some(1234),
            },
        )
        .await
        .unwrap();

        let entries = list_log_entries(&pool, r.id, 10).await.unwrap();
        assert_eq!(entries.len(), 2);
        // Newest first.
        assert_eq!(entries[0].message, "branch not found");
        assert_eq!(entries[0].level, LogLevel::Error);
        assert_eq!(entries[0].duration_ms, Some(1234));
        assert_eq!(entries[1].message, "cloning main");

        // Prune to the newest 1 → the cloning entry goes.
        let pruned = prune_log_entries(&pool, r.id, 1).await.unwrap();
        assert_eq!(pruned, 1);
        let after = list_log_entries(&pool, r.id, 10).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].message, "branch not found");
    }

    #[tokio::test]
    async fn index_log_cascades_on_ref_delete() {
        // Deleting a ref must take its log rows with it (FK ON DELETE CASCADE),
        // so a removed source leaves no orphaned log behind.
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let r = add_ref(&pool, c.id, "main", None, true).await.unwrap();
        insert_log_entry(
            &pool,
            &NewLogEntry {
                ref_id: r.id,
                collection_id: c.id,
                level: LogLevel::Info,
                phase: "queued".into(),
                message: "queued".into(),
                commit_sha: None,
                files: None,
                chunks: None,
                duration_ms: None,
            },
        )
        .await
        .unwrap();
        delete_ref(&pool, r.id).await.unwrap();
        assert!(list_log_entries(&pool, r.id, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn chunks_by_vector_ids_empty_input_is_empty_output() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        assert!(
            chunks_by_vector_ids(&pool, c.id, &[])
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The predicate both collection editors re-queue on.
    ///
    /// Pinned here because the cost of a false negative is silent and severe:
    /// a corpus that keeps answering out of a store built to different
    /// settings. `PATCH /api/v0/rag/collections/{id}` used to have no check at
    /// all and could swap the embedding model without re-queueing.
    /// Two files swapping paths must not violate `UNIQUE (collection_id,
    /// path)`.
    ///
    /// Regression: moves were applied one `UPDATE` at a time, so a swap
    /// collided whichever order was chosen and the error propagated out of
    /// the incremental pass, failing the whole sync. Reorganising a folder is
    /// the ordinary case this has to survive.
    /// Asking for a rebuild must not make the corpus unsearchable while it
    /// runs.
    ///
    /// Regression: `request_full_rebuild` forced the fresh-folder path by
    /// NULLing `last_indexed_commit`, which is also what `is_searchable`
    /// reads — so clicking Re-index made `rag_search` answer "its first index
    /// hasn't completed" for the whole build, on a corpus that was sitting
    /// right there. A full rebuild is atomic; the live store serves until the
    /// swap.
    #[tokio::test]
    async fn asking_for_a_rebuild_keeps_the_collection_searchable() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let rref = add_ref(&pool, c.id, "main", None, true).await.unwrap();

        set_ref_status(&pool, rref.id, CollectionStatus::Indexing)
            .await
            .unwrap();
        assert_eq!(
            swap_ref_index(&pool, rref.id, "uuid-1", "abc123", "ocr=false,office=false")
                .await
                .unwrap(),
            1
        );
        let indexed = list_refs(&pool, c.id).await.unwrap().remove(0);
        assert!(indexed.is_searchable());
        assert!(
            !indexed.needs_full_rebuild(),
            "a ref that just finished updates in place next time"
        );

        request_full_rebuild(&pool, rref.id).await.unwrap();
        let queued = list_refs(&pool, c.id).await.unwrap().remove(0);
        assert!(
            queued.is_searchable(),
            "the live store still answers while the rebuild runs"
        );
        assert!(
            queued.needs_full_rebuild(),
            "...but the next build goes down the fresh-folder path"
        );

        // And finishing the rebuild clears the request.
        set_ref_status(&pool, rref.id, CollectionStatus::Indexing)
            .await
            .unwrap();
        swap_ref_index(&pool, rref.id, "uuid-2", "def456", "ocr=false,office=false")
            .await
            .unwrap();
        let done = list_refs(&pool, c.id).await.unwrap().remove(0);
        assert!(!done.needs_full_rebuild());
    }

    /// A ref that has never completed still rebuilds from scratch.
    /// Turning on OCR must invalidate what was indexed without it.
    ///
    /// Regression: a file the ladder could not read was recorded as skipped,
    /// and a skip does not stop a pass recording its directory versions —
    /// correctly, since the file will not become readable on its own. But it
    /// does the moment an operator wires up the missing backend. Index 500
    /// scans with no OCR pool, add one, and every later sync saw those
    /// directories unchanged and pruned past them; the scans stayed invisible
    /// until someone forced a full rebuild by hand.
    #[tokio::test]
    async fn gaining_an_extractor_invalidates_what_was_indexed_without_it() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let rref = add_ref(&pool, c.id, "main", None, true).await.unwrap();

        set_ref_status(&pool, rref.id, CollectionStatus::Indexing)
            .await
            .unwrap();
        swap_ref_index(&pool, rref.id, "uuid-1", "sha", "ocr=false,office=false")
            .await
            .unwrap();

        let built = list_refs(&pool, c.id).await.unwrap().remove(0);
        assert_eq!(
            built.extractor_fingerprint.as_deref(),
            Some("ocr=false,office=false"),
            "the build records what it could read with"
        );
        assert!(
            !built.needs_full_rebuild(),
            "nothing changed, so the next pass may diff"
        );

        // The worker compares this against its live extractor set; a
        // different answer sends the build down the fresh-folder path.
        assert_ne!(
            built.extractor_fingerprint.as_deref(),
            Some("ocr=true,office=false"),
            "wiring OCR is a different answer, and must not be diffed against"
        );

        // A ref built before the column existed has no fingerprint, which
        // also differs from any live set — so it rebuilds once and settles.
        sqlx::query("UPDATE rag_collection_refs SET extractor_fingerprint = NULL WHERE id = ?")
            .bind(rref.id)
            .execute(&pool)
            .await
            .unwrap();
        let legacy = list_refs(&pool, c.id).await.unwrap().remove(0);
        assert_eq!(legacy.extractor_fingerprint, None);
    }

    #[tokio::test]
    async fn a_never_indexed_ref_needs_a_full_build() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        let rref = add_ref(&pool, c.id, "main", None, true).await.unwrap();
        assert!(!rref.is_searchable());
        assert!(rref.needs_full_rebuild());
    }

    /// Whose access a shared corpus is read through must be recorded, not
    /// inferred.
    ///
    /// With `allowed_groups` as the only access control, "everyone who can
    /// search this collection sees what that account sees" is the fact with
    /// the security consequence — and it used to live in a doc comment and a
    /// transient "Test connection" toast.
    #[tokio::test]
    async fn the_connected_account_is_recorded_and_read_back() {
        let pool = fresh().await;
        let c = create_collection(&pool, &sample_new()).await.unwrap();
        assert_eq!(c.connected_account, None, "nothing connected yet");

        set_connected_account(&pool, c.id, Some("svc@example.com"), "alice@example.com")
            .await
            .unwrap();
        let after = find_collection_by_id(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(after.connected_account.as_deref(), Some("svc@example.com"));
        assert_eq!(after.connected_by.as_deref(), Some("alice@example.com"));
        assert!(after.connected_at.is_some());

        // A provider that cannot name the account still records who granted
        // it — a partial answer beats none.
        set_connected_account(&pool, c.id, None, "bob@example.com")
            .await
            .unwrap();
        let after = find_collection_by_id(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(after.connected_account, None);
        assert_eq!(after.connected_by.as_deref(), Some("bob@example.com"));
    }

    #[tokio::test]
    async fn files_can_swap_paths_in_one_batch() {
        let pool = fresh().await;
        let store = crate::server::db::open_collection_store(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let _ = &pool;

        let origin = FileOrigin {
            web_url: None,
            remote_id: None,
            source_version: None,
        };
        let a = upsert_file(&store, 1, "2024/report.pdf", "h1", &origin)
            .await
            .unwrap();
        let b = upsert_file(&store, 1, "2025/report.pdf", "h2", &origin)
            .await
            .unwrap();

        rename_files(
            &store,
            &[
                (a, "2025/report.pdf".to_string()),
                (b, "2024/report.pdf".to_string()),
            ],
        )
        .await
        .expect("a swap is applied as one batch, not two colliding updates");

        let path_a: String = sqlx::query_scalar("SELECT path FROM rag_files WHERE id = ?")
            .bind(a)
            .fetch_one(&store)
            .await
            .unwrap();
        let path_b: String = sqlx::query_scalar("SELECT path FROM rag_files WHERE id = ?")
            .bind(b)
            .fetch_one(&store)
            .await
            .unwrap();
        assert_eq!(path_a, "2025/report.pdf");
        assert_eq!(path_b, "2024/report.pdf");
    }

    /// And nothing is left parked if the batch is a plain move.
    #[tokio::test]
    async fn a_batch_of_moves_leaves_no_parked_rows() {
        let store = crate::server::db::open_collection_store(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let origin = FileOrigin {
            web_url: None,
            remote_id: None,
            source_version: None,
        };
        let a = upsert_file(&store, 1, "old/a.pdf", "h", &origin)
            .await
            .unwrap();
        rename_files(&store, &[(a, "new/a.pdf".to_string())])
            .await
            .unwrap();
        let parked: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM rag_files WHERE path LIKE char(1) || '%'")
                .fetch_one(&store)
                .await
                .unwrap();
        assert_eq!(parked, 0);
    }

    #[tokio::test]
    async fn index_shape_changes_are_recognised_and_credential_rotation_is_not_one() {
        let pool = fresh().await;
        let base = create_collection(&pool, &sample_new()).await.unwrap();

        assert!(
            !index_shape_changed(&base, &base),
            "an unchanged collection does not force a rebuild"
        );

        // Every field that changes how documents are reached, read or embedded.
        let embedding = Collection {
            embedding_model: "a-different-model".into(),
            ..base.clone()
        };
        assert!(
            index_shape_changed(&base, &embedding),
            "a new embedding model makes the stored vectors incomparable"
        );

        let profile = Collection {
            profile_id: Some(7),
            ..base.clone()
        };
        assert!(index_shape_changed(&base, &profile));

        let chunking = Collection {
            chunk_size: base.chunk_size + 1,
            ..base.clone()
        };
        assert!(index_shape_changed(&base, &chunking));

        let globs = Collection {
            include_globs: vec!["*.md".into()],
            ..base.clone()
        };
        assert!(index_shape_changed(&base, &globs));

        let mut moved = base.clone();
        moved.source.kind = "webdav".into();
        assert!(
            index_shape_changed(&base, &moved),
            "pointing at a different provider invalidates the corpus"
        );

        let mut elsewhere = base.clone();
        elsewhere
            .source
            .config
            .insert("root".into(), "Other/Folder".into());
        assert!(index_shape_changed(&base, &elsewhere));

        // ...but not a rotated password: it reaches the same files, and
        // rebuilding a large corpus on a routine rotation would be its own bug.
        let rotated = Collection {
            source: SourceSpec {
                secrets: Some(Sealed {
                    nonce: vec![1, 2, 3],
                    ciphertext: vec![4, 5, 6],
                }),
                ..base.source.clone()
            },
            ..base.clone()
        };
        assert!(
            !index_shape_changed(&base, &rotated),
            "rotating a credential reaches the same files"
        );

        // Nor does renaming it or editing its description.
        let renamed = Collection {
            name: "renamed".into(),
            description: Some("new words".into()),
            ..base.clone()
        };
        assert!(!index_shape_changed(&base, &renamed));
    }
}
