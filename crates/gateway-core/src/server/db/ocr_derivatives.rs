// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Cached document-OCR derivatives — the persistence half of
//! `gateway_features::server::ocr`.
//!
//! One row per (document bytes, model, prompt version, settings) tuple, which
//! is exactly the set of inputs that changes the recognised text. The
//! original upload is never touched: this table only ever holds a derived
//! result that can be thrown away and recomputed.
//!
//! Lifecycle: [`OcrStatus::Queued`] → [`OcrStatus::Running`] →
//! [`OcrStatus::Completed`] | [`OcrStatus::Failed`]. [`get`] returns the row
//! in whatever state it is in; only a `completed` row is a cache *hit* (see
//! [`Derivative::hit`]) — a `failed` row is kept for the operator but retried,
//! and a stale `running` row (a gateway killed mid-OCR) likewise re-runs
//! rather than blocking the document forever.
//!
//! Schema lives in `migrations/0054_ocr_derivatives.sql`.

use jiff::Timestamp;
use sqlx::Row;

use super::{DbError, Pool};

/// Where a cached OCR run is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrStatus {
    /// Accepted, waiting for a concurrency slot.
    Queued,
    /// In flight against the OCR backend.
    Running,
    /// Text available in `markdown`.
    Completed,
    /// Gave up; reason in `error`.
    Failed,
}

impl OcrStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    /// Parse a stored status. An unrecognised value reads as `Failed` — the
    /// conservative choice, since it means "don't serve this as text".
    pub fn parse(value: &str) -> Self {
        match value {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "completed" => Self::Completed,
            _ => Self::Failed,
        }
    }
}

/// The four inputs that decide a cached result's identity. Built by
/// `gateway_features::server::ocr`, which owns the hashing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    /// Hex SHA-256 of the original document bytes.
    pub doc_sha256: String,
    /// OCR model id (the pinned model revision, as configured).
    pub model: String,
    /// Bumped in code whenever the parsing prompt's meaning changes.
    pub prompt_version: String,
    /// Hex digest of the inference + rasterisation settings.
    pub settings_key: String,
}

/// One cached OCR run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derivative {
    pub status: OcrStatus,
    pub markdown: Option<String>,
    pub pages_total: Option<i64>,
    pub pages_processed: Option<i64>,
    pub truncated: bool,
    pub error: Option<String>,
    pub updated_at: String,
}

impl Derivative {
    /// The recognised text, if this row is a usable cache hit. `None` for a
    /// queued / running / failed row, and for a completed row whose text is
    /// somehow absent (a shape the writer never produces, but reading it as a
    /// miss is free and re-runs rather than injecting an empty block).
    pub fn hit(&self) -> Option<&str> {
        match (self.status, self.markdown.as_deref()) {
            (OcrStatus::Completed, Some(text)) if !text.is_empty() => Some(text),
            _ => None,
        }
    }
}

/// Read a cached run, in whatever state it is in.
pub async fn get(pool: &Pool, key: &CacheKey) -> Result<Option<Derivative>, DbError> {
    let row = sqlx::query(
        r#"SELECT status, markdown, pages_total, pages_processed, truncated, error, updated_at
           FROM ocr_derivatives
           WHERE doc_sha256 = ? AND model = ? AND prompt_version = ? AND settings_key = ?"#,
    )
    .bind(&key.doc_sha256)
    .bind(&key.model)
    .bind(&key.prompt_version)
    .bind(&key.settings_key)
    .fetch_optional(pool)
    .await?;
    let Some(r) = row else {
        return Ok(None);
    };
    let status: String = r.try_get("status")?;
    let truncated: i64 = r.try_get("truncated")?;
    Ok(Some(Derivative {
        status: OcrStatus::parse(&status),
        markdown: r.try_get("markdown")?,
        pages_total: r.try_get("pages_total")?,
        pages_processed: r.try_get("pages_processed")?,
        truncated: truncated != 0,
        error: r.try_get("error")?,
        updated_at: r.try_get("updated_at")?,
    }))
}

/// Claim a run: insert (or reset) the row as `queued`.
///
/// An existing row is overwritten rather than left alone, so a retry after a
/// failure — or after a crash left a `running` row behind — starts from a
/// clean slate instead of showing the previous attempt's error.
pub async fn mark_queued(
    pool: &Pool,
    key: &CacheKey,
    filename: &str,
    mime: &str,
    doc_bytes: usize,
) -> Result<(), DbError> {
    let now = Timestamp::now().to_string();
    sqlx::query(
        r#"INSERT INTO ocr_derivatives
             (doc_sha256, model, prompt_version, settings_key, filename, mime, doc_bytes,
              status, markdown, pages_total, pages_processed, truncated, error,
              created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, 'queued', NULL, NULL, NULL, 0, NULL, ?, ?)
           ON CONFLICT(doc_sha256, model, prompt_version, settings_key) DO UPDATE SET
             filename        = excluded.filename,
             mime            = excluded.mime,
             doc_bytes       = excluded.doc_bytes,
             status          = 'queued',
             markdown        = NULL,
             pages_total     = NULL,
             pages_processed = NULL,
             truncated       = 0,
             error           = NULL,
             updated_at      = excluded.updated_at"#,
    )
    .bind(&key.doc_sha256)
    .bind(&key.model)
    .bind(&key.prompt_version)
    .bind(&key.settings_key)
    .bind(filename)
    .bind(mime)
    .bind(doc_bytes as i64)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Flip a claimed row to `running` — a concurrency slot came free.
pub async fn mark_running(pool: &Pool, key: &CacheKey) -> Result<(), DbError> {
    set_status(pool, key, OcrStatus::Running).await
}

/// Store a finished run's text and page tally.
pub async fn complete(
    pool: &Pool,
    key: &CacheKey,
    markdown: &str,
    pages_total: Option<usize>,
    pages_processed: Option<usize>,
    truncated: bool,
) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE ocr_derivatives
           SET status = 'completed', markdown = ?, pages_total = ?, pages_processed = ?,
               truncated = ?, error = NULL, updated_at = ?
           WHERE doc_sha256 = ? AND model = ? AND prompt_version = ? AND settings_key = ?"#,
    )
    .bind(markdown)
    .bind(pages_total.map(|p| p as i64))
    .bind(pages_processed.map(|p| p as i64))
    .bind(i64::from(truncated))
    .bind(Timestamp::now().to_string())
    .bind(&key.doc_sha256)
    .bind(&key.model)
    .bind(&key.prompt_version)
    .bind(&key.settings_key)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a failed run. The row survives so the operator can see *why* the
/// document has no OCR text, but [`Derivative::hit`] reads it as a miss.
pub async fn fail(pool: &Pool, key: &CacheKey, error: &str) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE ocr_derivatives
           SET status = 'failed', error = ?, updated_at = ?
           WHERE doc_sha256 = ? AND model = ? AND prompt_version = ? AND settings_key = ?"#,
    )
    .bind(error)
    .bind(Timestamp::now().to_string())
    .bind(&key.doc_sha256)
    .bind(&key.model)
    .bind(&key.prompt_version)
    .bind(&key.settings_key)
    .execute(pool)
    .await?;
    Ok(())
}

async fn set_status(pool: &Pool, key: &CacheKey, status: OcrStatus) -> Result<(), DbError> {
    sqlx::query(
        r#"UPDATE ocr_derivatives
           SET status = ?, updated_at = ?
           WHERE doc_sha256 = ? AND model = ? AND prompt_version = ? AND settings_key = ?"#,
    )
    .bind(status.as_str())
    .bind(Timestamp::now().to_string())
    .bind(&key.doc_sha256)
    .bind(&key.model)
    .bind(&key.prompt_version)
    .bind(&key.settings_key)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::open;
    use std::path::Path;

    fn key() -> CacheKey {
        CacheKey {
            doc_sha256: "abc".into(),
            model: "unlimited-ocr".into(),
            prompt_version: "v1".into(),
            settings_key: "s1".into(),
        }
    }

    #[tokio::test]
    async fn miss_then_lifecycle_to_hit() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        let k = key();
        assert_eq!(get(&pool, &k).await.unwrap(), None);

        mark_queued(&pool, &k, "scan.pdf", "application/pdf", 4096)
            .await
            .unwrap();
        let row = get(&pool, &k).await.unwrap().unwrap();
        assert_eq!(row.status, OcrStatus::Queued);
        // Queued is not servable text.
        assert_eq!(row.hit(), None);

        mark_running(&pool, &k).await.unwrap();
        assert_eq!(
            get(&pool, &k).await.unwrap().unwrap().status,
            OcrStatus::Running
        );

        complete(&pool, &k, "# Page one", Some(3), Some(3), false)
            .await
            .unwrap();
        let row = get(&pool, &k).await.unwrap().unwrap();
        assert_eq!(row.hit(), Some("# Page one"));
        assert_eq!(row.pages_total, Some(3));
        assert_eq!(row.pages_processed, Some(3));
        assert!(!row.truncated);
    }

    #[tokio::test]
    async fn failed_row_is_kept_but_reads_as_a_miss() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        let k = key();
        mark_queued(&pool, &k, "scan.pdf", "application/pdf", 10)
            .await
            .unwrap();
        fail(&pool, &k, "OCR upstream returned status 503")
            .await
            .unwrap();

        let row = get(&pool, &k).await.unwrap().unwrap();
        assert_eq!(row.status, OcrStatus::Failed);
        assert_eq!(row.hit(), None);
        assert_eq!(
            row.error.as_deref(),
            Some("OCR upstream returned status 503")
        );

        // Re-claiming clears the previous attempt's error so a retry doesn't
        // show a stale failure while it runs.
        mark_queued(&pool, &k, "scan.pdf", "application/pdf", 10)
            .await
            .unwrap();
        let row = get(&pool, &k).await.unwrap().unwrap();
        assert_eq!(row.status, OcrStatus::Queued);
        assert_eq!(row.error, None);
    }

    #[tokio::test]
    async fn every_key_field_is_part_of_the_identity() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        let base = key();
        mark_queued(&pool, &base, "a.pdf", "application/pdf", 1)
            .await
            .unwrap();
        complete(&pool, &base, "text", Some(1), Some(1), false)
            .await
            .unwrap();

        for changed in [
            CacheKey {
                doc_sha256: "different".into(),
                ..base.clone()
            },
            CacheKey {
                model: "other-ocr".into(),
                ..base.clone()
            },
            CacheKey {
                prompt_version: "v2".into(),
                ..base.clone()
            },
            CacheKey {
                settings_key: "s2".into(),
                ..base.clone()
            },
        ] {
            assert_eq!(
                get(&pool, &changed).await.unwrap(),
                None,
                "changing one key field must miss"
            );
        }
    }

    #[tokio::test]
    async fn truncated_run_reports_partial_pages() {
        let pool = open(Path::new(":memory:")).await.unwrap();
        let k = key();
        mark_queued(&pool, &k, "long.pdf", "application/pdf", 1)
            .await
            .unwrap();
        complete(&pool, &k, "head", Some(120), Some(64), true)
            .await
            .unwrap();
        let row = get(&pool, &k).await.unwrap().unwrap();
        assert!(row.truncated);
        assert_eq!(
            (row.pages_total, row.pages_processed),
            (Some(120), Some(64))
        );
    }
}
