// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Background scheduler for async ComfyUI jobs.
//!
//! Spawned at boot (alongside the RAG indexer, usage batcher, etc.).
//! Every `poll_interval` it reads pending jobs from `comfyui_jobs`,
//! polls ComfyUI's `/history/{prompt_id}` for each, and on completion
//! fetches the produced asset, re-hosts it in the chat-attachment S3
//! bucket, and appends an attachment marker to the owning turn's
//! content — so the user sees the result appear in their chat bubble
//! without a new message or a page refresh.
//!
//! Boot-tolerant: jobs live in the DB, not in memory. If the gateway
//! restarts mid-job, ComfyUI keeps running (it's a separate process),
//! and the scheduler picks the job back up on the next poll cycle.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;

use super::client::{Client, ProducedAsset, StatusCheck};
use super::jobs::{self, ComfyuiJob};
use crate::server::chat_attachments;
use crate::server::config::S3Config;
use crate::server::db::Pool;

/// Cheaply-cloneable scheduler handle. Spawned once at boot and held
/// for the process lifetime. The background task takes ownership of
/// the inner handles; cloning the outer struct just shares the
/// config + shutdown signal.
#[derive(Clone)]
pub struct ComfyuiScheduler {
    db: Pool,
    client: Client,
    s3: Option<Arc<S3Config>>,
    poll_interval: Duration,
    /// Hard ceiling on how long a single job can stay pending before
    /// the scheduler declares it timed out. Mirrors the operator's
    /// `[comfyui] timeout_secs`.
    timeout: Duration,
    chat_updates: super::ChatUpdateRegistry,
}

impl std::fmt::Debug for ComfyuiScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComfyuiScheduler")
            .field("base_url", &self.client.base_url())
            .field("poll_interval", &self.poll_interval)
            .field("timeout", &self.timeout)
            .field("s3_configured", &self.s3.is_some())
            .finish_non_exhaustive()
    }
}

impl ComfyuiScheduler {
    pub fn new(
        db: Pool,
        client: Client,
        s3: Option<Arc<S3Config>>,
        poll_interval: Duration,
        timeout: Duration,
        chat_updates: super::ChatUpdateRegistry,
    ) -> Self {
        Self {
            db,
            client,
            s3,
            poll_interval,
            timeout,
            chat_updates,
        }
    }

    /// Spawn the background poll loop. Returns immediately; the task
    /// runs for the process lifetime. Errors are logged and swallowed
    /// — a transient DB or network failure in one cycle doesn't kill
    /// the scheduler.
    pub fn spawn(self) {
        tokio::spawn(async move {
            tracing::info!(
                poll_interval_ms = self.poll_interval.as_millis(),
                timeout_secs = self.timeout.as_secs(),
                "ComfyUI job scheduler started",
            );
            loop {
                if let Err(e) = self.poll_cycle().await {
                    tracing::warn!(error = %e, "ComfyUI scheduler poll cycle failed");
                }
                tokio::time::sleep(self.poll_interval).await;
            }
        });
    }

    /// One poll cycle. Reads all pending jobs, checks each against
    /// ComfyUI, and handles completions / failures / timeouts.
    async fn poll_cycle(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let pending = jobs::pending(&self.db).await?;
        if pending.is_empty() {
            return Ok(());
        }
        tracing::debug!(
            count = pending.len(),
            "ComfyUI scheduler: polling pending jobs"
        );
        for job in pending {
            // Timeout check first — cheap, no network.
            if self.is_timed_out(&job) {
                self.handle_timeout(&job).await;
                continue;
            }
            // Poll ComfyUI.
            match self.poll_job(&job).await {
                Ok(PollResult::Pending) => {}
                Ok(PollResult::Completed { asset }) => {
                    self.handle_completion(&job, &asset).await;
                }
                Ok(PollResult::Failed(reason)) => {
                    self.handle_failure(&job, &reason).await;
                }
                Err(e) => {
                    tracing::warn!(
                        job_id = job.id,
                        prompt_id = %job.prompt_id,
                        error = %e,
                        "ComfyUI scheduler: poll error for job",
                    );
                }
            }
        }
        Ok(())
    }

    fn is_timed_out(&self, job: &ComfyuiJob) -> bool {
        let Ok(created) = job.created_at.parse::<Timestamp>() else {
            return false;
        };
        let elapsed = Timestamp::now().as_second() - created.as_second();
        elapsed > self.timeout.as_secs() as i64
    }

    async fn poll_job(&self, job: &ComfyuiJob) -> Result<PollResult, String> {
        let status = self
            .client
            .check_status(&job.prompt_id, &job.output_node_id)
            .await
            .map_err(|e| e.to_string())?;
        match status {
            StatusCheck::Pending => Ok(PollResult::Pending),
            StatusCheck::Completed(assets) => {
                let asset = assets
                    .into_iter()
                    .next()
                    .ok_or_else(|| "no output asset".to_string())?;
                Ok(PollResult::Completed { asset })
            }
            StatusCheck::Failed(reason) => Ok(PollResult::Failed(reason)),
        }
    }

    async fn handle_completion(&self, job: &ComfyuiJob, asset: &ProducedAsset) {
        // 1. Fetch the asset bytes from ComfyUI.
        let downloaded = match self.client.fetch_asset(asset).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(job_id = job.id, error = %e, "ComfyUI scheduler: fetch asset failed");
                jobs::fail(&self.db, job.id, &format!("asset fetch failed: {e}"))
                    .await
                    .ok();
                return;
            }
        };
        // 2. Upload to S3.
        let Some(s3) = self.s3.as_ref() else {
            tracing::warn!(
                job_id = job.id,
                "ComfyUI scheduler: [chat.s3] not configured — cannot re-host asset",
            );
            jobs::fail(
                &self.db,
                job.id,
                "chat S3 not configured — nowhere to store the asset",
            )
            .await
            .ok();
            return;
        };
        let ext = ext_for_mime(&downloaded.mime);
        // Include the job id + a short slice of prompt_id for uniqueness
        // — two comfyui_text_to_image calls in one turn would otherwise
        // collide on <turn_id>/comfyui-text_to_image.png and the second
        // upload would silently overwrite the first.
        let prompt_short: String = job.prompt_id.chars().take(8).collect();
        let filename = format!("comfyui-{}-{}{}", job.workflow_id, prompt_short, ext);
        let upload = match chat_attachments::upload(
            s3,
            &job.turn_id,
            &filename,
            &downloaded.mime,
            downloaded.bytes,
        )
        .await
        {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(job_id = job.id, error = %e, "ComfyUI scheduler: S3 upload failed");
                jobs::fail(&self.db, job.id, &format!("S3 upload failed: {e}"))
                    .await
                    .ok();
                return;
            }
        };
        // Mark the job completed BEFORE appending the marker. If complete
        // fails (SQLite BUSY), the job stays pending and gets re-polled
        // — but we haven't appended the marker yet, so no duplicates.
        // On the next poll, ComfyUI returns completed again and we retry
        // the whole chain. The S3 upload may produce a duplicate object
        // (same content, same key), but that's idempotent — only the
        // marker append matters for dedup, and it only runs after
        // complete succeeds.
        if let Err(e) = jobs::complete(&self.db, job.id, &upload.filename, &upload.mime).await {
            tracing::warn!(job_id = job.id, error = %e, "ComfyUI scheduler: complete DB write failed, will retry");
            return;
        }
        // Append the attachment marker to the owning turn.
        let marker = chat_attachments::marker_line(&job.turn_id, &upload);
        let chunk = format!("\n\n{marker}\n\n");
        if let Err(e) = session_core::db::append_content(&self.db, &job.turn_id, &chunk).await {
            tracing::warn!(job_id = job.id, error = %e, "ComfyUI scheduler: append to turn failed (job is already marked completed)");
        }
        self.chat_updates.notify(&job.session_id);
        tracing::info!(
            job_id = job.id,
            workflow = %job.workflow_id,
            filename = %upload.filename,
            "ComfyUI job completed — asset re-hosted and appended to turn",
        );
    }

    async fn handle_failure(&self, job: &ComfyuiJob, reason: &str) {
        tracing::warn!(
            job_id = job.id,
            prompt_id = %job.prompt_id,
            reason,
            "ComfyUI job failed",
        );
        // Append an error note to the turn so the user sees it.
        let note = format!("\n\n> ⚠️ ComfyUI generation failed: {reason}\n\n");
        let _ = session_core::db::append_content(&self.db, &job.turn_id, &note).await;
        let _ = jobs::fail(&self.db, job.id, reason).await;
    }

    async fn handle_timeout(&self, job: &ComfyuiJob) {
        let reason = format!("Timed out after {} seconds", self.timeout.as_secs());
        tracing::warn!(
            job_id = job.id,
            prompt_id = %job.prompt_id,
            "ComfyUI job timed out",
        );
        let note = format!(
            "\n\n> ⚠️ ComfyUI generation timed out (no result after {}s).\n\n",
            self.timeout.as_secs()
        );
        let _ = session_core::db::append_content(&self.db, &job.turn_id, &note).await;
        let _ = jobs::fail(&self.db, job.id, &reason).await;
    }
}

enum PollResult {
    Pending,
    Completed { asset: ProducedAsset },
    Failed(String),
}

fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/webp" => ".webp",
        "image/gif" => ".gif",
        "video/mp4" => ".mp4",
        "video/webm" => ".webm",
        "audio/mpeg" => ".mp3",
        "audio/wav" => ".wav",
        _ => ".bin",
    }
}
