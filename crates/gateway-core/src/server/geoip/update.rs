// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Optional weekly IP2Location LITE updater. When an IP2Location download token is configured
//! it fetches the DB11 LITE distribution, unzips the `.BIN`, and writes
//! it atomically to the configured `db_path`.
//!
//! It deliberately does **not** touch the in-memory reader: it only
//! writes the file. [`super::GeoIp::watch`] notices the atomic rename
//! and reloads — one code path for "file changed" whether it was us or
//! an operator. With no token the task is never spawned, so the gateway
//! runs unchanged.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// IP2Location's download endpoint. The LITE DB11 (IPv4+IPv6, BIN) is
/// `file=DB11LITEBIN`, the same artifact the ERP pulls.
const DOWNLOAD_URL: &str = "https://www.ip2location.com/download/";
const LITE_FILE: &str = "DB11LITEBIN";
/// Refresh cadence + the "is it stale?" threshold. IP2Location publishes
/// LITE updates roughly monthly; weekly keeps us close without hammering.
const UPDATE_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// A real DB11 BIN is tens of MB; anything tiny is an error/quota page
/// returned with a 200, not a database.
const MIN_PLAUSIBLE_BYTES: usize = 1024 * 1024;

/// Spawn the background updater. No-op (logs and returns without
/// spawning) when no token is configured — the explicit "works without a
/// token" path.
pub fn spawn(db_path: PathBuf, token: Option<String>) {
    let Some(token) = token.filter(|t| !t.trim().is_empty()) else {
        tracing::info!("geoip auto-update disabled (no IP2Location token configured)");
        return;
    };
    tokio::spawn(async move {
        // Brief startup delay so boot logs stay readable and we don't add
        // network load to the cold-start path.
        tokio::time::sleep(Duration::from_secs(15)).await;
        loop {
            if should_update(&db_path) {
                match update_once(&db_path, &token).await {
                    Ok(()) => tracing::info!(
                        path = %db_path.display(),
                        "geoip database updated (watcher will reload it)"
                    ),
                    Err(err) => {
                        tracing::warn!(error = %err, "geoip update failed; will retry next cycle")
                    }
                }
            } else {
                tracing::debug!(path = %db_path.display(), "geoip database fresh; skipping update");
            }
            tokio::time::sleep(UPDATE_INTERVAL).await;
        }
    });
}

/// True when the file is missing or older than [`UPDATE_INTERVAL`].
fn should_update(db_path: &Path) -> bool {
    match std::fs::metadata(db_path).and_then(|m| m.modified()) {
        Ok(modified) => modified
            .elapsed()
            .map(|age| age >= UPDATE_INTERVAL)
            .unwrap_or(true),
        Err(_) => true,
    }
}

async fn update_once(db_path: &Path, token: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .user_agent(concat!(
            "llm-gateway/",
            env!("CARGO_PKG_VERSION"),
            " geoip-update"
        ))
        .build()?;
    let resp = client
        .get(DOWNLOAD_URL)
        .query(&[("token", token), ("file", LITE_FILE)])
        .send()
        .await?
        .error_for_status()?;
    let bytes = resp.bytes().await?;
    // IP2Location answers quota/auth problems with a short text body and a
    // 200, so size-guard before treating the payload as an archive.
    anyhow::ensure!(
        bytes.len() >= MIN_PLAUSIBLE_BYTES,
        "download is only {} bytes — likely a quota/auth message from IP2Location, not a database",
        bytes.len()
    );
    let db_path = db_path.to_path_buf();
    // Unzip + atomic install is blocking sync IO/CPU — keep it off the
    // async runtime.
    tokio::task::spawn_blocking(move || extract_and_install(&bytes, &db_path)).await??;
    Ok(())
}

/// Extract the first `*.BIN` entry from the downloaded zip and install it
/// atomically as `db_path` (write `<path>.tmp`, fsync, rename).
fn extract_and_install(zip_bytes: &[u8], db_path: &Path) -> anyhow::Result<()> {
    let reader = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(reader)?;

    let bin_index = (0..archive.len()).find(|&i| {
        archive
            .by_index(i)
            .ok()
            .is_some_and(|e| e.name().to_ascii_uppercase().ends_with(".BIN"))
    });
    let idx =
        bin_index.ok_or_else(|| anyhow::anyhow!("no .BIN entry in the IP2Location archive"))?;
    let mut entry = archive.by_index(idx)?;

    let parent = db_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("db_path has no parent directory"))?;
    std::fs::create_dir_all(parent)?;

    let tmp = db_path.with_extension("bin.tmp");
    {
        let mut out = std::fs::File::create(&tmp)?;
        std::io::copy(&mut entry, &mut out)?;
        out.sync_all()?;
    }
    // Atomic on the same filesystem — the watcher sees one rename event,
    // never a half-written file.
    std::fs::rename(&tmp, db_path)?;
    Ok(())
}
