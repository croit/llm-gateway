// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Pool CRUD write handlers for the DB-backed upstream topology.
//!
//! A pool groups one or more [backends](super::backends) under a `kind`
//! (chat / transcription / embedding / image / speech) and a picker
//! `strategy`, plus per-pool compliance flags, an offline fallback model,
//! advertised models, and (for speech) a language→voice map. Edits are
//! written to the DB via [`gateway_core::server::db::upstreams_config`] but only
//! take effect on the runtime registry when the admin clicks "Apply changes"
//! (POST `/admin/upstreams/reload`).
//!
//! The page these handlers back is now [`super::upstreams`] (`/admin/upstreams`,
//! which merged the old `/admin/pools` + `/admin/backends`); this module keeps
//! only the POST handlers (their paths are unchanged) plus the voices textarea
//! parse/serialise helpers and the shared kind/strategy vocabularies.
//!
//! Every save/delete bumps the in-memory topology-dirty counter
//! ([`RamaState::topology_dirty_bump`]) so the apply bar can show how many
//! edits are pending; the response also patches the `topologyDirty` datastar
//! signal so the bar updates without a reload.
//!
//! Gated on the `admin` role via [`super::require_admin_or_403`], same as the
//! other operator pages.

use std::sync::Arc;

use rama::http::service::web::extract::State;
use rama::http::{Request, Response};

use super::{
    checkbox_on, dirty_signal, field, fields_all, parse_csv, read_form, require_admin_or_403, toast,
};
use session_core::chrome::{FlashKind, sse_response, sse_toast};
use session_core::i18n::{self, Lang, t, t_args};

use gateway_core::server::db::upstreams_config::{self, PoolRow, VoiceRow};
use gateway_runtime::rama_server::state::RamaState;

/// Pool kinds, matching the `snake_case` the `db_bridge` parser recognises.
/// Shared with the upstreams page's kind `<select>`.
pub(super) const KINDS: &[&str] = &[
    "chat",
    "transcription",
    "embedding",
    "image",
    "speech",
    "ocr",
];
/// Picker strategies, matching the `db_bridge` parser.
pub(super) const STRATEGIES: &[&str] = &["least_inflight", "round_robin"];
/// Kinds that support an unknown-model fallback (speech deliberately has none —
/// a mistyped voice/model just surfaces the backend's own error).
pub(super) const FALLBACK_KINDS: &[&str] = &["chat", "transcription", "embedding", "image"];

/// Serialise a pool's voices back into the textarea format (`lang=voice` per
/// line; a bare `voice` for the empty-key default). Used by the upstreams
/// editor to pre-fill the voices textarea.
pub(super) fn voice_lines(voices: &[VoiceRow]) -> String {
    voices
        .iter()
        .map(|v| {
            if v.lang_code.is_empty() {
                v.voice_id.clone()
            } else {
                format!("{}={}", v.lang_code, v.voice_id)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the voices textarea: one `lang=voice` per line; a line with no `=` is
/// the empty-key default voice.
fn parse_voices(v: &str) -> Vec<VoiceRow> {
    v.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            match line.split_once('=') {
                Some((lang, voice)) => Some(VoiceRow {
                    lang_code: lang.trim().to_string(),
                    voice_id: voice.trim().to_string(),
                }),
                None => Some(VoiceRow {
                    lang_code: String::new(),
                    voice_id: line.to_string(),
                }),
            }
        })
        .collect()
}

/// Re-read the pool list and build the `datastar-patch-elements` event that
/// refreshes the Add-backend form's Pool `<select>` in place, so a just-saved
/// or just-deleted pool shows up (or disappears) there without a page reload —
/// the select is rendered once at page load and would otherwise keep its stale
/// options. Pool order matches the page (`ORDER BY sort_order, name`, which the
/// snapshot query already applies). Best-effort: a load error skips the patch
/// (the toast + dirty signal still fire).
async fn pool_select_patch(state: &RamaState, lang: Lang) -> Option<rama::bytes::Bytes> {
    let snapshot = upstreams_config::load_snapshot(&state.db).await.ok()?;
    let names: Vec<String> = snapshot.pools.iter().map(|p| p.name.clone()).collect();
    Some(super::upstreams::add_backend_pool_select_patch(
        lang, &names,
    ))
}

// ---------------------------------------------------------------------------
// Handlers (POST) — write the DB topology; the registry picks changes up on
// POST /admin/upstreams/reload ("Apply changes").
// ---------------------------------------------------------------------------

/// POST /admin/pools/save — insert or update a pool from the editor form.
pub async fn pools_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let pairs: Vec<(String, String)> = match read_form(body).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let name = field(&pairs, "name").trim();
    if name.is_empty() {
        return toast(FlashKind::Error, t(lang, "pools-error-name-required"));
    }
    let kind = field(&pairs, "kind").trim();
    if !KINDS.contains(&kind) {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "pools-error-invalid-kind",
                &i18n::args([("kind", kind.to_string().into())]),
            ),
        );
    }
    let strategy = {
        let s = field(&pairs, "strategy").trim();
        if STRATEGIES.contains(&s) {
            s
        } else {
            "least_inflight"
        }
    };
    let fallback_offline = {
        let v = field(&pairs, "fallback_offline").trim();
        (!v.is_empty()).then(|| v.to_string())
    };
    let sort_order: i64 = field(&pairs, "sort_order").trim().parse().unwrap_or(0);

    let row = PoolRow {
        name: name.to_string(),
        kind: kind.to_string(),
        strategy: strategy.to_string(),
        fallback_offline,
        compliance_gdpr: checkbox_on(field(&pairs, "compliance_gdpr")),
        compliance_nda: checkbox_on(field(&pairs, "compliance_nda")),
        enforce_limits: checkbox_on(field(&pairs, "enforce_limits")),
        sort_order,
        // Gateway groups allowed to see + route to this pool (comma-separated
        // group names; empty = unrestricted). See `db::gateway_groups`.
        allowed_groups: parse_csv(field(&pairs, "allowed_groups")),
        backends: fields_all(&pairs, "backends"),
        models: parse_csv(field(&pairs, "models")),
        voices: parse_voices(field(&pairs, "voices")),
        created_at: jiff::Timestamp::now(),
        updated_at: jiff::Timestamp::now(),
    };
    match upstreams_config::upsert_pool(&state.db, &row).await {
        Ok(()) => {
            let dirty = state.topology_dirty_bump();
            let mut events = vec![
                sse_toast(&session_core::chrome::Flash {
                    kind: FlashKind::Success,
                    message: t_args(
                        lang,
                        "pools-saved",
                        &i18n::args([("name", name.to_string().into())]),
                    ),
                }),
                dirty_signal(dirty),
            ];
            // Refresh the Add-backend Pool select so this pool is immediately
            // selectable there (no reload / no "Apply changes" needed).
            events.extend(pool_select_patch(&state, lang).await);
            sse_response(&events)
        }
        Err(e) => toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-upsert-error",
                &i18n::args([("err", e.to_string().into())]),
            ),
        ),
    }
}

/// POST /admin/pools/delete — remove a pool and its dependent rows (backends
/// stay; deleting a pool never deletes a backend).
pub async fn pools_delete(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let pairs: Vec<(String, String)> = match read_form(body).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let name = field(&pairs, "name").trim();
    match upstreams_config::delete_pool(&state.db, name).await {
        Ok(()) => {
            let dirty = state.topology_dirty_bump();
            let mut events = vec![
                sse_toast(&session_core::chrome::Flash {
                    kind: FlashKind::Success,
                    message: t_args(
                        lang,
                        "pools-deleted",
                        &i18n::args([("name", name.to_string().into())]),
                    ),
                }),
                dirty_signal(dirty),
            ];
            // Drop the deleted pool from the Add-backend Pool select in place.
            events.extend(pool_select_patch(&state, lang).await);
            sse_response(&events)
        }
        Err(e) => toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-error",
                &i18n::args([("err", e.to_string().into())]),
            ),
        ),
    }
}

/// POST /admin/pools/fallback — set or clear the unknown-model fallback for a
/// kind. An empty `model` clears it. Fallbacks are resolved live from the DB on
/// every miss, so this needs no "Apply changes" and does not bump the dirty
/// counter.
pub async fn pools_fallback_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let pairs: Vec<(String, String)> = match read_form(body).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let kind = field(&pairs, "kind").trim();
    if !FALLBACK_KINDS.contains(&kind) {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "pools-error-invalid-kind",
                &i18n::args([("kind", kind.to_string().into())]),
            ),
        );
    }
    let model = {
        let m = field(&pairs, "model").trim();
        (!m.is_empty()).then_some(m)
    };
    match upstreams_config::set_fallback(&state.db, kind, model).await {
        Ok(()) => {
            let msg = match model {
                Some(m) => t_args(
                    lang,
                    "pools-fallback-saved",
                    &i18n::args([
                        ("kind", kind.to_string().into()),
                        ("model", m.to_string().into()),
                    ]),
                ),
                None => t_args(
                    lang,
                    "pools-fallback-cleared",
                    &i18n::args([("kind", kind.to_string().into())]),
                ),
            };
            toast(FlashKind::Success, msg)
        }
        Err(e) => toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-error",
                &i18n::args([("err", e.to_string().into())]),
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Voices round-trip through parse → serialise: `lang=voice`, and a bare
    /// line becomes the empty-key default voice.
    #[test]
    fn voices_round_trip() {
        let parsed = parse_voices("de=de-voice\ndefault-voice\nen = en-voice ");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].lang_code, "de");
        assert_eq!(parsed[0].voice_id, "de-voice");
        assert!(parsed[1].lang_code.is_empty());
        assert_eq!(parsed[1].voice_id, "default-voice");
        assert_eq!(parsed[2].lang_code, "en");
        assert_eq!(parsed[2].voice_id, "en-voice");
        assert_eq!(
            voice_lines(&parsed),
            "de=de-voice\ndefault-voice\nen=en-voice"
        );
    }
}
