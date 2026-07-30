// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Bridge between the DB topology snapshot and the config structs the
//! [`UpstreamRegistry`] builder expects.
//!
//! Converts [`crate::server::db::upstreams_config::UpstreamConfigSnapshot`]
//! into the same `HashMap<String, UpstreamPoolConfig>` + `FallbackConfig`
//! shapes that TOML parsing produces, so [`UpstreamRegistry::build`] is
//! reused without duplicating Pool/Backend construction logic.

use std::collections::HashMap;

use crate::server::crypto::Crypto;
use crate::server::db::upstreams_config::{BackendRow, UpstreamConfigSnapshot};
use crate::server::upstreams::config::{
    AliasSpec, BackendConfig, Compliance, FallbackConfig, PickerStrategy, PoolKind,
    UpstreamPoolConfig,
};

/// Convert a DB snapshot into the `(pool_configs, fallback)` pair the registry
/// builder consumes. Returns empty collections when the DB has no rows
/// (first boot, before seeding). `crypto` unseals each backend's stored API key.
pub fn snapshot_to_configs(
    snap: &UpstreamConfigSnapshot,
    crypto: &Crypto,
) -> (HashMap<String, UpstreamPoolConfig>, FallbackConfig) {
    let mut pool_configs: HashMap<String, UpstreamPoolConfig> = HashMap::new();

    for pool in &snap.pools {
        // An unrecognised kind must NOT be silently coerced to Chat — that would
        // route an intended embedding/image pool under the wrong kind. Skip the
        // pool (it becomes unroutable, surfacing as a clean model_not_found) and
        // log loudly so the misconfiguration is visible rather than mis-serving.
        let Some(kind) = parse_kind(&pool.kind) else {
            tracing::error!(
                pool = %pool.name, kind = %pool.kind,
                "unknown pool kind — skipping this pool (expected one of \
                 chat/transcription/embedding/image/speech)"
            );
            continue;
        };

        let backend_configs: Vec<BackendConfig> = pool
            .backends
            .iter()
            .filter_map(|name| snap.backends.get(name))
            .map(|row| backend_row_to_config(row, crypto))
            .collect();

        let voices: HashMap<String, String> = pool
            .voices
            .iter()
            .map(|v| (v.lang_code.clone(), v.voice_id.clone()))
            .collect();

        pool_configs.insert(
            pool.name.clone(),
            UpstreamPoolConfig {
                kind,
                strategy: parse_strategy(&pool.name, &pool.strategy),
                fallback_offline: pool.fallback_offline.clone(),
                models: pool.models.clone(),
                compliance: Compliance {
                    gdpr: pool.compliance_gdpr,
                    nda: pool.compliance_nda,
                },
                enforce_limits: pool.enforce_limits,
                voices,
                offer_voices: pool.offer_voices.clone(),
                allowed_groups: pool.allowed_groups.clone(),
                backend: backend_configs,
            },
        );
    }

    let fallback = FallbackConfig {
        chat: snap.fallbacks.get("chat").cloned(),
        embedding: snap.fallbacks.get("embedding").cloned(),
        transcription: snap.fallbacks.get("transcription").cloned(),
        image: snap.fallbacks.get("image").cloned(),
    };

    (pool_configs, fallback)
}

fn backend_row_to_config(row: &BackendRow, crypto: &Crypto) -> BackendConfig {
    // Each DB alias row carries its own optional target (bare or targeted), so
    // emit the per-alias `Mixed` form directly — it's the normalized shape the
    // registry builder consumes via `AliasSpec::into_map`. The `Names`/`Targets`
    // variants exist only for TOML's array/table syntax; picking the "tightest"
    // one here would be a distinction `into_map` immediately discards.
    let alias = (!row.aliases.is_empty()).then(|| {
        AliasSpec::Mixed(
            row.aliases
                .iter()
                .map(|a| (a.alias.clone(), a.target.clone()))
                .collect(),
        )
    });

    // Unseal the stored API key, if any. A decrypt failure (e.g. the encryption
    // key changed) is logged and treated as "no stored key" so routing degrades
    // to the `api_key_env` fallback rather than the whole reload failing.
    let api_key = match (&row.api_key_ct, &row.api_key_nonce) {
        (Some(ct), Some(nonce)) => match crypto.open_str(nonce, ct) {
            Ok(key) => Some(key),
            Err(e) => {
                tracing::warn!(backend = %row.name, error = %e, "decrypting backend API key");
                None
            }
        },
        _ => None,
    };

    BackendConfig {
        name: row.name.clone(),
        base_url: row.base_url.clone(),
        api_key_env: row.api_key_env.clone(),
        api_key,
        weight: row.weight,
        max_inflight: row.max_inflight,
        health_path: row.health_path.clone(),
        models: row.models.clone(),
        alias,
        probe_models: row.probe_models,
        supports_edit: row.supports_edit,
    }
}

/// Parse a pool `kind` string. Returns `None` for an unrecognised value so the
/// caller can skip the pool rather than mis-route it (see `snapshot_to_configs`).
fn parse_kind(s: &str) -> Option<PoolKind> {
    match s {
        "chat" => Some(PoolKind::Chat),
        "transcription" => Some(PoolKind::Transcription),
        "embedding" => Some(PoolKind::Embedding),
        "image" => Some(PoolKind::Image),
        "speech" => Some(PoolKind::Speech),
        "ocr" => Some(PoolKind::Ocr),
        _ => None,
    }
}

/// Parse a picker `strategy`. Unlike `kind`, an unknown strategy is harmless
/// (it only affects backend selection within a pool), so it warns and falls back
/// to the default rather than dropping the pool.
fn parse_strategy(pool: &str, s: &str) -> PickerStrategy {
    match s {
        "round_robin" => PickerStrategy::RoundRobin,
        "least_inflight" => PickerStrategy::LeastInflight,
        other => {
            tracing::warn!(
                pool = %pool, strategy = %other,
                "unknown picker strategy — defaulting to least_inflight"
            );
            PickerStrategy::LeastInflight
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::upstreams_config::{AliasRow, BackendRow, PoolRow, VoiceRow};
    use jiff::Timestamp;

    fn ts() -> Timestamp {
        Timestamp::now()
    }

    fn crypto() -> Crypto {
        Crypto::from_key([0u8; 32])
    }

    #[test]
    fn empty_snapshot_produces_empty_configs() {
        let snap = UpstreamConfigSnapshot::default();
        let (pools, fallback) = snapshot_to_configs(&snap, &crypto());
        assert!(pools.is_empty());
        assert_eq!(fallback, FallbackConfig::default());
    }

    #[test]
    fn full_snapshot_converts_correctly() {
        let mut snap = UpstreamConfigSnapshot::default();
        snap.backends.insert(
            "gpu-01".into(),
            BackendRow {
                name: "gpu-01".into(),
                base_url: "http://gpu-01:8000/v1".into(),
                api_key_env: Some("GPU_KEY".into()),
                api_key_ct: None,
                api_key_nonce: None,
                weight: 2,
                max_inflight: 32,
                health_path: "/models".into(),
                probe_models: true,
                supports_edit: false,
                models: vec!["qwen-32b".into()],
                aliases: vec![AliasRow {
                    alias: "fast".into(),
                    target: Some("qwen-32b".into()),
                }],
                created_at: ts(),
                updated_at: ts(),
            },
        );
        snap.pools.push(PoolRow {
            name: "chat".into(),
            kind: "chat".into(),
            strategy: "round_robin".into(),
            fallback_offline: Some("backup".into()),
            compliance_gdpr: false,
            compliance_nda: true,
            enforce_limits: true,
            sort_order: 0,
            allowed_groups: Vec::new(),
            backends: vec!["gpu-01".into()],
            models: vec!["pool-fb".into()],
            voices: vec![VoiceRow {
                lang_code: "de".into(),
                voice_id: "de-v".into(),
            }],
            offer_voices: Vec::new(),
            created_at: ts(),
            updated_at: ts(),
        });
        snap.fallbacks.insert("chat".into(), "default-chat".into());

        let (pools, fallback) = snapshot_to_configs(&snap, &crypto());
        assert_eq!(pools.len(), 1);
        let p = &pools["chat"];
        assert_eq!(p.kind, PoolKind::Chat);
        assert_eq!(p.strategy, PickerStrategy::RoundRobin);
        assert!(!p.compliance.gdpr);
        assert!(p.compliance.nda);
        assert_eq!(p.backend.len(), 1);
        assert_eq!(p.backend[0].name, "gpu-01");
        assert_eq!(p.backend[0].weight, 2);
        assert_eq!(p.backend[0].models, vec!["qwen-32b"]);
        assert_eq!(fallback.chat.as_deref(), Some("default-chat"));
    }

    #[test]
    fn bare_aliases_normalise_to_none_targets() {
        let mut snap = UpstreamConfigSnapshot::default();
        snap.backends.insert(
            "b".into(),
            BackendRow {
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
                aliases: vec![
                    AliasRow {
                        alias: "a1".into(),
                        target: None,
                    },
                    AliasRow {
                        alias: "a2".into(),
                        target: None,
                    },
                ],
                created_at: ts(),
                updated_at: ts(),
            },
        );
        snap.pools.push(PoolRow {
            name: "p".into(),
            kind: "chat".into(),
            strategy: "least_inflight".into(),
            fallback_offline: None,
            compliance_gdpr: true,
            compliance_nda: true,
            enforce_limits: true,
            sort_order: 0,
            allowed_groups: Vec::new(),
            backends: vec!["b".into()],
            models: vec![],
            voices: vec![],
            offer_voices: Vec::new(),
            created_at: ts(),
            updated_at: ts(),
        });

        let (pools, _) = snapshot_to_configs(&snap, &crypto());
        let alias = pools["p"].backend[0].alias.as_ref().unwrap();
        let map = alias.into_map();
        assert_eq!(map.get("a1"), Some(&None));
        assert_eq!(map.get("a2"), Some(&None));
    }

    fn snap_with_backend(kind: &str, aliases: Vec<AliasRow>) -> UpstreamConfigSnapshot {
        let mut snap = UpstreamConfigSnapshot::default();
        snap.backends.insert(
            "b".into(),
            BackendRow {
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
                models: vec!["m1".into()],
                aliases,
                created_at: ts(),
                updated_at: ts(),
            },
        );
        snap.pools.push(PoolRow {
            name: "p".into(),
            kind: kind.into(),
            strategy: "least_inflight".into(),
            fallback_offline: None,
            compliance_gdpr: true,
            compliance_nda: true,
            enforce_limits: true,
            sort_order: 0,
            allowed_groups: Vec::new(),
            backends: vec!["b".into()],
            models: vec![],
            voices: vec![],
            offer_voices: Vec::new(),
            created_at: ts(),
            updated_at: ts(),
        });
        snap
    }

    /// A backend mixing a bare alias with a targeted one must preserve each
    /// alias's own target — the bare one stays `None`, not a bogus `Some("")`.
    #[test]
    fn mixed_aliases_preserve_per_alias_targets() {
        let snap = snap_with_backend(
            "chat",
            vec![
                AliasRow {
                    alias: "fast".into(),
                    target: None,
                },
                AliasRow {
                    alias: "smart".into(),
                    target: Some("m1".into()),
                },
            ],
        );
        let (pools, _) = snapshot_to_configs(&snap, &crypto());
        let map = pools["p"].backend[0].alias.as_ref().unwrap().into_map();
        assert_eq!(map.get("fast"), Some(&None), "bare alias must stay bare");
        assert_eq!(map.get("smart"), Some(&Some("m1".to_string())));
    }

    /// An unrecognised pool kind is dropped (surfaces as model_not_found) rather
    /// than silently coerced to a Chat pool that mis-serves.
    #[test]
    fn unknown_kind_pool_is_skipped() {
        let snap = snap_with_backend("bogus-kind", vec![]);
        let (pools, _) = snapshot_to_configs(&snap, &crypto());
        assert!(
            pools.is_empty(),
            "pool with unknown kind must be skipped, got {pools:?}"
        );
    }

    /// A backend's sealed API key is unsealed into the resolved `BackendConfig`,
    /// so the DB is a sufficient source of the credential — no env var needed.
    #[test]
    fn sealed_api_key_unseals_into_config() {
        let c = crypto();
        let sealed = c.seal_str("sk-live-secret").unwrap();
        let mut snap = snap_with_backend("chat", vec![]);
        let b = snap.backends.get_mut("b").unwrap();
        b.api_key_ct = Some(sealed.ciphertext);
        b.api_key_nonce = Some(sealed.nonce);

        let (pools, _) = snapshot_to_configs(&snap, &c);
        assert_eq!(
            pools["p"].backend[0].api_key().as_deref(),
            Some("sk-live-secret"),
            "the stored key must decrypt and win over any env fallback"
        );
    }

    /// A key sealed under a different encryption key can't be opened; the
    /// backend falls back to no direct key rather than failing the whole reload.
    #[test]
    fn undecryptable_key_degrades_to_none() {
        let sealed = Crypto::from_key([1u8; 32]).seal_str("x").unwrap();
        let mut snap = snap_with_backend("chat", vec![]);
        let b = snap.backends.get_mut("b").unwrap();
        b.api_key_ct = Some(sealed.ciphertext);
        b.api_key_nonce = Some(sealed.nonce);
        b.api_key_env = None;

        let (pools, _) = snapshot_to_configs(&snap, &Crypto::from_key([2u8; 32]));
        assert!(pools["p"].backend[0].api_key().is_none());
    }
}
