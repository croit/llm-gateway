// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Operator-configurable per-feature default models.
//!
//! Each feature (chat, voice/transcription, image generation) pre-selects
//! one model. Historically that was just the alphabetically-first model the
//! pool advertised; this module lets an operator override it from
//! `/admin/models`, persisting the choice in the [`app_settings`] KV table.
//!
//! The chosen id is always *resolved against the live advertised set* before
//! use: a setting that is absent, empty, or names a model no longer being
//! served falls back to the first advertised model — exactly the pre-existing
//! behaviour — so defaults degrade gracefully across redeploys and backend
//! changes. This mirrors `pages::feedback::resolve_model`.
//!
//! [`app_settings`]: crate::server::db::app_settings

use crate::server::db::{Pool, app_settings};
use crate::server::upstreams::PoolKind;

/// The features that carry a configurable default model. The wire name (used
/// in the admin form and the `app_settings` key) is stable; adding a variant
/// is the only change needed to expose a new feature's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Chat,
    Transcription,
    Image,
    /// The embedding model pre-selected in the RAG collection form. Unlike the
    /// other features this is *only* a UI pre-fill: the model is committed per
    /// collection and never used as an implicit fallback, so it can't silently
    /// mix incompatible vectors into an existing index.
    Embedding,
}

impl Feature {
    /// Parse the wire name posted by the admin form. Unknown names return
    /// `None` so the handler can reject them.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "chat" => Some(Self::Chat),
            "transcription" => Some(Self::Transcription),
            "image" => Some(Self::Image),
            "embedding" => Some(Self::Embedding),
            _ => None,
        }
    }

    /// Stable wire name (admin form field + persisted key suffix).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Transcription => "transcription",
            Self::Image => "image",
            Self::Embedding => "embedding",
        }
    }

    /// The pool whose models this feature picks from.
    pub fn pool_kind(self) -> PoolKind {
        match self {
            Self::Chat => PoolKind::Chat,
            Self::Transcription => PoolKind::Transcription,
            Self::Image => PoolKind::Image,
            Self::Embedding => PoolKind::Embedding,
        }
    }

    /// The `app_settings` key the default is stored under.
    fn key(self) -> &'static str {
        match self {
            Self::Chat => "default_model.chat",
            Self::Transcription => "default_model.transcription",
            Self::Image => "default_model.image",
            Self::Embedding => "default_model.embedding",
        }
    }
}

/// The raw configured default for a feature, or `None` if unset. Empty stored
/// values are treated as unset. Does *not* check the id is still served — use
/// [`resolve`] or [`promote`] for that.
pub async fn get(pool: &Pool, feature: Feature) -> Option<String> {
    match app_settings::get(pool, feature.key()).await {
        Ok(Some(v)) if !v.trim().is_empty() => Some(v),
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(error = %err, feature = feature.as_str(), "feature_defaults: get failed");
            None
        }
    }
}

/// Persist (`Some`) or clear (`None`) a feature's default model.
pub async fn set(
    pool: &Pool,
    feature: Feature,
    model: Option<&str>,
) -> Result<(), crate::server::db::DbError> {
    match model {
        Some(m) if !m.trim().is_empty() => app_settings::set(pool, feature.key(), m.trim()).await,
        _ => app_settings::delete(pool, feature.key()).await,
    }
}

/// Resolve a configured id against the live advertised set: honour it if it's
/// actually being served, else fall back to the first advertised model (or
/// `None` when the pool is empty). The single source of truth for "which model
/// is the default for this feature right now".
pub fn resolve(configured: Option<&str>, available: &[String]) -> Option<String> {
    configured
        .filter(|m| !m.is_empty() && available.iter().any(|a| a == m))
        .map(str::to_string)
        .or_else(|| available.first().cloned())
}

/// Move the resolved default to the front of `items` in place, so callers that
/// treat "first entry" as "the default" (the chat/voice pickers, which have no
/// separate `selected` state) pre-select the operator's choice. `id_of` reads
/// the model id from each item, so this works for both bare id lists and richer
/// option structs. A configured-but-unavailable id is a no-op (the list keeps
/// its existing first entry).
pub fn promote<T>(configured: Option<&str>, items: &mut [T], id_of: impl Fn(&T) -> &str) {
    let Some(id) = configured.filter(|m| !m.is_empty()) else {
        return;
    };
    if let Some(pos) = items.iter().position(|it| id_of(it) == id) {
        items[..=pos].rotate_right(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn feature_wire_names_round_trip() {
        for f in [
            Feature::Chat,
            Feature::Transcription,
            Feature::Image,
            Feature::Embedding,
        ] {
            assert_eq!(Feature::from_wire(f.as_str()), Some(f));
        }
        assert_eq!(Feature::from_wire("bogus"), None);
    }

    #[test]
    fn resolve_honours_available_configured() {
        let avail = v(&["a", "b", "c"]);
        assert_eq!(resolve(Some("b"), &avail).as_deref(), Some("b"));
    }

    #[test]
    fn resolve_falls_back_when_configured_absent() {
        let avail = v(&["a", "b"]);
        assert_eq!(resolve(Some("gone"), &avail).as_deref(), Some("a"));
        assert_eq!(resolve(Some(""), &avail).as_deref(), Some("a"));
        assert_eq!(resolve(None, &avail).as_deref(), Some("a"));
    }

    #[test]
    fn resolve_none_on_empty_pool() {
        assert_eq!(resolve(Some("x"), &[]), None);
        assert_eq!(resolve(None, &[]), None);
    }

    #[test]
    fn promote_moves_configured_to_front_preserving_rest_order() {
        let mut items = v(&["a", "b", "c", "d"]);
        promote(Some("c"), &mut items, |s| s.as_str());
        assert_eq!(items, v(&["c", "a", "b", "d"]));
    }

    #[test]
    fn promote_is_noop_when_absent_or_unset() {
        let mut items = v(&["a", "b"]);
        promote(Some("gone"), &mut items, |s| s.as_str());
        assert_eq!(items, v(&["a", "b"]));
        promote(None, &mut items, |s| s.as_str());
        assert_eq!(items, v(&["a", "b"]));
    }
}
