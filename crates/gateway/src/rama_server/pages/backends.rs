// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Backend CRUD write handlers for the DB-backed upstream topology.
//!
//! A backend is a single OpenAI-compatible upstream (base URL, sealed API key,
//! weight, in-flight cap, health path, and a static model/alias list for
//! backends that don't expose `/models`). Edits are written to the DB via
//! [`crate::server::db::upstreams_config`] but only take effect on the runtime
//! registry when the admin clicks "Apply changes" (POST
//! `/admin/upstreams/reload`).
//!
//! The page these handlers back is now [`super::upstreams`] (`/admin/upstreams`,
//! which merged the old `/admin/pools` + `/admin/backends`); this module keeps
//! only the POST handlers (their paths are unchanged) plus the aliases textarea
//! parse/serialise helpers and the runtime-health sparkline the upstreams page
//! renders.
//!
//! Every save/delete bumps the in-memory topology-dirty counter
//! ([`RamaState::topology_dirty_bump`]) and patches the `topologyDirty` datastar
//! signal so the apply bar updates without a reload.
//!
//! Gated on the `admin` role via [`super::require_admin_or_403`], same as the
//! other operator pages.

use std::sync::Arc;

use rama::http::service::web::extract::State;
use rama::http::{Request, Response};

use super::{
    checkbox_on, dirty_signal, field, parse_csv, parse_u32, read_form, require_admin_or_403, toast,
};
use session_core::chrome::{FlashKind, sse_response, sse_toast};
use session_core::i18n::{self, Lang, t, t_args};

use crate::rama_server::state::RamaState;
use crate::server::db::upstreams_config::{self, AliasRow, BackendRow};

/// Serialise a backend's aliases back into the textarea format
/// (`name=target` per line, or a bare `name` for a target-less alias). Used by
/// the upstreams editor to pre-fill the aliases textarea.
pub(super) fn alias_lines(aliases: &[AliasRow]) -> String {
    aliases
        .iter()
        .map(|a| match &a.target {
            Some(t) => format!("{}={t}", a.alias),
            None => a.alias.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the aliases textarea: one `name=target` per line, or a bare `name`
/// for a target-less alias (binds to the backend's sole model at request time).
fn parse_aliases(v: &str) -> Vec<AliasRow> {
    v.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            match line.split_once('=') {
                Some((alias, target)) => Some(AliasRow {
                    alias: alias.trim().to_string(),
                    target: Some(target.trim().to_string()),
                }),
                None => Some(AliasRow {
                    alias: line.to_string(),
                    target: None,
                }),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Backend CRUD handlers (POST) — write to the DB topology; the registry only
// picks the change up on POST /admin/upstreams/reload ("Apply changes").
// ---------------------------------------------------------------------------

/// POST /admin/backends/save — insert or update a backend from the editor form.
/// A blank `weight`/`max_inflight` falls back to the config defaults (1 / 16),
/// a blank `health_path` to `/models`. Reminds the admin to click "Apply
/// changes" since the registry isn't reloaded here.
pub async fn backends_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
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
        return toast(FlashKind::Error, t(lang, "backends-error-name-required"));
    }
    let base_url = field(&pairs, "base_url").trim();
    if base_url.is_empty() {
        return toast(
            FlashKind::Error,
            t(lang, "backends-error-base-url-required"),
        );
    }
    let health_path = {
        let h = field(&pairs, "health_path").trim();
        if h.is_empty() {
            "/models".to_string()
        } else {
            h.to_string()
        }
    };
    let api_key_env = {
        let v = field(&pairs, "api_key_env").trim();
        (!v.is_empty()).then(|| v.to_string())
    };
    // API key value: a freshly entered key is sealed and stored; a blank field
    // on an existing backend keeps the current key (the form never echoes the
    // secret back), and a blank field on a new backend means "no stored key"
    // (the `api_key_env` fallback still applies). This is what lets an operator
    // add a backend with its key at runtime, no restart / new env var needed.
    let (api_key_ct, api_key_nonce) = {
        let entered = field(&pairs, "api_key").trim();
        if !entered.is_empty() {
            match state.crypto.seal_str(entered) {
                Ok(s) => (Some(s.ciphertext), Some(s.nonce)),
                Err(e) => {
                    return toast(
                        FlashKind::Error,
                        t_args(
                            lang,
                            "admin-db-upsert-error",
                            &i18n::args([("err", e.to_string().into())]),
                        ),
                    );
                }
            }
        } else {
            match upstreams_config::get_backend(&state.db, name).await {
                Ok(Some(existing)) => (existing.api_key_ct, existing.api_key_nonce),
                _ => (None, None),
            }
        }
    };
    let row = BackendRow {
        name: name.to_string(),
        base_url: base_url.to_string(),
        api_key_env,
        api_key_ct,
        api_key_nonce,
        weight: parse_u32(field(&pairs, "weight"), 1),
        max_inflight: parse_u32(field(&pairs, "max_inflight"), 16),
        health_path,
        probe_models: checkbox_on(field(&pairs, "probe_models")),
        supports_edit: checkbox_on(field(&pairs, "supports_edit")),
        models: parse_csv(field(&pairs, "models")),
        aliases: parse_aliases(field(&pairs, "aliases")),
        created_at: jiff::Timestamp::now(),
        updated_at: jiff::Timestamp::now(),
    };
    if let Err(e) = upstreams_config::upsert_backend(&state.db, &row).await {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-upsert-error",
                &i18n::args([("err", e.to_string().into())]),
            ),
        );
    }
    // Single "Pool" select: set this backend's membership to exactly the chosen
    // pool (empty = none). Only when the form actually carried the field, so a
    // stale form missing it can't silently unassign the backend.
    if pairs.iter().any(|(k, _)| k == "pool") {
        let pool = {
            let p = field(&pairs, "pool").trim();
            (!p.is_empty()).then_some(p)
        };
        if let Err(e) = upstreams_config::set_backend_pool(&state.db, name, pool).await {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-db-upsert-error",
                    &i18n::args([("err", e.to_string().into())]),
                ),
            );
        }
    }
    let dirty = state.topology_dirty_bump();
    sse_response(&[
        sse_toast(&session_core::chrome::Flash {
            kind: FlashKind::Success,
            message: t_args(
                lang,
                "backends-saved",
                &i18n::args([("name", name.to_string().into())]),
            ),
        }),
        dirty_signal(dirty),
    ])
}

/// POST /admin/backends/delete — remove a backend and its dependent rows.
pub async fn backends_delete(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let pairs: Vec<(String, String)> = match read_form(body).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let name = field(&pairs, "name");
    match upstreams_config::delete_backend(&state.db, name).await {
        Ok(()) => {
            let dirty = state.topology_dirty_bump();
            sse_response(&[
                sse_toast(&session_core::chrome::Flash {
                    kind: FlashKind::Success,
                    message: t_args(
                        lang,
                        "backends-deleted",
                        &i18n::args([("name", name.to_string().into())]),
                    ),
                }),
                dirty_signal(dirty),
            ])
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

    #[test]
    fn parse_csv_trims_and_drops_empties() {
        assert_eq!(parse_csv(" a, b ,,c , "), vec!["a", "b", "c"]);
        assert!(parse_csv("   ").is_empty());
    }

    #[test]
    fn checkbox_on_recognises_present_values() {
        assert!(checkbox_on("on"));
        assert!(checkbox_on(" true "));
        assert!(!checkbox_on(""));
        assert!(!checkbox_on("off"));
    }

    /// The aliases textarea round-trips through parse → serialise unchanged:
    /// `name=target` for explicit targets, a bare `name` for target-less ones.
    #[test]
    fn aliases_round_trip() {
        let parsed = parse_aliases("fast=qwen-7b\nbare\n\n  smart = qwen-32b ");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].alias, "fast");
        assert_eq!(parsed[0].target.as_deref(), Some("qwen-7b"));
        assert_eq!(parsed[1].alias, "bare");
        assert!(parsed[1].target.is_none());
        assert_eq!(parsed[2].target.as_deref(), Some("qwen-32b"));
        // Serialising back yields the canonical `name=target` / bare form.
        assert_eq!(alias_lines(&parsed), "fast=qwen-7b\nbare\nsmart=qwen-32b");
    }
}
