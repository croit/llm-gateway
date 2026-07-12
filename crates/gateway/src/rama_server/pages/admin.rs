// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/*` pages. Currently just `/admin/models` for the
//! per-model sampling defaults — temperature, top_p, top_k,
//! min_p, repeat_penalty, frequency_penalty, presence_penalty,
//! max_tokens, stop tokens, etc. Each model gets a key=value TOML
//! textarea; the gateway parses it at save-time to reject
//! obviously-broken submissions and at request-time to merge
//! missing keys into the outgoing body. Client values always win.
//!
//! All routes are gated on the `admin` role via
//! [`super::require_admin_or_403`] — non-admins see a 403 page and
//! never the form. The sidebar entry is also conditional on that
//! role, so non-admins don't even know the page exists.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};

use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_admin_or_403, toast};
use session_core::chrome::{
    FlashKind, NavSections, Theme, is_datastar_request, read_body_to_bytes,
};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

use crate::rama_server::state::RamaState;
use crate::server::db::model_defaults as db;
use crate::server::feature_defaults::{self, Feature};
use crate::server::model_defaults as merge;
use crate::server::upstreams::PoolKind;

/// GET /admin/models — one card per chat model, each with the
/// stored TOML as a textarea + a Save button. Models with no row
/// yet render an empty textarea (operator picks defaults from
/// scratch).
pub async fn models_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    // Aliases carry no settings or price of their own — requests are configured
    // and metered as the model they resolve to (see `openai_driver`/cost
    // accounting). So split them out: real models get the full editor, aliases
    // get a read-only "→ target" row.
    let chat_models = state.upstreams.models_with_alias_target(PoolKind::Chat);
    let mut rows: Vec<ModelRow> = Vec::new();
    let mut aliases: Vec<AliasRow> = Vec::new();
    for (name, alias_target) in &chat_models {
        if let Some(target) = alias_target {
            aliases.push(AliasRow {
                name: name.clone(),
                target: target.clone(),
                kind_label: "chat",
            });
            continue;
        }
        let row = match db::get(&state.db, name).await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err, model = %name, "model_defaults: get failed");
                None
            }
        };
        rows.push(ModelRow {
            name: name.clone(),
            toml: row
                .as_ref()
                .map(|r| r.defaults_toml.clone())
                .unwrap_or_default(),
            reasoning_style: row
                .as_ref()
                .and_then(|r| r.reasoning_style.clone())
                .unwrap_or_default(),
            budget_standard: row.as_ref().and_then(|r| r.thinking_budget_standard),
            budget_deep: row.as_ref().and_then(|r| r.thinking_budget_deep),
            budget_max: row.as_ref().and_then(|r| r.thinking_budget_max),
            effort_standard: row
                .as_ref()
                .and_then(|r| r.reasoning_effort_standard.clone())
                .unwrap_or_default(),
            effort_deep: row
                .as_ref()
                .and_then(|r| r.reasoning_effort_deep.clone())
                .unwrap_or_default(),
            effort_max: row
                .as_ref()
                .and_then(|r| r.reasoning_effort_max.clone())
                .unwrap_or_default(),
            context_window: row.as_ref().and_then(|r| r.context_window),
            input_price: row.as_ref().and_then(|r| r.input_price),
            output_price: row.as_ref().and_then(|r| r.output_price),
            cap_vision: row.as_ref().and_then(|r| r.capabilities.vision),
            cap_tools: row.as_ref().and_then(|r| r.capabilities.tools),
            cap_structured_output: row.as_ref().and_then(|r| r.capabilities.structured_output),
            fallback_vision: row
                .as_ref()
                .and_then(|r| r.capabilities.fallback_vision.clone()),
            fallback_tools: row
                .as_ref()
                .and_then(|r| r.capabilities.fallback_tools.clone()),
        });
    }

    // Non-chat models (embedding / image / speech / transcription) get a
    // pricing-only row — sampling/reasoning/context don't apply, but cost
    // accounting does. Their aliases join the read-only alias list. Dedup by id;
    // a model already shown as chat is skipped.
    let chat_names: std::collections::HashSet<&str> =
        chat_models.iter().map(|(n, _)| n.as_str()).collect();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut other: Vec<OtherModelRow> = Vec::new();
    for (kind, kind_label) in [
        (PoolKind::Embedding, "embedding"),
        (PoolKind::Image, "image"),
        (PoolKind::Speech, "speech"),
        (PoolKind::Transcription, "transcription"),
    ] {
        for (name, alias_target) in state.upstreams.models_with_alias_target(kind) {
            if chat_names.contains(name.as_str()) || !seen.insert(name.clone()) {
                continue;
            }
            if let Some(target) = alias_target {
                aliases.push(AliasRow {
                    name,
                    target,
                    kind_label,
                });
                continue;
            }
            let row = db::get(&state.db, &name).await.ok().flatten();
            other.push(OtherModelRow {
                kind_label,
                input_price: row.as_ref().and_then(|r| r.input_price),
                output_price: row.as_ref().and_then(|r| r.output_price),
                name,
            });
        }
    }

    let defaults = defaults_rows(&state).await;
    let currency = &state.config.usage.currency;
    let all_models = state.upstreams.all_models();
    let body = render_models_body(
        lang,
        currency,
        &defaults,
        &rows,
        &aliases,
        &other,
        &all_models,
    );
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    let title = t(lang, "admin-page-title");
    nav_or_html_page(
        datastar,
        theme,
        lang,
        nav,
        NavItem::Admin,
        &title,
        &user.email,
        is_admin(&state, &user),
        session.impersonator_id.is_some(),
        body,
        "/admin/models",
        &chat,
    )
}

/// POST /admin/models — save the per-model defaults. Form body
/// carries both the `model_name` (as a hidden input — putting it
/// in the URL path doesn't survive rama's path lowercasing +
/// case-sensitive HuggingFace IDs) and the `defaults_toml`. An
/// empty `defaults_toml` clears the stored row.
pub async fn models_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: SaveForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-malformed-form",
                    &i18n::args([("err", err.to_string().into())]),
                ),
            );
        }
    };
    if form.model_name.is_empty() {
        return toast(FlashKind::Error, t(lang, "admin-missing-model-name"));
    }
    let trimmed = form.defaults_toml.trim();
    if trimmed.is_empty() {
        if let Err(err) = db::delete(&state.db, &form.model_name).await {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-db-delete-error",
                    &i18n::args([("err", err.to_string().into())]),
                ),
            );
        }
        return toast(
            FlashKind::Success,
            t_args(
                lang,
                "admin-cleared-defaults",
                &i18n::args([("model", form.model_name.clone().into())]),
            ),
        );
    }
    // Parse before persisting so we never store TOML that
    // `apply_defaults` would later reject — keeps the round-trip
    // honest (whatever you save is exactly what the merge will use).
    if let Err(err) = merge::parse_defaults(&form.defaults_toml) {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-invalid-toml",
                &i18n::args([("err", err.to_string().into())]),
            ),
        );
    }
    if let Err(err) = db::upsert(&state.db, &form.model_name, &form.defaults_toml).await {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-upsert-error",
                &i18n::args([("err", err.to_string().into())]),
            ),
        );
    }
    toast(
        FlashKind::Success,
        t_args(
            lang,
            "admin-saved-defaults",
            &i18n::args([("model", form.model_name.clone().into())]),
        ),
    )
}

/// POST /admin/models/reasoning — save a model's reasoning style (how its
/// reasoning budget is expressed on the wire). Kept separate from the TOML
/// save so clearing the sampling defaults (which deletes the row) doesn't also
/// reset the reasoning style, and vice versa. An empty / "auto" value clears
/// the explicit choice and falls back to name-based auto-detection.
pub async fn models_reasoning_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: ReasoningForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-malformed-form",
                    &i18n::args([("err", err.to_string().into())]),
                ),
            );
        }
    };
    if form.model_name.is_empty() {
        return toast(FlashKind::Error, t(lang, "admin-missing-model-name"));
    }
    // Empty / "auto" → clear the explicit choice (NULL), otherwise store the
    // canonical value. Validate against the known styles so a bad submission
    // can't poison the row.
    let style = match form.reasoning_style.trim() {
        "" | "auto" => None,
        s @ ("none" | "qwen" | "openai" | "glm" | "anthropic") => Some(s),
        other => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-unknown-reasoning-style",
                    &i18n::args([("style", other.to_string().into())]),
                ),
            );
        }
    };
    if let Err(err) = db::set_reasoning_style(&state.db, &form.model_name, style).await {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-error",
                &i18n::args([("err", err.to_string().into())]),
            ),
        );
    }
    toast(
        FlashKind::Success,
        t_args(
            lang,
            "admin-saved-reasoning-style",
            &i18n::args([("model", form.model_name.clone().into())]),
        ),
    )
}

/// POST /admin/models/reasoning-budget — save a model's per-effort reasoning
/// overrides (token budgets for Qwen/Anthropic, `reasoning_effort` levels for
/// OpenAI/GLM). Like the reasoning-style save, this touches only its own
/// columns so it composes with the TOML save and the style save. Empty fields
/// clear that level back to the built-in default.
pub async fn models_reasoning_budget_save(
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: ReasoningBudgetForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-malformed-form",
                    &i18n::args([("err", err.to_string().into())]),
                ),
            );
        }
    };
    if form.model_name.is_empty() {
        return toast(FlashKind::Error, t(lang, "admin-missing-model-name"));
    }
    // Parse + validate each field; an empty string clears the level.
    let budget = |s: &str| -> Result<Option<i64>, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(None);
        }
        match s.parse::<i64>() {
            Ok(n) if n >= 1 => Ok(Some(n)),
            _ => Err(t_args(
                lang,
                "admin-budget-not-positive",
                &i18n::args([("value", s.to_string().into())]),
            )),
        }
    };
    let effort = |s: &str| -> Result<Option<String>, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(None);
        }
        // Validate against the full intensity scale (the GLM superset); the
        // dropdown only offers per-style-valid values anyway.
        if crate::server::reasoning::ReasoningStyle::Glm
            .effort_levels()
            .contains(&s)
        {
            Ok(Some(s.to_string()))
        } else {
            Err(t_args(
                lang,
                "admin-unknown-reasoning-effort",
                &i18n::args([("value", s.to_string().into())]),
            ))
        }
    };
    let build = || -> Result<db::ReasoningOverrideCols, String> {
        Ok(db::ReasoningOverrideCols {
            budget_standard: budget(&form.budget_standard)?,
            budget_deep: budget(&form.budget_deep)?,
            budget_max: budget(&form.budget_max)?,
            effort_standard: effort(&form.effort_standard)?,
            effort_deep: effort(&form.effort_deep)?,
            effort_max: effort(&form.effort_max)?,
        })
    };
    let cols = match build() {
        Ok(c) => c,
        Err(e) => return toast(FlashKind::Error, e),
    };
    if let Err(err) = db::set_reasoning_overrides(&state.db, &form.model_name, &cols).await {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-error",
                &i18n::args([("err", err.to_string().into())]),
            ),
        );
    }
    toast(
        FlashKind::Success,
        t_args(
            lang,
            "admin-saved-reasoning-budget",
            &i18n::args([("model", form.model_name.clone().into())]),
        ),
    )
}

/// POST /admin/models/context-window — save a model's context window in tokens
/// (used by the auto-compaction trigger). Touches only its own column so it
/// composes with the TOML/style/budget saves. An empty value clears the row's
/// window and falls back to the global `default_context_window`.
pub async fn models_context_window_save(
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: ContextWindowForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-malformed-form",
                    &i18n::args([("err", err.to_string().into())]),
                ),
            );
        }
    };
    if form.model_name.is_empty() {
        return toast(FlashKind::Error, t(lang, "admin-missing-model-name"));
    }
    let window = match form.context_window.trim() {
        "" => None,
        s => match s.parse::<i64>() {
            Ok(n) if n >= 1 => Some(n),
            _ => {
                return toast(
                    FlashKind::Error,
                    t_args(
                        lang,
                        "admin-context-window-invalid",
                        &i18n::args([("value", s.to_string().into())]),
                    ),
                );
            }
        },
    };
    if let Err(err) = db::set_context_window(&state.db, &form.model_name, window).await {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-upsert-error",
                &i18n::args([("err", err.to_string().into())]),
            ),
        );
    }
    let msg = match window {
        Some(_) => t_args(
            lang,
            "admin-context-window-saved",
            &i18n::args([("model", form.model_name.clone().into())]),
        ),
        None => t_args(
            lang,
            "admin-context-window-cleared",
            &i18n::args([("model", form.model_name.clone().into())]),
        ),
    };
    toast(FlashKind::Success, msg)
}

/// POST /admin/models/pricing — save a model's per-1M-token prices for cost
/// accounting. Touches only the two price columns so it composes with the
/// TOML/style/budget/context-window saves. Either field blank clears that
/// side (the model becomes unpriced on that side → 0 cost). A negative or
/// non-numeric value is rejected.
pub async fn models_pricing_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: PricingForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-malformed-form",
                    &i18n::args([("err", err.to_string().into())]),
                ),
            );
        }
    };
    if form.model_name.is_empty() {
        return toast(FlashKind::Error, t(lang, "admin-missing-model-name"));
    }
    // A blank field clears that side; a present one must parse to a finite,
    // non-negative price.
    let parse_price = |s: &str| -> Result<Option<f64>, String> {
        match s.trim() {
            "" => Ok(None),
            v => match v.parse::<f64>() {
                Ok(n) if n.is_finite() && n >= 0.0 => Ok(Some(n)),
                _ => Err(v.to_string()),
            },
        }
    };
    let input_price = match parse_price(&form.input_price) {
        Ok(p) => p,
        Err(v) => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-price-invalid",
                    &i18n::args([("value", v.into())]),
                ),
            );
        }
    };
    let output_price = match parse_price(&form.output_price) {
        Ok(p) => p,
        Err(v) => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-price-invalid",
                    &i18n::args([("value", v.into())]),
                ),
            );
        }
    };
    if let Err(err) = db::set_pricing(&state.db, &form.model_name, input_price, output_price).await
    {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-upsert-error",
                &i18n::args([("err", err.to_string().into())]),
            ),
        );
    }
    toast(
        FlashKind::Success,
        t_args(
            lang,
            "admin-price-saved",
            &i18n::args([("model", form.model_name.clone().into())]),
        ),
    )
}

/// POST /admin/models/defaults — set (or clear) the default model pre-selected
/// for a feature (chat / voice-transcription / image). An empty `model` clears
/// the override, restoring the "first advertised model" behaviour. The stored
/// id is only *resolved* against the live set at use-time (see
/// [`crate::server::feature_defaults`]), so saving a model that later stops
/// being served degrades gracefully rather than erroring.
pub async fn models_defaults_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: DefaultsForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-malformed-form",
                    &i18n::args([("err", err.to_string().into())]),
                ),
            );
        }
    };
    let Some(feature) = Feature::from_wire(&form.feature) else {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-defaults-unknown-feature",
                &i18n::args([("feature", form.feature.clone().into())]),
            ),
        );
    };
    let trimmed = form.model.trim();
    let model = (!trimmed.is_empty()).then_some(trimmed);
    if let Err(err) = feature_defaults::set(&state.db, feature, model).await {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-error",
                &i18n::args([("err", err.to_string().into())]),
            ),
        );
    }
    let msg = match model {
        Some(m) => t_args(
            lang,
            "admin-defaults-saved",
            &i18n::args([("model", m.to_string().into())]),
        ),
        None => t(lang, "admin-defaults-cleared"),
    };
    toast(FlashKind::Success, msg)
}

/// POST /admin/models/capabilities — save a model's capability flags (tri-state)
/// and fallback model references. Each capability field is "" (unknown/clear),
/// "true", or "false". Fallback fields are a model id or "" (no fallback).
pub async fn models_capabilities_save(
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: CapabilitiesForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-malformed-form",
                    &i18n::args([("err", err.to_string().into())]),
                ),
            );
        }
    };
    if form.model_name.is_empty() {
        return toast(FlashKind::Error, t(lang, "admin-missing-model-name"));
    }

    let parse_tri = |s: &str| -> Option<bool> {
        match s.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    };
    let parse_fb = |s: &str| -> Option<String> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };

    let caps = crate::server::db::model_defaults::ModelCapabilities {
        vision: parse_tri(&form.cap_vision),
        audio_input: parse_tri(&form.cap_audio_input),
        pdf_input: parse_tri(&form.cap_pdf_input),
        tools: parse_tri(&form.cap_tools),
        parallel_tools: parse_tri(&form.cap_parallel_tools),
        structured_output: parse_tri(&form.cap_structured_output),
        fallback_vision: parse_fb(&form.fallback_vision),
        fallback_tools: parse_fb(&form.fallback_tools),
    };

    match crate::server::db::model_defaults::set_capabilities(&state.db, &form.model_name, &caps)
        .await
    {
        Ok(()) => toast(
            FlashKind::Success,
            t_args(
                lang,
                "admin-capabilities-saved",
                &i18n::args([("model", form.model_name.into())]),
            ),
        ),
        Err(e) => toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-capabilities-error",
                &i18n::args([("err", e.to_string().into())]),
            ),
        ),
    }
}

/// POST /admin/upstreams/reload — rebuild the upstream registry from the DB
/// topology (pools, backends, fallbacks) and re-spawn health probes for the
/// new/changed backends. The "Apply changes" button after editing pools or
/// backends in the admin UI.
pub async fn upstreams_reload(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }

    let snapshot = match crate::server::db::upstreams_config::load_snapshot(&state.db).await {
        Ok(s) => s,
        Err(e) => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-reload-error",
                    &i18n::args([("err", e.to_string().into())]),
                ),
            );
        }
    };

    match state.upstreams.reload(&snapshot, &state.crypto) {
        Ok(()) => {
            // Await the initial probe round (bounded by the 2s probe timeout)
            // before responding, so a brand-new backend has its model set
            // populated before "Apply changes" reports success. Unchanged
            // backends already carried their live set across the swap in
            // `reload`, so existing traffic never sees an empty-model window.
            // `spawn` also arms the new generation's probe loops; the old
            // generation's loops retire themselves (see `health::run_probe`).
            crate::server::upstreams::health::spawn(state.upstreams.clone()).await;
            let pool_count = snapshot.pools.len();
            let backend_count = snapshot.backends.len();
            tracing::info!(
                pools = pool_count,
                backends = backend_count,
                "upstream registry reloaded from DB"
            );
            toast(
                FlashKind::Success,
                t_args(
                    lang,
                    "admin-reloaded",
                    &i18n::args([
                        ("pools", pool_count.to_string().into()),
                        ("backends", backend_count.to_string().into()),
                    ]),
                ),
            )
        }
        Err(e) => toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-reload-error",
                &i18n::args([("err", e.to_string().into())]),
            ),
        ),
    }
}

#[derive(serde::Deserialize)]
struct CapabilitiesForm {
    model_name: String,
    #[serde(default)]
    cap_vision: String,
    #[serde(default)]
    cap_audio_input: String,
    #[serde(default)]
    cap_pdf_input: String,
    #[serde(default)]
    cap_tools: String,
    #[serde(default)]
    cap_parallel_tools: String,
    #[serde(default)]
    cap_structured_output: String,
    #[serde(default)]
    fallback_vision: String,
    #[serde(default)]
    fallback_tools: String,
}

#[derive(serde::Deserialize)]
struct DefaultsForm {
    feature: String,
    #[serde(default)]
    model: String,
}

#[derive(serde::Deserialize)]
struct SaveForm {
    model_name: String,
    defaults_toml: String,
}

#[derive(serde::Deserialize)]
struct ContextWindowForm {
    model_name: String,
    #[serde(default)]
    context_window: String,
}

#[derive(serde::Deserialize)]
struct PricingForm {
    model_name: String,
    #[serde(default)]
    input_price: String,
    #[serde(default)]
    output_price: String,
}

#[derive(serde::Deserialize)]
struct ReasoningForm {
    model_name: String,
    reasoning_style: String,
}

/// Per-effort reasoning overrides form. All fields optional / empty = "clear
/// this level" (fall back to the built-in default). For token-budget styles the
/// `budget_*` fields are filled; for effort-level styles the `effort_*` fields.
/// The form only renders the relevant set, but we accept and store all six so a
/// later style switch can clear stale values.
#[derive(Default, serde::Deserialize)]
struct ReasoningBudgetForm {
    model_name: String,
    #[serde(default)]
    budget_standard: String,
    #[serde(default)]
    budget_deep: String,
    #[serde(default)]
    budget_max: String,
    #[serde(default)]
    effort_standard: String,
    #[serde(default)]
    effort_deep: String,
    #[serde(default)]
    effort_max: String,
}

struct ModelRow {
    name: String,
    toml: String,
    reasoning_style: String,
    budget_standard: Option<i64>,
    budget_deep: Option<i64>,
    budget_max: Option<i64>,
    effort_standard: String,
    effort_deep: String,
    effort_max: String,
    context_window: Option<i64>,
    input_price: Option<f64>,
    output_price: Option<f64>,
    /// Model capabilities for the capability editor. `None` = unknown.
    cap_vision: Option<bool>,
    cap_tools: Option<bool>,
    cap_structured_output: Option<bool>,
    fallback_vision: Option<String>,
    fallback_tools: Option<String>,
}

/// A non-chat model (embedding / image / speech / transcription) shown in the
/// pricing-only section: sampling, reasoning, and context settings don't apply
/// to it, but per-model cost pricing does.
struct OtherModelRow {
    name: String,
    /// The pool kind that serves it (`embedding` / `image` / …), for the badge.
    kind_label: &'static str,
    input_price: Option<f64>,
    output_price: Option<f64>,
}

/// A model name that is an alias for another (real) model. Rendered read-only:
/// aliases carry no settings or price of their own — each request is configured
/// and metered as the model it resolves to (`target`).
struct AliasRow {
    name: String,
    /// The real model id this alias resolves to.
    target: String,
    /// The pool kind that serves it (`chat` / `embedding` / …), for the badge.
    kind_label: &'static str,
}

/// One row of the "Default models" card: the feature, its label, the models
/// its pool advertises (sorted), and the currently-stored override (raw — not
/// yet resolved against `available`).
struct FeatureDefaultRow {
    feature: Feature,
    label_key: &'static str,
    available: Vec<String>,
    current: Option<String>,
}

/// Build the per-feature default-model rows for the admin card. A feature with
/// no advertised models is omitted (nothing to pick from).
async fn defaults_rows(state: &RamaState) -> Vec<FeatureDefaultRow> {
    let mut out = Vec::new();
    for (feature, label_key) in [
        (Feature::Chat, "admin-defaults-chat-label"),
        (Feature::Transcription, "admin-defaults-voice-label"),
        (Feature::Image, "admin-defaults-image-label"),
        (Feature::Embedding, "admin-defaults-embedding-label"),
    ] {
        let mut available = state.upstreams.models_for_kind(feature.pool_kind());
        available.sort();
        if available.is_empty() {
            continue;
        }
        let current = feature_defaults::get(&state.db, feature).await;
        out.push(FeatureDefaultRow {
            feature,
            label_key,
            available,
            current,
        });
    }
    out
}

/// The "Default models" card: one auto-saving `<select>` per feature that picks
/// which model is pre-selected in the chat/voice pickers (and the API fallback
/// when a call omits a model). Renders nothing when no feature has any models.
fn render_defaults_card(lang: Lang, rows: &[FeatureDefaultRow]) -> Html {
    if rows.is_empty() {
        return html! { (String::new()) }.to_html();
    }
    let action = "/admin/models/defaults";
    let selects: Vec<Html> = rows
        .iter()
        .map(|row| {
            // "First available" (empty value) is selected when nothing is
            // stored, or the stored id is no longer advertised.
            let current_served = row
                .current
                .as_deref()
                .filter(|c| row.available.iter().any(|a| a == c));
            let mut opts: Vec<Html> = Vec::with_capacity(row.available.len() + 1);
            let first_label = t(lang, "admin-defaults-first-option");
            opts.push(if current_served.is_none() {
                html! { option(value: "", selected: "selected") { (first_label) } }.to_html()
            } else {
                html! { option(value: "") { (first_label) } }.to_html()
            });
            for m in &row.available {
                opts.push(if current_served == Some(m.as_str()) {
                    html! { option(value: (m.clone()), selected: "selected") { (m.clone()) } }
                        .to_html()
                } else {
                    html! { option(value: (m.clone())) { (m.clone()) } }.to_html()
                });
            }
            html! {
                form(
                    method: "post",
                    action: (action),
                    class: "form-control m-0"
                ) {
                    input(type: "hidden", name: "feature", value: (row.feature.as_str()));
                    span(class: "label-text text-xs mb-1") { (t(lang, row.label_key)) }
                    select(
                        name: "model",
                        "aria-label": (t(lang, row.label_key)),
                        "data-on:change": (format!("@post('{action}', {{contentType: 'form'}})")),
                        class: "select select-bordered select-sm w-full"
                    ) {
                        for o in opts.iter() { (o.clone()) }
                    }
                }
            }
            .to_html()
        })
        .collect();
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                header(class: "flex flex-col gap-1") {
                    h2(class: "card-title text-base") { (t(lang, "admin-defaults-heading")) }
                    p(class: "text-base-content/70 text-sm") { (t(lang, "admin-defaults-intro")) }
                }
                // Uniform half-width columns: a 2-up grid so every picker is the
                // same width and rows line up (single column on narrow screens).
                div(class: "grid grid-cols-1 md:grid-cols-2 gap-4") {
                    for s in selects.iter() { (s.clone()) }
                }
            }
        }
    }
    .to_html()
}

fn render_models_body(
    lang: Lang,
    currency: &str,
    defaults: &[FeatureDefaultRow],
    rows: &[ModelRow],
    aliases: &[AliasRow],
    other: &[OtherModelRow],
    all_models: &[String],
) -> Html {
    let cards: Vec<Html> = rows
        .iter()
        .map(|row| render_model_card(lang, currency, row, all_models))
        .collect();
    html! {
        section(class: "max-w-5xl mx-auto p-4 sm:p-6 flex flex-col gap-4") {
            header(class: "flex flex-col gap-1") {
                h1(class: "text-2xl font-bold") { (t(lang, "admin-heading")) }
                p(class: "text-base-content/70 text-sm") {
                    (t(lang, "admin-intro-prefix"))
                    " "
                    strong { (t(lang, "admin-intro-every")) }
                    " "
                    (t(lang, "admin-intro-middle"))
                    " "
                    strong { (t(lang, "admin-intro-always-wins")) }
                    (t(lang, "admin-intro-suffix"))
                }
            }
            (render_defaults_card(lang, defaults))
            if rows.is_empty() {
                div(class: "alert") {
                    (icons::info(18))
                    span {
                        (t(lang, "admin-no-models"))
                    }
                }
            } else {
                div(class: "flex flex-col gap-4") {
                    for c in cards.iter() {
                        (c.clone())
                    }
                }
            }
            (render_alias_card(lang, aliases))
            (render_other_models_card(lang, currency, other))
        }
    }
    .to_html()
}

/// Read-only card listing alias model names and the real model each resolves to.
/// Aliases carry no settings or price of their own — they inherit the target's,
/// and cost accounting meters the resolved id — so they get no editors here,
/// just an `(alias)` chip and the "→ target" mapping. Renders nothing when there
/// are no aliases.
fn render_alias_card(lang: Lang, aliases: &[AliasRow]) -> Html {
    if aliases.is_empty() {
        return html! {}.to_html();
    }
    let rows: Vec<Html> = aliases
        .iter()
        .map(|a| {
            html! {
                div(class: "flex items-center gap-2 flex-wrap py-1") {
                    span(class: "badge badge-ghost badge-sm") { (a.kind_label) }
                    span(class: "font-mono text-sm break-all") { (a.name.clone()) }
                    span(class: "badge badge-info badge-sm") { (t(lang, "admin-alias-chip")) }
                    span(class: "text-base-content/40") { "→" }
                    span(class: "font-mono text-sm break-all text-base-content/70") {
                        (a.target.clone())
                    }
                }
            }
            .to_html()
        })
        .collect();
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                header(class: "flex flex-col gap-1") {
                    h2(class: "card-title text-base") { (t(lang, "admin-aliases-heading")) }
                    p(class: "text-base-content/70 text-sm") { (t(lang, "admin-aliases-intro")) }
                }
                div(class: "flex flex-col divide-y divide-base-200") {
                    for r in rows.iter() { (r.clone()) }
                }
            }
        }
    }
    .to_html()
}

/// Pricing-only card for non-chat models (embedding / image / speech /
/// transcription). Those pools serve no sampling/reasoning/context settings,
/// but their calls still cost money, so each gets just the price control.
/// Renders nothing when there are no such models.
fn render_other_models_card(lang: Lang, currency: &str, other: &[OtherModelRow]) -> Html {
    if other.is_empty() {
        return html! {}.to_html();
    }
    let rows: Vec<Html> = other
        .iter()
        .map(|m| {
            html! {
                div(class: "flex items-center justify-between gap-3 flex-wrap py-1") {
                    div(class: "flex items-center gap-2 min-w-0") {
                        span(class: "badge badge-ghost badge-sm") { (m.kind_label) }
                        span(class: "font-mono text-sm break-all") { (m.name.clone()) }
                    }
                    (render_pricing(lang, currency, &m.name, m.input_price, m.output_price))
                }
            }
            .to_html()
        })
        .collect();
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                header(class: "flex flex-col gap-1") {
                    h2(class: "card-title text-base") { (t(lang, "admin-other-heading")) }
                    p(class: "text-base-content/70 text-sm") { (t(lang, "admin-other-intro")) }
                }
                div(class: "flex flex-col divide-y divide-base-200") {
                    for r in rows.iter() { (r.clone()) }
                }
            }
        }
    }
    .to_html()
}

fn render_model_card(lang: Lang, currency: &str, row: &ModelRow, all_models: &[String]) -> Html {
    let action = "/admin/models";
    let placeholder = format!(
        "{}\n\
         # temperature      = 0.7\n\
         # top_p            = 0.95\n\
         # top_k            = 40\n\
         # min_p            = 0.05\n\
         # repeat_penalty   = 1.1\n\
         # frequency_penalty= 0.0\n\
         # presence_penalty = 0.0\n\
         # max_tokens       = 2048\n\
         # stop             = [\"<|im_end|>\"]\n",
        t(lang, "admin-toml-placeholder-header")
    );
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                header(class: "flex items-center justify-between gap-3 flex-wrap") {
                    h2(class: "card-title text-base font-mono break-all") { (row.name.clone()) }
                    div(class: "flex items-center gap-2 flex-wrap") {
                        (render_pricing(lang, currency, &row.name, row.input_price, row.output_price))
                        (render_context_window(lang, row))
                        (render_reasoning_select(lang, row))
                    }
                }
                (render_reasoning_budget(lang, row))
                (render_capabilities(lang, row, all_models))
                form(
                    method: "post",
                    action: (action),
                    "data-on:submit__prevent":
                        (format!("@post('{action}', {{contentType: 'form'}})")),
                    class: "flex flex-col gap-2 m-0"
                ) {
                    input(type: "hidden", name: "model_name", value: (row.name.clone()));
                    label(class: "label sr-only", "for": (format!("toml-{}", row.name))) {
                        (t(lang, "admin-toml-defaults-label"))
                    }
                    textarea(
                        id: (format!("toml-{}", row.name)),
                        name: "defaults_toml",
                        class: "textarea textarea-bordered font-mono text-sm w-full leading-relaxed",
                        rows: "11",
                        spellcheck: "false",
                        placeholder: (placeholder)
                    ) { (row.toml.clone()) }
                    div(class: "flex justify-end") {
                        button(type: "submit", class: "btn btn-primary btn-sm") {
                            (icons::check(14))
                            span { (t(lang, "admin-save")) }
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

/// Capabilities editor: tri-state selects (Unknown/Enabled/Disabled) for each
/// capability + fallback model dropdowns. One auto-saving form posts all fields
/// to `/admin/models/capabilities` on any change.
fn render_capabilities(lang: Lang, row: &ModelRow, all_models: &[String]) -> Html {
    let action = "/admin/models/capabilities";
    let post = format!("@post('{action}', {{contentType: 'form'}})");
    html! {
        form(method: "post", action: (action), class: "m-0") {
            input(type: "hidden", name: "model_name", value: (row.name.clone()));
            div(class: "collapse collapse-arrow bg-base-200/50") {
                input(type: "checkbox");
                div(class: "collapse-title font-medium text-sm py-2 min-h-0") {
                    (t(lang, "admin-capabilities-heading"))
                }
                div(class: "collapse-content") {
                    div(class: "grid grid-cols-1 sm:grid-cols-3 gap-2 mt-1") {
                        (cap_tri_select(lang, "cap_vision", "Vision", row.cap_vision, &post))
                        (cap_tri_select(lang, "cap_tools", "Tools", row.cap_tools, &post))
                        (cap_tri_select(lang, "cap_structured_output", &t(lang, "admin-cap-structured-output"), row.cap_structured_output, &post))
                    }
                    div(class: "grid grid-cols-1 sm:grid-cols-2 gap-2 mt-2") {
                        (cap_fb_select(lang, "fallback_vision", &t(lang, "admin-cap-fallback-vision"), &row.fallback_vision, all_models, &post))
                        (cap_fb_select(lang, "fallback_tools", &t(lang, "admin-cap-fallback-tools"), &row.fallback_tools, all_models, &post))
                    }
                }
            }
        }
    }
    .to_html()
}

fn cap_tri_select(lang: Lang, name: &str, label: &str, val: Option<bool>, post: &str) -> Html {
    let opts = [
        super::select_option("", &t(lang, "admin-cap-unknown"), val.is_none()),
        super::select_option("true", &t(lang, "admin-cap-enabled"), val == Some(true)),
        super::select_option("false", &t(lang, "admin-cap-disabled"), val == Some(false)),
    ];
    html! {
        label(class: "form-control gap-1") {
            span(class: "text-xs opacity-70") { (label) }
            select(name: (name), class: "select select-bordered select-sm", "data-on:change": (post)) {
                for o in opts.iter() { (o.clone()) }
            }
        }
    }
    .to_html()
}

fn cap_fb_select(
    lang: Lang,
    name: &str,
    label: &str,
    current: &Option<String>,
    all_models: &[String],
    post: &str,
) -> Html {
    let mut opts: Vec<Html> = vec![super::select_option(
        "",
        &t(lang, "admin-cap-no-fallback"),
        false,
    )];
    for m in all_models {
        opts.push(super::select_option(
            m,
            m,
            current.as_deref() == Some(m.as_str()),
        ));
    }
    html! {
        label(class: "form-control gap-1") {
            span(class: "text-xs opacity-70") { (label) }
            select(name: (name), class: "select select-bordered select-sm", "data-on:change": (post)) {
                for o in opts.iter() { (o.clone()) }
            }
        }
    }
    .to_html()
}

/// The per-model context-window field: a tiny number input that auto-saves on
/// change. Drives the auto-compaction trigger (compaction fires once a session's
/// replayed prompt reaches `[chat.compaction] trigger_ratio` of this window).
/// Blank falls back to the global `default_context_window`.
fn render_context_window(lang: Lang, row: &ModelRow) -> Html {
    let action = "/admin/models/context-window";
    let value = row
        .context_window
        .map(|n| n.to_string())
        .unwrap_or_default();
    let label_text = t(lang, "admin-context-window-label");
    let unit_text = t(lang, "admin-context-window-unit");
    let placeholder = t(lang, "admin-context-window-placeholder");
    let aria = t(lang, "admin-context-window-aria");
    html! {
        form(method: "post", action: (action), class: "m-0") {
            input(type: "hidden", name: "model_name", value: (row.name.clone()));
            label(class: "input input-bordered input-xs flex items-center gap-1") {
                span(class: "text-xs opacity-70") { (label_text) }
                input(
                    type: "number", name: "context_window", value: (value), min: "1",
                    placeholder: (placeholder), "aria-label": (aria),
                    "data-on:change": (format!("@post('{action}', {{contentType: 'form'}})")),
                    class: "w-24"
                );
                span(class: "text-xs opacity-70") { (unit_text) }
            }
        }
    }
    .to_html()
}

/// The per-model pricing field: two tiny number inputs (input / output price
/// per 1M tokens) in one auto-saving form. Both are posted together on any
/// change to `/admin/models/pricing`, so editing either side never clears the
/// other. Blank = unpriced (0 cost). The `currency` label is cosmetic.
fn render_pricing(
    lang: Lang,
    currency: &str,
    model_name: &str,
    input_price: Option<f64>,
    output_price: Option<f64>,
) -> Html {
    let action = "/admin/models/pricing";
    let fmt = |p: Option<f64>| p.map(|n| n.to_string()).unwrap_or_default();
    let in_val = fmt(input_price);
    let out_val = fmt(output_price);
    let label_text = t_args(
        lang,
        "admin-price-label",
        &i18n::args([("cur", currency.to_string().into())]),
    );
    let in_ph = t(lang, "admin-price-in-placeholder");
    let out_ph = t(lang, "admin-price-out-placeholder");
    let in_aria = t(lang, "admin-price-in-aria");
    let out_aria = t(lang, "admin-price-out-aria");
    let unit_text = t(lang, "admin-price-unit");
    let post = format!("@post('{action}', {{contentType: 'form'}})");
    html! {
        form(method: "post", action: (action), class: "m-0") {
            input(type: "hidden", name: "model_name", value: (model_name.to_string()));
            label(class: "input input-bordered input-xs flex items-center gap-1") {
                span(class: "text-xs opacity-70") { (label_text) }
                input(
                    type: "number", name: "input_price", value: (in_val), min: "0", step: "any",
                    placeholder: (in_ph), "aria-label": (in_aria),
                    "data-on:change": (post.clone()),
                    class: "w-16"
                );
                span(class: "text-xs opacity-50") { "/" }
                input(
                    type: "number", name: "output_price", value: (out_val), min: "0", step: "any",
                    placeholder: (out_ph), "aria-label": (out_aria),
                    "data-on:change": (post),
                    class: "w-16"
                );
                span(class: "text-xs opacity-70") { (unit_text) }
            }
        }
    }
    .to_html()
}

/// The per-model "reasoning style" picker: a tiny form whose `<select>`
/// auto-saves on change. Tells `apply_effort` how this model expects its
/// reasoning budget on the wire; "Auto" leaves it to name detection.
fn render_reasoning_select(lang: Lang, row: &ModelRow) -> Html {
    let action = "/admin/models/reasoning";
    let options: &[(&str, &str)] = &[
        ("", "admin-reasoning-auto"),
        ("none", "admin-reasoning-none"),
        ("qwen", "admin-reasoning-qwen"),
        ("openai", "admin-reasoning-openai"),
        ("glm", "admin-reasoning-glm"),
        ("anthropic", "admin-reasoning-anthropic"),
    ];
    let current = row.reasoning_style.as_str();
    let option_html: Vec<Html> = options
        .iter()
        .map(|(value, key)| {
            let label = t(lang, key);
            if *value == current {
                html! { option(value: (*value), selected: "selected") { (label) } }.to_html()
            } else {
                html! { option(value: (*value)) { (label) } }.to_html()
            }
        })
        .collect();
    html! {
        form(
            method: "post",
            action: (action),
            class: "m-0"
        ) {
            input(type: "hidden", name: "model_name", value: (row.name.clone()));
            select(
                name: "reasoning_style",
                "aria-label": (t(lang, "admin-reasoning-style-aria")),
                "data-on:change": (format!("@post('{action}', {{contentType: 'form'}})")),
                class: "select select-bordered select-xs"
            ) {
                for o in option_html.iter() {
                    (o.clone())
                }
            }
        }
    }
    .to_html()
}

/// Adaptive per-effort reasoning controls, shown below the style picker. Token-
/// budget styles (Qwen, Anthropic) get integer token fields; effort-level styles
/// (OpenAI, GLM) get `reasoning_effort` dropdowns; styles without reasoning
/// render nothing. The effective style is resolved the same way the request path
/// does (explicit choice, else name detection), so the right controls appear
/// even when the style is left on "Auto".
fn render_reasoning_budget(lang: Lang, row: &ModelRow) -> Html {
    use crate::server::reasoning::ReasoningStyle;
    let explicit = (!row.reasoning_style.is_empty()).then_some(row.reasoning_style.as_str());
    let style = ReasoningStyle::resolve(explicit, &row.name);
    let action = "/admin/models/reasoning-budget";

    let (controls, hint) = if style.uses_token_budget() {
        let num = |name: &str, label_key: &str, val: &Option<i64>| {
            let v = val.map(|n| n.to_string()).unwrap_or_default();
            html! {
                label(class: "form-control") {
                    span(class: "label-text text-xs") { (t(lang, label_key)) }
                    input(
                        type: "number", name: (name), value: (v), min: "1",
                        placeholder: (t(lang, "admin-budget-placeholder")),
                        class: "input input-bordered input-xs w-28"
                    );
                }
            }
            .to_html()
        };
        let controls = html! {
            (num("budget_standard", "admin-effort-standard", &row.budget_standard))
            (num("budget_deep", "admin-effort-deep", &row.budget_deep))
            (num("budget_max", "admin-effort-max", &row.budget_max))
        }
        .to_html();
        (controls, t(lang, "admin-budget-hint"))
    } else if style.uses_effort_level() {
        let levels = style.effort_levels();
        let sel = |name: &str, label_key: &str, current: &str| {
            let mut opts: Vec<Html> = Vec::new();
            let default_label = t(lang, "admin-effort-default-option");
            opts.push(if current.is_empty() {
                html! { option(value: "", selected: "selected") { (default_label) } }.to_html()
            } else {
                html! { option(value: "") { (default_label) } }.to_html()
            });
            for lvl in levels {
                opts.push(if *lvl == current {
                    html! { option(value: (*lvl), selected: "selected") { (*lvl) } }.to_html()
                } else {
                    html! { option(value: (*lvl)) { (*lvl) } }.to_html()
                });
            }
            html! {
                label(class: "form-control") {
                    span(class: "label-text text-xs") { (t(lang, label_key)) }
                    select(name: (name), class: "select select-bordered select-xs") {
                        for o in opts.iter() { (o.clone()) }
                    }
                }
            }
            .to_html()
        };
        let controls = html! {
            (sel("effort_standard", "admin-effort-standard", &row.effort_standard))
            (sel("effort_deep", "admin-effort-deep", &row.effort_deep))
            (sel("effort_max", "admin-effort-max", &row.effort_max))
        }
        .to_html();
        (controls, t(lang, "admin-effort-hint"))
    } else {
        // No reasoning support → no controls.
        return html! { (String::new()) }.to_html();
    };

    html! {
        form(
            method: "post",
            action: (action),
            "data-on:submit__prevent":
                (format!("@post('{action}', {{contentType: 'form'}})")),
            class: "flex flex-col gap-2 m-0 border-t border-base-300 pt-3"
        ) {
            input(type: "hidden", name: "model_name", value: (row.name.clone()));
            span(class: "text-xs text-base-content/60") { (hint) }
            div(class: "flex flex-wrap items-end gap-3") {
                (controls)
                button(type: "submit", class: "btn btn-ghost btn-xs ml-auto self-end") {
                    (icons::check(12))
                    span { (t(lang, "admin-save-reasoning-budget")) }
                }
            }
        }
    }
    .to_html()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(available: &[&str], current: Option<&str>) -> FeatureDefaultRow {
        FeatureDefaultRow {
            feature: Feature::Chat,
            label_key: "admin-defaults-chat-label",
            available: available.iter().map(|s| s.to_string()).collect(),
            current: current.map(str::to_string),
        }
    }

    /// The card's `<select>` must post to the exact route the router registers,
    /// carry the feature discriminator, and mark the stored model selected — the
    /// UI-directive ↔ endpoint contract.
    #[test]
    fn defaults_card_wires_select_to_endpoint_and_marks_current() {
        let rows = vec![row(&["glm-4.5", "glm-4.7"], Some("glm-4.7"))];
        let html = render_defaults_card(Lang::En, &rows).to_string();
        assert!(
            html.contains(r#"action="/admin/models/defaults""#),
            "form must post to the defaults route: {html}"
        );
        assert!(
            html.contains("@post(") && html.contains("/admin/models/defaults"),
            "select must auto-save to the defaults route on change: {html}"
        );
        assert!(
            html.contains(r#"name="feature" value="chat""#),
            "hidden feature field must identify the feature: {html}"
        );
        assert!(
            html.contains(r#"value="glm-4.7" selected="selected""#),
            "the stored default must be the selected option: {html}"
        );
    }

    /// With nothing stored, or a stored id that's no longer advertised, the
    /// "First available" option is the selected one (graceful fallback) and no
    /// concrete model is pre-selected.
    #[test]
    fn defaults_card_selects_first_available_when_unset_or_stale() {
        for current in [None, Some("no-longer-served")] {
            let rows = vec![row(&["a", "b"], current)];
            let html = render_defaults_card(Lang::En, &rows).to_string();
            assert!(
                html.contains(r#"value="" selected="selected""#),
                "first-available must be selected ({current:?}): {html}"
            );
            assert_eq!(
                html.matches(r#"selected="selected""#).count(),
                1,
                "exactly one option selected ({current:?}): {html}"
            );
        }
    }

    /// No advertised models for any feature → the card renders nothing (no
    /// stray form pointing at the endpoint).
    #[test]
    fn defaults_card_empty_when_no_features() {
        let html = render_defaults_card(Lang::En, &[]).to_string();
        assert!(
            !html.contains("/admin/models/defaults"),
            "empty card must not render the form: {html}"
        );
    }
}
