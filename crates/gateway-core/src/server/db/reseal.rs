// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! One-time re-sealing of secrets written under the previous at-rest key.
//!
//! # Why this exists
//!
//! The at-rest key is derived from the session secret with a domain-separation
//! label (see [`crate::server::crypto`]). That label was renamed once — from
//! `mcp-token-encryption/v1` to `at-rest-encryption/v1`, when sealing grew
//! beyond MCP tokens — and the rename shipped with **no migration**. Every
//! value sealed before it stopped decrypting, and the release notes told
//! operators to re-enter them.
//!
//! For most of these that instruction was not even possible to follow: a
//! backend API key, a connector client secret and a file-share credential are
//! all **write-only** in the UI, so "just save it again" means retyping a
//! credential the operator may not have. Losing an upstream key silently is not
//! an upgrade path.
//!
//! [`Crypto::open`] now falls back to the old key, so nothing is lost either
//! way. This pass closes the loop: it rewrites those values under the current
//! key so the fallback stops being load-bearing, and so a future release can
//! drop it.
//!
//! # Shape
//!
//! Idempotent and cheap: one `SELECT` per table, and an `UPDATE` only for rows
//! that actually need it — [`Crypto::is_legacy_sealed`] is false for anything
//! the current key already opens. A normal boot does the selects, finds
//! nothing, and moves on. Failures are logged and skipped rather than fatal: a
//! row nobody can decrypt is not made worse by leaving it alone, and refusing
//! to boot over it would take a working gateway down for a value it may never
//! read.

use crate::server::crypto::Crypto;
use crate::server::db::{DbError, Pool};

/// A sealed `(nonce, ciphertext)` column pair and the table it lives in.
struct Column {
    table: &'static str,
    /// Single-column primary key, used to address the row on update.
    id: &'static str,
    nonce: &'static str,
    ct: &'static str,
}

/// Every long-lived sealed value in the schema.
///
/// `pending_logins` and `pending_mcp_oauth` are deliberately absent: both hold
/// in-flight state that expires within minutes, so they heal on their own and a
/// migration would only race with them.
const COLUMNS: &[Column] = &[
    Column {
        table: "backends",
        id: "name",
        nonce: "api_key_nonce",
        ct: "api_key_ct",
    },
    Column {
        table: "mcp_catalog_connectors",
        id: "key",
        nonce: "client_secret_nonce",
        ct: "client_secret_ct",
    },
    Column {
        table: "rag_collections",
        id: "id",
        nonce: "source_secrets_nonce",
        ct: "source_secrets_ct",
    },
    Column {
        table: "user_mcp_connections",
        id: "id",
        nonce: "access_token_nonce",
        ct: "access_token_ct",
    },
    Column {
        table: "user_mcp_connections",
        id: "id",
        nonce: "refresh_token_nonce",
        ct: "refresh_token_ct",
    },
    Column {
        table: "user_mcp_connections",
        id: "id",
        nonce: "dcr_client_secret_nonce",
        ct: "dcr_client_secret_ct",
    },
];

/// Re-seal everything still under the previous key. Returns how many values
/// were rewritten, for logging.
pub async fn legacy_sealed_values(pool: &Pool, crypto: &Crypto) -> Result<usize, DbError> {
    let mut rewritten = 0usize;
    for col in COLUMNS {
        rewritten += one_column(pool, crypto, col).await?;
    }
    // Secrets kept as a single `"<nonce>.<ciphertext>"` string rather than a
    // BLOB pair: the VAPID key, the web-search key, the OIDC client secret and
    // every `Kind::Secret` settings field. One table, so one pass covers all.
    rewritten += app_settings_strings(pool, crypto).await?;
    Ok(rewritten)
}

async fn one_column(pool: &Pool, crypto: &Crypto, col: &Column) -> Result<usize, DbError> {
    let sql = format!(
        "SELECT {id}, {nonce}, {ct} FROM {table} WHERE {ct} IS NOT NULL AND {nonce} IS NOT NULL",
        id = col.id,
        nonce = col.nonce,
        ct = col.ct,
        table = col.table
    );
    let rows: Vec<(String, Vec<u8>, Vec<u8>)> = sqlx::query_as(&sql).fetch_all(pool).await?;

    let mut rewritten = 0usize;
    for (id, nonce, ct) in rows {
        if !crypto.is_legacy_sealed(&nonce, &ct) {
            continue;
        }
        let Ok(plain) = crypto.open(&nonce, &ct) else {
            continue;
        };
        let Ok(sealed) = crypto.seal(&plain) else {
            tracing::warn!(
                table = col.table, column = col.ct, %id,
                "re-sealing a legacy-encrypted value failed; leaving it as it is (it still \
                 decrypts through the compatibility path)"
            );
            continue;
        };
        let update = format!(
            "UPDATE {table} SET {nonce} = ?, {ct} = ? WHERE {id} = ?",
            table = col.table,
            nonce = col.nonce,
            ct = col.ct,
            id = col.id
        );
        sqlx::query(&update)
            .bind(&sealed.nonce)
            .bind(&sealed.ciphertext)
            .bind(&id)
            .execute(pool)
            .await?;
        rewritten += 1;
    }
    Ok(rewritten)
}

async fn app_settings_strings(pool: &Pool, crypto: &Crypto) -> Result<usize, DbError> {
    // Only values that *look* sealed — the table is mostly plain rows (a model
    // id, a retention count), and those must be left exactly as they are.
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM app_settings WHERE value LIKE '%.%'")
            .fetch_all(pool)
            .await?;

    let mut rewritten = 0usize;
    for (key, value) in rows {
        if !crypto.is_legacy_sealed_string(&value) {
            continue;
        }
        let Some(plain) = crypto.open_bytes_from_string(&value) else {
            continue;
        };
        let Ok(sealed) = crypto.seal_bytes_to_string(&plain) else {
            tracing::warn!(%key, "re-sealing a legacy-encrypted setting failed; leaving it");
            continue;
        };
        sqlx::query("UPDATE app_settings SET value = ? WHERE key = ?")
            .bind(&sealed)
            .bind(&key)
            .execute(pool)
            .await?;
        rewritten += 1;
    }
    Ok(rewritten)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::crypto::{LABEL, LEGACY_LABEL, derive};
    use crate::server::db::open;
    use std::path::Path;

    async fn fresh() -> Pool {
        open(Path::new(":memory:")).await.unwrap()
    }

    fn keys() -> (Crypto, Crypto) {
        let session = [3u8; 32];
        // What a pre-rename build wrote, and what this build uses.
        (
            Crypto::from_key(derive(&session, LEGACY_LABEL)),
            Crypto::from_session(&session),
        )
    }

    #[tokio::test]
    async fn a_legacy_sealed_backend_key_is_rewritten_under_the_current_key() {
        let pool = fresh().await;
        let (old, now) = keys();
        let sealed = old.seal(b"sk-upstream").unwrap();
        sqlx::query(
            "INSERT INTO backends \
             (name, base_url, created_at, updated_at, api_key_nonce, api_key_ct) \
             VALUES ('qwen', 'http://x', '2026-01-01T00:00:00Z', \
                     '2026-01-01T00:00:00Z', ?, ?)",
        )
        .bind(&sealed.nonce)
        .bind(&sealed.ciphertext)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(legacy_sealed_values(&pool, &now).await.unwrap(), 1);

        // Stored under the current key now: readable by a Crypto with no
        // legacy fallback at all, which is the whole point.
        let (nonce, ct): (Vec<u8>, Vec<u8>) =
            sqlx::query_as("SELECT api_key_nonce, api_key_ct FROM backends WHERE name = 'qwen'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let no_fallback = Crypto::from_key(derive(&[3u8; 32], LABEL));
        assert_eq!(no_fallback.open(&nonce, &ct).unwrap(), b"sk-upstream");

        // And it is idempotent.
        assert_eq!(legacy_sealed_values(&pool, &now).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_plain_app_setting_is_left_alone() {
        let pool = fresh().await;
        let (_, now) = keys();
        // Contains a dot, so it matches the LIKE filter, but is not ciphertext.
        crate::server::db::app_settings::set(&pool, "default_model.chat", "qwen.72b")
            .await
            .unwrap();

        assert_eq!(legacy_sealed_values(&pool, &now).await.unwrap(), 0);
        assert_eq!(
            crate::server::db::app_settings::get(&pool, "default_model.chat")
                .await
                .unwrap()
                .as_deref(),
            Some("qwen.72b"),
            "a plain value must survive the pass untouched"
        );
    }

    #[tokio::test]
    async fn a_legacy_sealed_setting_is_rewritten() {
        let pool = fresh().await;
        let (old, now) = keys();
        let stored = old.seal_to_string("vapid-private").unwrap();
        crate::server::db::app_settings::set(&pool, "push.vapid.private", &stored)
            .await
            .unwrap();

        assert_eq!(legacy_sealed_values(&pool, &now).await.unwrap(), 1);
        let after = crate::server::db::app_settings::get(&pool, "push.vapid.private")
            .await
            .unwrap()
            .unwrap();
        let no_fallback = Crypto::from_key(derive(&[3u8; 32], LABEL));
        assert_eq!(
            no_fallback.open_from_string(&after).as_deref(),
            Some("vapid-private")
        );
    }
}
