// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! TOML configuration shape for the multi-provider routing layer.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamPoolConfig {
    pub kind: PoolKind,
    #[serde(default)]
    pub strategy: PickerStrategy,
    /// Pool-level fallback model IDs. Used to advertise/route a model when a
    /// backend in this pool doesn't report it via its `/models` probe (e.g.
    /// a Voxtral realtime server that has no `/models` endpoint). This is the
    /// lowest-priority source — see [`BackendConfig::models`] for the full
    /// precedence (probe → backend `models` → pool `models`).
    #[serde(default)]
    pub models: Vec<String>,
    pub backend: Vec<BackendConfig>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PoolKind {
    Chat,
    Transcription,
    Embedding,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PickerStrategy {
    RoundRobin,
    #[default]
    LeastInflight,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub name: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default = "default_max_inflight")]
    pub max_inflight: u32,
    /// Custom health probe path. Defaults to `/models` (every OpenAI-compat
    /// server exposes it). For backends that don't, e.g. plain whisper.cpp,
    /// override here.
    #[serde(default = "default_health_path")]
    pub health_path: String,
    /// Backend-level fallback model IDs, used when this backend's `/models`
    /// probe reports nothing (unparseable body, `401`, or no such endpoint).
    ///
    /// Model resolution precedence, highest first:
    ///   1. what the backend's `/models` probe reports (authoritative while
    ///      it returns *any* model);
    ///   2. this backend's `models` (more specific than the pool's);
    ///   3. the pool's [`UpstreamPoolConfig::models`].
    ///
    /// The first non-empty source wins — config is a fallback for backends
    /// that don't self-report, not a supplement to a live probe.
    #[serde(default)]
    pub models: Vec<String>,
}

fn default_weight() -> u32 {
    1
}
fn default_max_inflight() -> u32 {
    16
}
fn default_health_path() -> String {
    "/models".into()
}

impl BackendConfig {
    /// Reads `api_key_env`'s env var, if any. Returns `None` when the var is
    /// unset or empty.
    pub fn api_key(&self) -> Option<String> {
        self.api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|v| !v.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_pool() {
        let s = r#"
            kind = "chat"
            strategy = "round_robin"

            [[backend]]
            name = "gpu-01"
            base_url = "http://gpu-01:8000/v1"
            weight = 2
            max_inflight = 32

            [[backend]]
            name = "gpu-02"
            base_url = "http://gpu-02:8000/v1"
        "#;
        let p: UpstreamPoolConfig = toml::from_str(s).unwrap();
        assert_eq!(p.kind, PoolKind::Chat);
        assert_eq!(p.strategy, PickerStrategy::RoundRobin);
        assert_eq!(p.backend.len(), 2);
        assert_eq!(p.backend[0].weight, 2);
        assert_eq!(p.backend[0].max_inflight, 32);
        assert_eq!(p.backend[1].weight, 1);
        assert_eq!(p.backend[1].max_inflight, 16);
        assert_eq!(p.backend[0].health_path, "/models");
    }

    #[test]
    fn parses_pool_and_backend_model_fallbacks() {
        let s = r#"
            kind = "transcription"
            models = ["pool-fallback"]

            [[backend]]
            name = "voxtral"
            base_url = "http://voxtral:8000/v1"
            models = ["mistralai/Voxtral-Mini-4B-Realtime-2602"]

            [[backend]]
            name = "plain"
            base_url = "http://plain:8000/v1"
        "#;
        let p: UpstreamPoolConfig = toml::from_str(s).unwrap();
        assert_eq!(p.models, vec!["pool-fallback"]);
        assert_eq!(
            p.backend[0].models,
            vec!["mistralai/Voxtral-Mini-4B-Realtime-2602"]
        );
        // Backend without its own `models` parses to an empty list (the pool
        // fallback is applied later, in the registry).
        assert!(p.backend[1].models.is_empty());
    }

    #[test]
    fn model_fallbacks_default_to_empty() {
        let s = r#"
            kind = "chat"

            [[backend]]
            name = "x"
            base_url = "http://x"
        "#;
        let p: UpstreamPoolConfig = toml::from_str(s).unwrap();
        assert!(p.models.is_empty());
        assert!(p.backend[0].models.is_empty());
    }

    #[test]
    fn picker_strategy_defaults_to_least_inflight() {
        let s = r#"
            kind = "transcription"

            [[backend]]
            name = "x"
            base_url = "http://x"
        "#;
        let p: UpstreamPoolConfig = toml::from_str(s).unwrap();
        assert_eq!(p.strategy, PickerStrategy::LeastInflight);
    }
}
