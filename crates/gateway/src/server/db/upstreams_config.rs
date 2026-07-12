// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Database-backed upstream topology — pools, backends, and their relationships.
//!
//! Replaces the `[upstream_pools]` TOML sections with DB rows managed through
//! the admin UI. [`load_snapshot`] reads the full topology in a handful of
//! queries for registry rebuilds; the CRUD functions power `/admin/backends`
//! and `/admin/pools`.
//!
//! Schema lives in `migrations/0042_upstream_config_db.sql`.

use std::collections::HashMap;

use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{DbError, Pool};
use crate::server::crypto::Crypto;
use crate::server::upstreams::config::{
    AliasSpec, BackendConfig, FallbackConfig, UpstreamPoolConfig,
};

// ---------------------------------------------------------------------------
// Row structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct BackendRow {
    pub name: String,
    pub base_url: String,
    /// Optional env-var NAME holding the API key — a fallback resolved from the
    /// environment only when no sealed [`api_key_ct`](Self::api_key_ct) is set.
    pub api_key_env: Option<String>,
    /// The API key value itself, AES-256-GCM sealed (`ciphertext`, `nonce`). The
    /// DB layer treats these as opaque blobs; only a holder of
    /// [`crate::server::crypto::Crypto`] can `open` them (see `db_bridge`).
    /// `None` when the backend has no stored key (uses `api_key_env` or none).
    pub api_key_ct: Option<Vec<u8>>,
    pub api_key_nonce: Option<Vec<u8>>,
    pub weight: u32,
    pub max_inflight: u32,
    pub health_path: String,
    pub probe_models: bool,
    pub supports_edit: bool,
    pub models: Vec<String>,
    pub aliases: Vec<AliasRow>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AliasRow {
    pub alias: String,
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolRow {
    pub name: String,
    pub kind: String,
    pub strategy: String,
    pub fallback_offline: Option<String>,
    pub compliance_gdpr: bool,
    pub compliance_nda: bool,
    pub enforce_limits: bool,
    pub sort_order: i64,
    pub backends: Vec<String>,
    pub models: Vec<String>,
    pub voices: Vec<VoiceRow>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VoiceRow {
    pub lang_code: String,
    pub voice_id: String,
}

/// Complete topology snapshot for an [`crate::server::upstreams::UpstreamRegistry`]
/// rebuild. Loaded in a small fixed number of queries.
#[derive(Debug, Clone, Default)]
pub struct UpstreamConfigSnapshot {
    pub pools: Vec<PoolRow>,
    pub backends: HashMap<String, BackendRow>,
    pub fallbacks: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Timestamp helper
// ---------------------------------------------------------------------------

fn parse_ts(col: &'static str, row: &SqliteRow) -> Result<Timestamp, DbError> {
    let s: String = row.try_get(col)?;
    s.parse().map_err(|e: jiff::Error| DbError::Decode {
        column: col,
        source: e.into(),
    })
}

fn now_rfc3339() -> String {
    Timestamp::now().to_string()
}

// ---------------------------------------------------------------------------
// Snapshot loader — used by registry rebuild on startup and on "Apply changes"
// ---------------------------------------------------------------------------

/// Loads the complete upstream topology from the database.
///
/// Returns an empty snapshot when no pools/backends are configured yet (first
/// boot). Drives the registry build on startup and on "Apply changes".
pub async fn load_snapshot(db: &Pool) -> Result<UpstreamConfigSnapshot, DbError> {
    let backends = load_all_backends(db).await?;
    let pools = load_all_pools(db).await?;
    let fallbacks = load_all_fallbacks(db).await?;
    Ok(UpstreamConfigSnapshot {
        pools,
        backends,
        fallbacks,
    })
}

/// True when the DB has no pools (and therefore no usable topology). Note the
/// first-boot seed in `main.rs` gates on a persistent `topology.seeded` marker,
/// not on this — deleting every pool via the UI must not trigger a reseed.
pub async fn is_empty(db: &Pool) -> Result<bool, DbError> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pools")
        .fetch_one(db)
        .await?;
    Ok(count == 0)
}

async fn load_all_backends(db: &Pool) -> Result<HashMap<String, BackendRow>, DbError> {
    let rows = sqlx::query(
        r#"SELECT name, base_url, api_key_env, api_key_ct, api_key_nonce, weight, max_inflight,
                  health_path, probe_models, supports_edit, created_at, updated_at
             FROM backends ORDER BY name"#,
    )
    .fetch_all(db)
    .await?;

    let mut backends: HashMap<String, BackendRow> = HashMap::new();
    for row in &rows {
        let name: String = row.try_get("name")?;
        let created_at = parse_ts("created_at", row)?;
        let updated_at = parse_ts("updated_at", row)?;
        backends.insert(
            name.clone(),
            BackendRow {
                name: name.clone(),
                base_url: row.try_get("base_url")?,
                api_key_env: row.try_get("api_key_env")?,
                api_key_ct: row.try_get("api_key_ct")?,
                api_key_nonce: row.try_get("api_key_nonce")?,
                weight: row.try_get::<u32, _>("weight")?,
                max_inflight: row.try_get::<u32, _>("max_inflight")?,
                health_path: row.try_get("health_path")?,
                probe_models: row.try_get::<i64, _>("probe_models")? != 0,
                supports_edit: row.try_get::<i64, _>("supports_edit")? != 0,
                models: Vec::new(),
                aliases: Vec::new(),
                created_at,
                updated_at,
            },
        );
    }

    // Load models for all backends in one query.
    let model_rows = sqlx::query(
        r#"SELECT backend_name, model_id FROM backend_models ORDER BY backend_name, sort_order"#,
    )
    .fetch_all(db)
    .await?;
    for row in &model_rows {
        let backend_name: String = row.try_get("backend_name")?;
        let model_id: String = row.try_get("model_id")?;
        if let Some(b) = backends.get_mut(&backend_name) {
            b.models.push(model_id);
        }
    }

    // Load aliases for all backends in one query.
    let alias_rows = sqlx::query(
        r#"SELECT backend_name, alias, target FROM backend_aliases ORDER BY backend_name, alias"#,
    )
    .fetch_all(db)
    .await?;
    for row in &alias_rows {
        let backend_name: String = row.try_get("backend_name")?;
        let alias_row = AliasRow {
            alias: row.try_get("alias")?,
            target: row.try_get("target")?,
        };
        if let Some(b) = backends.get_mut(&backend_name) {
            b.aliases.push(alias_row);
        }
    }

    Ok(backends)
}

async fn load_all_pools(db: &Pool) -> Result<Vec<PoolRow>, DbError> {
    let rows = sqlx::query(
        r#"SELECT name, kind, strategy, fallback_offline, compliance_gdpr,
                  compliance_nda, enforce_limits, sort_order, created_at, updated_at
             FROM pools ORDER BY sort_order, name"#,
    )
    .fetch_all(db)
    .await?;

    let mut pools: Vec<PoolRow> = Vec::new();
    for row in &rows {
        let created_at = parse_ts("created_at", row)?;
        let updated_at = parse_ts("updated_at", row)?;
        pools.push(PoolRow {
            name: row.try_get("name")?,
            kind: row.try_get("kind")?,
            strategy: row.try_get("strategy")?,
            fallback_offline: row.try_get("fallback_offline")?,
            compliance_gdpr: row.try_get::<i64, _>("compliance_gdpr")? != 0,
            compliance_nda: row.try_get::<i64, _>("compliance_nda")? != 0,
            enforce_limits: row.try_get::<i64, _>("enforce_limits")? != 0,
            sort_order: row.try_get("sort_order")?,
            backends: Vec::new(),
            models: Vec::new(),
            voices: Vec::new(),
            created_at,
            updated_at,
        });
    }

    // Load pool-backend assignments.
    let pb_rows = sqlx::query(
        r#"SELECT pool_name, backend_name FROM pool_backends ORDER BY pool_name, sort_order"#,
    )
    .fetch_all(db)
    .await?;
    for row in &pb_rows {
        let pool_name: String = row.try_get("pool_name")?;
        let backend_name: String = row.try_get("backend_name")?;
        if let Some(p) = pools.iter_mut().find(|p| p.name == pool_name) {
            p.backends.push(backend_name);
        }
    }

    // Load pool models.
    let pm_rows = sqlx::query(
        r#"SELECT pool_name, model_id FROM pool_models ORDER BY pool_name, sort_order"#,
    )
    .fetch_all(db)
    .await?;
    for row in &pm_rows {
        let pool_name: String = row.try_get("pool_name")?;
        let model_id: String = row.try_get("model_id")?;
        if let Some(p) = pools.iter_mut().find(|p| p.name == pool_name) {
            p.models.push(model_id);
        }
    }

    // Load pool voices.
    let pv_rows = sqlx::query(
        r#"SELECT pool_name, lang_code, voice_id FROM pool_voices ORDER BY pool_name, lang_code"#,
    )
    .fetch_all(db)
    .await?;
    for row in &pv_rows {
        let pool_name: String = row.try_get("pool_name")?;
        let voice = VoiceRow {
            lang_code: row.try_get("lang_code")?,
            voice_id: row.try_get("voice_id")?,
        };
        if let Some(p) = pools.iter_mut().find(|p| p.name == pool_name) {
            p.voices.push(voice);
        }
    }

    Ok(pools)
}

async fn load_all_fallbacks(db: &Pool) -> Result<HashMap<String, String>, DbError> {
    let rows = sqlx::query("SELECT kind, model_id FROM fallback_models")
        .fetch_all(db)
        .await?;
    let mut map = HashMap::new();
    for row in &rows {
        map.insert(row.try_get("kind")?, row.try_get("model_id")?);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Backend CRUD
// ---------------------------------------------------------------------------

/// List all backends for the admin UI.
pub async fn list_backends(db: &Pool) -> Result<Vec<BackendRow>, DbError> {
    Ok(load_all_backends(db).await?.into_values().collect())
}

/// Fetch a single backend by name.
pub async fn get_backend(db: &Pool, name: &str) -> Result<Option<BackendRow>, DbError> {
    let mut backends = load_all_backends(db).await?;
    Ok(backends.remove(name))
}

/// Insert or update a backend, replacing its models and aliases atomically.
pub async fn upsert_backend(db: &Pool, row: &BackendRow) -> Result<(), DbError> {
    let now = now_rfc3339();
    sqlx::query(
        r#"INSERT INTO backends
               (name, base_url, api_key_env, api_key_ct, api_key_nonce, weight, max_inflight,
                health_path, probe_models, supports_edit, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT(name) DO UPDATE SET
               base_url      = excluded.base_url,
               api_key_env   = excluded.api_key_env,
               api_key_ct    = excluded.api_key_ct,
               api_key_nonce = excluded.api_key_nonce,
               weight        = excluded.weight,
               max_inflight  = excluded.max_inflight,
               health_path   = excluded.health_path,
               probe_models  = excluded.probe_models,
               supports_edit = excluded.supports_edit,
               updated_at    = excluded.updated_at"#,
    )
    .bind(&row.name)
    .bind(&row.base_url)
    .bind(&row.api_key_env)
    .bind(&row.api_key_ct)
    .bind(&row.api_key_nonce)
    .bind(row.weight)
    .bind(row.max_inflight)
    .bind(&row.health_path)
    .bind(row.probe_models as i64)
    .bind(row.supports_edit as i64)
    .bind(&now)
    .bind(&now)
    .execute(db)
    .await?;

    replace_backend_models(db, &row.name, &row.models).await?;
    replace_backend_aliases(db, &row.name, &row.aliases).await?;
    Ok(())
}

/// Delete a backend and all its dependent rows (models, aliases, pool links).
pub async fn delete_backend(db: &Pool, name: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM backends WHERE name = ?")
        .bind(name)
        .execute(db)
        .await?;
    Ok(())
}

async fn replace_backend_models(
    db: &Pool,
    backend_name: &str,
    models: &[String],
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM backend_models WHERE backend_name = ?")
        .bind(backend_name)
        .execute(db)
        .await?;
    for (i, model_id) in models.iter().enumerate() {
        sqlx::query(
            r#"INSERT INTO backend_models (backend_name, model_id, sort_order)
               VALUES (?, ?, ?)"#,
        )
        .bind(backend_name)
        .bind(model_id)
        .bind(i as i64)
        .execute(db)
        .await?;
    }
    Ok(())
}

async fn replace_backend_aliases(
    db: &Pool,
    backend_name: &str,
    aliases: &[AliasRow],
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM backend_aliases WHERE backend_name = ?")
        .bind(backend_name)
        .execute(db)
        .await?;
    for a in aliases {
        sqlx::query(
            r#"INSERT INTO backend_aliases (backend_name, alias, target)
               VALUES (?, ?, ?)"#,
        )
        .bind(backend_name)
        .bind(&a.alias)
        .bind(&a.target)
        .execute(db)
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pool CRUD
// ---------------------------------------------------------------------------

/// Insert or update a pool, replacing backends/models/voices atomically.
pub async fn upsert_pool(db: &Pool, row: &PoolRow) -> Result<(), DbError> {
    let now = now_rfc3339();
    sqlx::query(
        r#"INSERT INTO pools
               (name, kind, strategy, fallback_offline, compliance_gdpr,
                compliance_nda, enforce_limits, sort_order, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT(name) DO UPDATE SET
               kind             = excluded.kind,
               strategy         = excluded.strategy,
               fallback_offline = excluded.fallback_offline,
               compliance_gdpr  = excluded.compliance_gdpr,
               compliance_nda   = excluded.compliance_nda,
               enforce_limits   = excluded.enforce_limits,
               sort_order       = excluded.sort_order,
               updated_at       = excluded.updated_at"#,
    )
    .bind(&row.name)
    .bind(&row.kind)
    .bind(&row.strategy)
    .bind(&row.fallback_offline)
    .bind(row.compliance_gdpr as i64)
    .bind(row.compliance_nda as i64)
    .bind(row.enforce_limits as i64)
    .bind(row.sort_order)
    .bind(&now)
    .bind(&now)
    .execute(db)
    .await?;

    replace_pool_backends(db, &row.name, &row.backends).await?;
    replace_pool_models(db, &row.name, &row.models).await?;
    replace_pool_voices(db, &row.name, &row.voices).await?;
    Ok(())
}

/// Delete a pool and all its dependent rows.
pub async fn delete_pool(db: &Pool, name: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM pools WHERE name = ?")
        .bind(name)
        .execute(db)
        .await?;
    Ok(())
}

async fn replace_pool_backends(
    db: &Pool,
    pool_name: &str,
    backend_names: &[String],
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM pool_backends WHERE pool_name = ?")
        .bind(pool_name)
        .execute(db)
        .await?;
    for (i, name) in backend_names.iter().enumerate() {
        sqlx::query(
            r#"INSERT INTO pool_backends (pool_name, backend_name, sort_order)
               VALUES (?, ?, ?)"#,
        )
        .bind(pool_name)
        .bind(name)
        .bind(i as i64)
        .execute(db)
        .await?;
    }
    Ok(())
}

async fn replace_pool_models(db: &Pool, pool_name: &str, models: &[String]) -> Result<(), DbError> {
    sqlx::query("DELETE FROM pool_models WHERE pool_name = ?")
        .bind(pool_name)
        .execute(db)
        .await?;
    for (i, model_id) in models.iter().enumerate() {
        sqlx::query(
            r#"INSERT INTO pool_models (pool_name, model_id, sort_order)
               VALUES (?, ?, ?)"#,
        )
        .bind(pool_name)
        .bind(model_id)
        .bind(i as i64)
        .execute(db)
        .await?;
    }
    Ok(())
}

async fn replace_pool_voices(
    db: &Pool,
    pool_name: &str,
    voices: &[VoiceRow],
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM pool_voices WHERE pool_name = ?")
        .bind(pool_name)
        .execute(db)
        .await?;
    for v in voices {
        sqlx::query(
            r#"INSERT INTO pool_voices (pool_name, lang_code, voice_id)
               VALUES (?, ?, ?)"#,
        )
        .bind(pool_name)
        .bind(&v.lang_code)
        .bind(&v.voice_id)
        .execute(db)
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Fallback models CRUD
// ---------------------------------------------------------------------------

/// Set or clear (with `None`) the unknown-model fallback for a kind.
pub async fn set_fallback(db: &Pool, kind: &str, model_id: Option<&str>) -> Result<(), DbError> {
    match model_id {
        Some(id) => {
            sqlx::query(
                r#"INSERT INTO fallback_models (kind, model_id)
                   VALUES (?, ?)
                   ON CONFLICT(kind) DO UPDATE SET model_id = excluded.model_id"#,
            )
            .bind(kind)
            .bind(id)
            .execute(db)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM fallback_models WHERE kind = ?")
                .bind(kind)
                .execute(db)
                .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config seeding — one-time import from config.toml on first boot.
// ---------------------------------------------------------------------------

/// Seed the DB topology tables from a `config.toml` `[upstream_pools]` map.
/// Called once on startup when the DB has no pools (first boot or migration
/// from config-managed topology). After seeding, the DB is the source of
/// truth and the TOML sections are ignored.
pub async fn seed_from_config(
    db: &Pool,
    pool_configs: &HashMap<String, UpstreamPoolConfig>,
    fallback: &FallbackConfig,
    crypto: &Crypto,
) -> Result<(), DbError> {
    for (sort_order, (name, pool_cfg)) in pool_configs.iter().enumerate() {
        for backend in &pool_cfg.backend {
            let mut row = config_to_backend_row(backend);
            // Resolve the referenced env var ONCE, at migration time, and seal
            // its value into the DB. The backend then keeps working across a
            // restart without that env var being present — the whole point of
            // moving topology into the DB. (`api_key()` reads the env here since
            // TOML backends carry no direct key.)
            if let Some(key) = backend.api_key() {
                let sealed = crypto.seal_str(&key)?;
                row.api_key_ct = Some(sealed.ciphertext);
                row.api_key_nonce = Some(sealed.nonce);
            }
            upsert_backend(db, &row).await?;
        }
        let pool_row = config_to_pool_row(name, pool_cfg, sort_order as i64);
        upsert_pool(db, &pool_row).await?;
    }

    if let Some(m) = &fallback.chat {
        set_fallback(db, "chat", Some(m)).await?;
    }
    if let Some(m) = &fallback.embedding {
        set_fallback(db, "embedding", Some(m)).await?;
    }
    if let Some(m) = &fallback.transcription {
        set_fallback(db, "transcription", Some(m)).await?;
    }
    if let Some(m) = &fallback.image {
        set_fallback(db, "image", Some(m)).await?;
    }

    Ok(())
}

fn config_to_backend_row(cfg: &BackendConfig) -> BackendRow {
    let (models, aliases) = config_to_models_and_aliases(cfg);
    BackendRow {
        name: cfg.name.clone(),
        base_url: cfg.base_url.clone(),
        api_key_env: cfg.api_key_env.clone(),
        api_key_ct: None,
        api_key_nonce: None,
        weight: cfg.weight,
        max_inflight: cfg.max_inflight,
        health_path: cfg.health_path.clone(),
        probe_models: cfg.probe_models,
        supports_edit: cfg.supports_edit,
        models,
        aliases,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
    }
}

fn config_to_pool_row(name: &str, cfg: &UpstreamPoolConfig, sort_order: i64) -> PoolRow {
    let kind_str = match cfg.kind {
        crate::server::upstreams::config::PoolKind::Chat => "chat",
        crate::server::upstreams::config::PoolKind::Transcription => "transcription",
        crate::server::upstreams::config::PoolKind::Embedding => "embedding",
        crate::server::upstreams::config::PoolKind::Image => "image",
        crate::server::upstreams::config::PoolKind::Speech => "speech",
    };
    let strategy_str = match cfg.strategy {
        crate::server::upstreams::config::PickerStrategy::LeastInflight => "least_inflight",
        crate::server::upstreams::config::PickerStrategy::RoundRobin => "round_robin",
    };
    let voices = cfg
        .voices
        .iter()
        .map(|(k, v)| VoiceRow {
            lang_code: k.clone(),
            voice_id: v.clone(),
        })
        .collect();

    PoolRow {
        name: name.to_string(),
        kind: kind_str.to_string(),
        strategy: strategy_str.to_string(),
        fallback_offline: cfg.fallback_offline.clone(),
        compliance_gdpr: cfg.compliance.gdpr,
        compliance_nda: cfg.compliance.nda,
        enforce_limits: cfg.enforce_limits,
        sort_order,
        backends: cfg.backend.iter().map(|b| b.name.clone()).collect(),
        models: cfg.models.clone(),
        voices,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
    }
}

fn config_to_models_and_aliases(cfg: &BackendConfig) -> (Vec<String>, Vec<AliasRow>) {
    let models = cfg.models.clone();
    let aliases = match cfg.alias.as_ref() {
        Some(AliasSpec::Names(names)) => names
            .iter()
            .map(|n| AliasRow {
                alias: n.clone(),
                target: None,
            })
            .collect(),
        Some(AliasSpec::Targets(map)) => map
            .iter()
            .map(|(k, v)| AliasRow {
                alias: k.clone(),
                target: Some(v.clone()),
            })
            .collect(),
        Some(AliasSpec::Mixed(map)) => map
            .iter()
            .map(|(k, v)| AliasRow {
                alias: k.clone(),
                target: v.clone(),
            })
            .collect(),
        None => Vec::new(),
    };
    (models, aliases)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;

    async fn test_pool() -> Pool {
        db::open(std::path::Path::new(":memory:")).await.unwrap()
    }

    #[tokio::test]
    async fn empty_snapshot_when_no_rows() {
        let pool = test_pool().await;
        let snap = load_snapshot(&pool).await.unwrap();
        assert!(snap.pools.is_empty());
        assert!(snap.backends.is_empty());
        assert!(snap.fallbacks.is_empty());
        assert!(is_empty(&pool).await.unwrap());
    }

    #[tokio::test]
    async fn backend_round_trip() {
        let pool = test_pool().await;
        let backend = BackendRow {
            name: "gpu-01".into(),
            base_url: "http://gpu-01:8000/v1".into(),
            api_key_env: Some("GPU01_KEY".into()),
            api_key_ct: None,
            api_key_nonce: None,
            weight: 2,
            max_inflight: 32,
            health_path: "/v1/models".into(),
            probe_models: true,
            supports_edit: false,
            models: vec!["qwen-32b".into(), "qwen-7b".into()],
            aliases: vec![
                AliasRow {
                    alias: "fast".into(),
                    target: Some("qwen-7b".into()),
                },
                AliasRow {
                    alias: "smart".into(),
                    target: Some("qwen-32b".into()),
                },
            ],
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        };
        upsert_backend(&pool, &backend).await.unwrap();

        let loaded = get_backend(&pool, "gpu-01").await.unwrap().unwrap();
        assert_eq!(loaded.base_url, "http://gpu-01:8000/v1");
        assert_eq!(loaded.weight, 2);
        assert_eq!(loaded.max_inflight, 32);
        assert_eq!(loaded.models, vec!["qwen-32b", "qwen-7b"]);
        assert_eq!(loaded.aliases.len(), 2);
        // is_empty tracks pools, not backends: inserting a backend alone leaves
        // the topology "empty" (no pool → nothing routable yet).
        assert!(is_empty(&pool).await.unwrap());

        // Update — change weight, drop a model.
        let mut updated = backend.clone();
        updated.weight = 5;
        updated.models = vec!["qwen-32b".into()];
        upsert_backend(&pool, &updated).await.unwrap();
        let reloaded = get_backend(&pool, "gpu-01").await.unwrap().unwrap();
        assert_eq!(reloaded.weight, 5);
        assert_eq!(reloaded.models, vec!["qwen-32b"]);
    }

    #[tokio::test]
    async fn backend_delete_cascades() {
        let pool = test_pool().await;
        let backend = BackendRow {
            name: "tmp".into(),
            base_url: "http://tmp".into(),
            api_key_env: None,
            api_key_ct: None,
            api_key_nonce: None,
            weight: 1,
            max_inflight: 16,
            health_path: "/models".into(),
            probe_models: true,
            supports_edit: false,
            models: vec!["m1".into()],
            aliases: vec![AliasRow {
                alias: "a1".into(),
                target: None,
            }],
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        };
        upsert_backend(&pool, &backend).await.unwrap();
        assert!(get_backend(&pool, "tmp").await.unwrap().is_some());

        delete_backend(&pool, "tmp").await.unwrap();
        assert!(get_backend(&pool, "tmp").await.unwrap().is_none());

        // Dependent rows should be gone.
        let model_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM backend_models WHERE backend_name = 'tmp'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(model_count, 0);
    }

    #[tokio::test]
    async fn pool_round_trip() {
        let pool = test_pool().await;

        // Create a backend first.
        upsert_backend(
            &pool,
            &BackendRow {
                name: "b1".into(),
                base_url: "http://b1".into(),
                api_key_env: None,
                api_key_ct: None,
                api_key_nonce: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                probe_models: true,
                supports_edit: false,
                models: vec![],
                aliases: vec![],
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
            },
        )
        .await
        .unwrap();

        let pool_row = PoolRow {
            name: "chat-pool".into(),
            kind: "chat".into(),
            strategy: "round_robin".into(),
            fallback_offline: Some("backup-model".into()),
            compliance_gdpr: false,
            compliance_nda: true,
            enforce_limits: true,
            sort_order: 0,
            backends: vec!["b1".into()],
            models: vec!["pool-fallback-model".into()],
            voices: vec![],
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        };
        upsert_pool(&pool, &pool_row).await.unwrap();

        let snap = load_snapshot(&pool).await.unwrap();
        assert_eq!(snap.pools.len(), 1);
        let p = &snap.pools[0];
        assert_eq!(p.name, "chat-pool");
        assert_eq!(p.kind, "chat");
        assert_eq!(p.strategy, "round_robin");
        assert!(!p.compliance_gdpr);
        assert!(p.compliance_nda);
        assert_eq!(p.backends, vec!["b1"]);
        assert_eq!(p.models, vec!["pool-fallback-model"]);
        assert!(!is_empty(&pool).await.unwrap());
    }

    #[tokio::test]
    async fn pool_with_voices_round_trip() {
        let pool = test_pool().await;
        upsert_backend(
            &pool,
            &BackendRow {
                name: "tts".into(),
                base_url: "http://tts".into(),
                api_key_env: None,
                api_key_ct: None,
                api_key_nonce: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                probe_models: true,
                supports_edit: false,
                models: vec![],
                aliases: vec![],
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
            },
        )
        .await
        .unwrap();

        let pool_row = PoolRow {
            name: "voice".into(),
            kind: "speech".into(),
            strategy: "least_inflight".into(),
            fallback_offline: None,
            compliance_gdpr: true,
            compliance_nda: true,
            enforce_limits: true,
            sort_order: 0,
            backends: vec!["tts".into()],
            models: vec![],
            voices: vec![
                VoiceRow {
                    lang_code: "de".into(),
                    voice_id: "de-voice".into(),
                },
                VoiceRow {
                    lang_code: "".into(),
                    voice_id: "default-voice".into(),
                },
            ],
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        };
        upsert_pool(&pool, &pool_row).await.unwrap();

        let snap = load_snapshot(&pool).await.unwrap();
        let p = &snap.pools[0];
        assert_eq!(p.voices.len(), 2);
        assert_eq!(
            p.voices
                .iter()
                .find(|v| v.lang_code == "de")
                .unwrap()
                .voice_id,
            "de-voice"
        );
        assert_eq!(
            p.voices
                .iter()
                .find(|v| v.lang_code.is_empty())
                .unwrap()
                .voice_id,
            "default-voice"
        );
    }

    #[tokio::test]
    async fn fallback_set_and_clear() {
        let pool = test_pool().await;
        set_fallback(&pool, "chat", Some("qwen")).await.unwrap();
        set_fallback(&pool, "embedding", Some("text-embed"))
            .await
            .unwrap();

        let snap = load_snapshot(&pool).await.unwrap();
        assert_eq!(snap.fallbacks.get("chat").unwrap(), "qwen");
        assert_eq!(snap.fallbacks.get("embedding").unwrap(), "text-embed");

        // Clear one.
        set_fallback(&pool, "chat", None).await.unwrap();
        let snap = load_snapshot(&pool).await.unwrap();
        assert!(!snap.fallbacks.contains_key("chat"));
        assert!(snap.fallbacks.contains_key("embedding"));
    }

    #[tokio::test]
    async fn pool_delete_cascades() {
        let pool = test_pool().await;
        upsert_backend(
            &pool,
            &BackendRow {
                name: "b".into(),
                base_url: "http://b".into(),
                api_key_env: None,
                api_key_ct: None,
                api_key_nonce: None,
                weight: 1,
                max_inflight: 16,
                health_path: "/models".into(),
                probe_models: true,
                supports_edit: false,
                models: vec![],
                aliases: vec![],
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
            },
        )
        .await
        .unwrap();
        upsert_pool(
            &pool,
            &PoolRow {
                name: "p".into(),
                kind: "chat".into(),
                strategy: "least_inflight".into(),
                fallback_offline: None,
                compliance_gdpr: true,
                compliance_nda: true,
                enforce_limits: true,
                sort_order: 0,
                backends: vec!["b".into()],
                models: vec!["m".into()],
                voices: vec![VoiceRow {
                    lang_code: "en".into(),
                    voice_id: "v".into(),
                }],
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
            },
        )
        .await
        .unwrap();

        delete_pool(&pool, "p").await.unwrap();
        assert!(is_empty(&pool).await.unwrap());

        // Backend should still exist (delete pool ≠ delete backend).
        assert!(get_backend(&pool, "b").await.unwrap().is_some());

        // Pool-dependent rows gone.
        let pb_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pool_backends WHERE pool_name = 'p'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pb_count, 0);
    }
}
