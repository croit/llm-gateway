// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! TOML configuration shape for the multi-provider routing layer.

use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamPoolConfig {
    pub kind: PoolKind,
    #[serde(default)]
    pub strategy: PickerStrategy,
    /// Backup model for this pool when a model it *knows* has no healthy
    /// backend right now (every replica down). On that `503`-shaped outage
    /// the router re-resolves the request to this model instead — typically a
    /// different tier (e.g. local GPUs down → a cloud model). Re-resolved
    /// through the normal path (so it may itself be an alias/group) and only a
    /// **single hop**: if the backup is also unavailable the original `503` is
    /// returned. Absent ⇒ the outage surfaces as `503`, as before. Distinct
    /// from [`crate::server::config::Config`]'s `[fallback]`, which handles a
    /// wholly *unknown* model name.
    #[serde(default)]
    pub fallback_offline: Option<String>,
    /// Pool-level fallback model IDs. Used to advertise/route a model when a
    /// backend in this pool doesn't report it via its `/models` probe (e.g.
    /// a Voxtral realtime server that has no `/models` endpoint). This is the
    /// lowest-priority source — see [`BackendConfig::models`] for the full
    /// precedence (probe → backend `models` → pool `models`).
    #[serde(default)]
    pub models: Vec<String>,
    /// Data-handling/compliance attributes for every model this pool serves.
    /// Absent block ⇒ [`Compliance::default`] (all-clear): no UI warning. Set
    /// a flag to `false` to surface a per-conversation warning banner in the
    /// chat UI — purely advisory signalling, no request-blocking. Lives at the
    /// pool level because residency/coverage is a property of the upstream
    /// endpoint, not the individual model id.
    #[serde(default)]
    pub compliance: Compliance,
    /// Language → voice name map, only meaningful for `kind = "speech"` pools.
    /// The voice-conversation flow picks the voice whose key matches the
    /// language the user spoke (from STT), falling back to the `""` (empty-key)
    /// entry as the default voice, then to no explicit voice at all. Keys are
    /// lowercase ISO-639-1 codes (`"de"`, `"en"`, …); values are backend voice
    /// ids. Empty ⇒ always send the backend/default voice.
    #[serde(default)]
    pub voices: HashMap<String, String>,
    pub backend: Vec<BackendConfig>,
}

impl UpstreamPoolConfig {
    /// Resolve the TTS voice for a spoken `language` (lowercase ISO-639-1).
    /// Exact match wins; then the default (`""`) entry; then `None` (let the
    /// backend use its own default voice). Pure so it's unit-tested.
    pub fn voice_for_language(&self, language: &str) -> Option<&str> {
        self.voices
            .get(language)
            .or_else(|| self.voices.get(""))
            .map(String::as_str)
    }
}

/// Per-pool data-handling attributes, surfaced to the user as chat-UI warnings
/// when a flag is `false`. Both default to `true` (compliant), so an existing
/// config with no `[upstream_pools.x.compliance]` block keeps today's
/// no-warning behaviour — you opt **in** to a warning by declaring `false`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Compliance {
    /// `false` ⇒ data sent here leaves the EU / has no GDPR safeguards. Warns
    /// the user not to enter personal data.
    #[serde(default = "default_true")]
    pub gdpr: bool,
    /// `false` ⇒ the endpoint is not covered by a confidentiality agreement.
    /// Warns the user not to send NDA-protected / proprietary material.
    #[serde(default = "default_true")]
    pub nda: bool,
}

impl Default for Compliance {
    fn default() -> Self {
        Self {
            gdpr: true,
            nda: true,
        }
    }
}

impl Compliance {
    /// True when every flag is clear — the common case, drawn with no warning.
    pub fn is_all_clear(&self) -> bool {
        self.gdpr && self.nda
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PoolKind {
    Chat,
    Transcription,
    Embedding,
    /// Image generation (OpenAI `/images/generations`-shaped). Routed like any
    /// other pool; backends that don't expose `/models` declare their model ids
    /// statically and set `probe_models = false` (see [`BackendConfig`]).
    Image,
    /// Text-to-speech. Backs `POST /v1/audio/speech` (OpenAI-shaped) and the
    /// session `POST /api/v0/speech` the voice-conversation UI calls. Dormant
    /// unless an operator configures a `kind = "speech"` pool — voice mode only
    /// appears when one exists, mirroring how transcription degrades.
    Speech,
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
    /// Client-facing aliases this backend also answers to. An alias decouples
    /// the name clients send from the real model that's loaded: point clients
    /// at `qwen`, swap the loaded model, keep the alias, and nothing downstream
    /// changes. The same alias on several backends forms a load-balanced
    /// **group**. Both forms combine across backends into one group.
    ///
    /// Two forms (pick one per backend):
    ///   - **list** — `alias = ["qwen", "fast"]`. Each name binds to the one
    ///     model this backend serves; use on single-model backends (the GPU norm).
    ///   - **map** — `alias = { smart = "glm-4.6" }`. Each name targets a
    ///     specific real id; required on multi-model backends (e.g. a cloud
    ///     provider serving many models behind one `base_url`), where a bare
    ///     name couldn't tell which model it means.
    ///
    /// See [`AliasSpec`] and `docs/upstreams.md`.
    #[serde(default)]
    pub alias: Option<AliasSpec>,
    /// Whether the health probe may *discover* this backend's model set from a
    /// `/models` response. Default `true` (the OpenAI-compat norm). Set `false`
    /// for a backend whose `/models` endpoint lists a *different* capability
    /// than this pool serves — notably an image backend (z.AI's general
    /// endpoint answers `/models` with its **chat** catalog): with `true` the
    /// probe would overwrite the configured image model ids and make them
    /// unroutable. When `false` the probe still tracks liveness but never
    /// touches the model set, so the configured [`models`](Self::models) /
    /// [`pool models`](UpstreamPoolConfig::models) stay authoritative.
    #[serde(default = "default_true")]
    pub probe_models: bool,
    /// Whether this backend supports image *editing* (image-to-image), not just
    /// text-to-image generation. Only meaningful on `kind = "image"` pools.
    /// Default `false` (text→image only, e.g. z.AI GLM-Image). A self-hosted
    /// edit-capable model (e.g. Qwen-Image-Edit) sets `true`, which is what
    /// lights up the `edit_image` tool / `/v1/images/edits` surface.
    #[serde(default)]
    pub supports_edit: bool,
}

/// How a backend's [`alias`](BackendConfig::alias) is written in TOML — either a
/// bare list of names (each binds to the backend's sole model) or a map from
/// alias name to the real model id it targets. Deserialised untagged: a TOML
/// array parses as [`AliasSpec::Names`], an inline table as
/// [`AliasSpec::Targets`]. (Untagged enums can't carry `deny_unknown_fields`.)
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum AliasSpec {
    Names(Vec<String>),
    Targets(HashMap<String, String>),
}

impl AliasSpec {
    /// Normalise to `alias name → optional explicit target`. A list entry maps
    /// to `None` (resolve to the backend's sole model at request time); a map
    /// entry maps to `Some(real_id)`.
    pub fn into_map(&self) -> HashMap<String, Option<String>> {
        match self {
            AliasSpec::Names(names) => names.iter().map(|n| (n.clone(), None)).collect(),
            AliasSpec::Targets(m) => m
                .iter()
                .map(|(k, v)| (k.clone(), Some(v.clone())))
                .collect(),
        }
    }
}

/// Unknown-model fallback, keyed by request kind. When a request names a model
/// that is neither a real id nor any alias, the router substitutes the model
/// configured here for that kind (re-resolved through the normal path, single
/// hop). Answers "the client asked for something we've never heard of" — a typo
/// or a renamed model. An unset kind ⇒ the miss surfaces as `404
/// model_not_found`, as before. Per-kind because a chat model can't sensibly
/// rescue an embeddings or transcription miss. Distinct from a pool's
/// [`fallback_offline`](UpstreamPoolConfig::fallback_offline), which handles a
/// *known* model whose backends are all down.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FallbackConfig {
    #[serde(default)]
    pub chat: Option<String>,
    #[serde(default)]
    pub embedding: Option<String>,
    #[serde(default)]
    pub transcription: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
}

impl FallbackConfig {
    /// The configured unknown-model fallback for `kind`, if any.
    pub fn for_kind(&self, kind: PoolKind) -> Option<&str> {
        match kind {
            PoolKind::Chat => self.chat.as_deref(),
            PoolKind::Embedding => self.embedding.as_deref(),
            PoolKind::Transcription => self.transcription.as_deref(),
            PoolKind::Image => self.image.as_deref(),
            // Speech has no unknown-model fallback: a mistyped voice/model just
            // surfaces the backend's own error. No sensible cross-substitution.
            PoolKind::Speech => None,
        }
    }
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
    fn parses_speech_pool_with_voices() {
        let s = r#"
            kind = "speech"

            [voices]
            de = "de-voice"
            en = "en-voice"
            "" = "fallback-voice"

            [[backend]]
            name = "tts"
            base_url = "http://tts:8000/v1"
            models = ["tts-1"]
        "#;
        let p: UpstreamPoolConfig = toml::from_str(s).unwrap();
        assert_eq!(p.kind, PoolKind::Speech);
        // Exact match wins.
        assert_eq!(p.voice_for_language("de"), Some("de-voice"));
        assert_eq!(p.voice_for_language("en"), Some("en-voice"));
        // Unknown language falls back to the "" default entry.
        assert_eq!(p.voice_for_language("fr"), Some("fallback-voice"));
    }

    #[test]
    fn voice_for_language_none_without_map() {
        let s = r#"
            kind = "speech"
            [[backend]]
            name = "tts"
            base_url = "http://tts:8000/v1"
        "#;
        let p: UpstreamPoolConfig = toml::from_str(s).unwrap();
        // No voices map + no default → let the backend pick its own voice.
        assert_eq!(p.voice_for_language("de"), None);
    }

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
    fn compliance_absent_defaults_to_all_clear() {
        let s = r#"
            kind = "chat"

            [[backend]]
            name = "x"
            base_url = "http://x"
        "#;
        let p: UpstreamPoolConfig = toml::from_str(s).unwrap();
        assert_eq!(p.compliance, Compliance::default());
        assert!(p.compliance.is_all_clear());
        assert!(p.compliance.gdpr && p.compliance.nda);
    }

    #[test]
    fn compliance_flags_parse_and_partial_block_keeps_other_true() {
        let s = r#"
            kind = "chat"

            [compliance]
            gdpr = false

            [[backend]]
            name = "zai"
            base_url = "https://api.z.ai/api/coding/paas/v4"
        "#;
        let p: UpstreamPoolConfig = toml::from_str(s).unwrap();
        assert!(!p.compliance.gdpr, "explicit gdpr=false must parse");
        assert!(p.compliance.nda, "unspecified nda must default true");
        assert!(!p.compliance.is_all_clear());
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

    #[test]
    fn alias_list_form_parses_and_normalises_to_bare_targets() {
        let s = r#"
            kind = "chat"
            fallback_offline = "glm-4.6"

            [[backend]]
            name = "gpu-a"
            base_url = "http://gpu-a:8000/v1"
            alias = ["qwen", "fast"]
        "#;
        let p: UpstreamPoolConfig = toml::from_str(s).unwrap();
        assert_eq!(p.fallback_offline.as_deref(), Some("glm-4.6"));
        let map = p.backend[0].alias.as_ref().unwrap().into_map();
        assert_eq!(map.get("qwen"), Some(&None));
        assert_eq!(map.get("fast"), Some(&None));
    }

    #[test]
    fn alias_map_form_parses_with_explicit_targets() {
        let s = r#"
            kind = "chat"

            [[backend]]
            name = "zai"
            base_url = "https://api.z.ai/v1"
            alias = { smart = "glm-4.6", cheap = "glm-4.5-air" }
        "#;
        let p: UpstreamPoolConfig = toml::from_str(s).unwrap();
        let map = p.backend[0].alias.as_ref().unwrap().into_map();
        assert_eq!(map.get("smart"), Some(&Some("glm-4.6".to_string())));
        assert_eq!(map.get("cheap"), Some(&Some("glm-4.5-air".to_string())));
    }

    #[test]
    fn alias_absent_and_fallback_offline_default_to_none() {
        let s = r#"
            kind = "chat"

            [[backend]]
            name = "x"
            base_url = "http://x"
        "#;
        let p: UpstreamPoolConfig = toml::from_str(s).unwrap();
        assert!(p.fallback_offline.is_none());
        assert!(p.backend[0].alias.is_none());
    }

    #[test]
    fn fallback_config_parses_per_kind_and_reports_by_kind() {
        let s = r#"
            chat = "qwen"
            embedding = "text-embedding-3-small"
        "#;
        let fb: FallbackConfig = toml::from_str(s).unwrap();
        assert_eq!(fb.for_kind(PoolKind::Chat), Some("qwen"));
        assert_eq!(
            fb.for_kind(PoolKind::Embedding),
            Some("text-embedding-3-small")
        );
        assert_eq!(fb.for_kind(PoolKind::Transcription), None);
    }
}
