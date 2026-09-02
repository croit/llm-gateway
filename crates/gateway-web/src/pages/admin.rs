// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/models` — per-model settings.
//!
//! A "Default models" card (auto-saving per-feature pickers) plus one
//! filterable list of every model the gateway advertises — chat models,
//! aliases, and other-kind models (embedding / image / speech / transcription).
//! Each real model is a collapsed row (name, kind, price, context, resolved
//! reasoning style, and which facets are configured); expanding it reveals a
//! single editor form that persists **all** settings at once via
//! `POST /admin/models/save`:
//!
//!   - per-1M-token prices (cost accounting),
//!   - context window (drives auto-compaction),
//!   - reasoning style + adaptive per-effort budgets (Qwen/Anthropic token
//!     budgets) or effort levels (OpenAI/GLM),
//!   - capability tri-states + vision/tools fallbacks,
//!   - sampling defaults (TOML, merged into requests that don't set the key).
//!
//! "Clear all overrides" (`POST /admin/models/clear`) deletes the row. Aliases
//! are dimmed, non-expandable rows (they inherit their target's settings);
//! other-kind models get a price-only editor posting to the same save endpoint.
//!
//! All routes are gated on the `admin` role via
//! [`super::require_admin_or_403`] — non-admins see a 403 page and never the
//! form. The sidebar entry is also conditional on that role.

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

use gateway_core::server::db::model_defaults as db;
use gateway_core::server::feature_defaults::{self, Feature};
use gateway_core::server::model_defaults as merge;
use gateway_core::server::reasoning::ReasoningStyle;
use gateway_core::server::upstreams::PoolKind;
use gateway_features::server::search_settings::{self, SearchProvider, SearchSettingsView};
use gateway_runtime::rama_server::state::RamaState;

/// GET /admin/models — the default-models card + a single filterable list of
/// every advertised model (chat, aliases, other kinds).
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
    // and metered as the model they resolve to. Split them out: real models get
    // the full editor, aliases get a dimmed "→ target" row.
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
            pricing_unit: row
                .as_ref()
                .map(|r| r.pricing_unit)
                .unwrap_or(db::PricingUnit::Tokens),
            cap_vision: row.as_ref().and_then(|r| r.capabilities.vision),
            cap_audio_input: row.as_ref().and_then(|r| r.capabilities.audio_input),
            cap_pdf_input: row.as_ref().and_then(|r| r.capabilities.pdf_input),
            cap_tools: row.as_ref().and_then(|r| r.capabilities.tools),
            cap_parallel_tools: row.as_ref().and_then(|r| r.capabilities.parallel_tools),
            cap_structured_output: row.as_ref().and_then(|r| r.capabilities.structured_output),
            fallback_vision: row
                .as_ref()
                .and_then(|r| r.capabilities.fallback_vision.clone()),
            fallback_tools: row
                .as_ref()
                .and_then(|r| r.capabilities.fallback_tools.clone()),
        });
    }

    // Non-chat models get a price-only row. Their aliases join the alias list.
    // Dedup by id; a model already shown as chat is skipped.
    let chat_names: std::collections::HashSet<&str> =
        chat_models.iter().map(|(n, _)| n.as_str()).collect();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut other: Vec<OtherModelRow> = Vec::new();
    for (kind, kind_label) in [
        (PoolKind::Embedding, "embedding"),
        (PoolKind::Image, "image"),
        (PoolKind::Speech, "speech"),
        (PoolKind::Transcription, "transcription"),
        (PoolKind::Ocr, "ocr"),
        (PoolKind::Rerank, "rerank"),
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
                pricing_unit: displayed_pricing_unit(
                    kind_label,
                    row.as_ref().map(|r| r.pricing_unit),
                ),
                name,
            });
        }
    }

    let defaults = defaults_rows(&state).await;
    // A DB read failure here must not blank the whole page — fall back to the
    // built-in defaults so the card still renders (and the operator can fix
    // the setting), same posture as the per-model rows above.
    let search = search_settings::view(&state.db)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "loading web-search settings");
            SearchSettingsView {
                provider: SearchProvider::default(),
                searxng_url: None,
                brave_key_set: false,
            }
        });
    let currency = &state.config().usage.currency;
    let all_models = state.upstreams.all_models();
    let body = render_models_body(
        lang,
        currency,
        PageSettings {
            defaults: &defaults,
            search: &search,
        },
        &rows,
        &aliases,
        &other,
        &all_models,
    );
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    let title = t(lang, "admin-page-title");
    {
        let pctx = super::PageCtx {
            theme,
            lang,
            nav,
            datastar,
            user_email: user.email.clone(),
            is_admin: is_admin(&state, &user),
            skills_enabled: state.user_skills_enabled(),
            impersonating: session.impersonator_id.is_some(),
        };
        nav_or_html_page(&pctx, NavItem::Admin, &title, body, "/admin/models", &chat)
    }
}

// ---------------------------------------------------------------------------
// POST handlers
// ---------------------------------------------------------------------------

/// POST /admin/models/save — persist every per-model setting at once. The form
/// carries `model_name` as a hidden input (rama lowercases path segments, which
/// would mangle case-sensitive HuggingFace ids). All fields are validated up
/// front (TOML parses, budgets positive, efforts known, prices finite &
/// non-negative, context window positive), then written in one atomic upsert.
/// A blank field clears that facet; the row is kept (use "Clear all overrides"
/// to delete it).
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

    // ---- validate every field before touching the DB ----
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
    let pricing_unit = db::PricingUnit::parse(&form.pricing_unit);

    if form.price_only == "1" {
        return match db::set_pricing_with_unit(
            &state.db,
            &form.model_name,
            input_price,
            output_price,
            pricing_unit,
        )
        .await
        {
            Ok(()) => toast(
                FlashKind::Success,
                t_args(
                    lang,
                    "admin-saved-model",
                    &i18n::args([("model", form.model_name.clone().into())]),
                ),
            ),
            Err(err) => toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "admin-db-upsert-error",
                    &i18n::args([("err", err.to_string().into())]),
                ),
            ),
        };
    }
    let context_window = match form.context_window.trim() {
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
    let reasoning_style = match form.reasoning_style.trim() {
        "" | "auto" => None,
        s @ ("none" | "qwen" | "openai" | "glm" | "anthropic") => Some(s.to_string()),
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
        if ReasoningStyle::Glm.effort_levels().contains(&s) {
            Ok(Some(s.to_string()))
        } else {
            Err(t_args(
                lang,
                "admin-unknown-reasoning-effort",
                &i18n::args([("value", s.to_string().into())]),
            ))
        }
    };
    let overrides = match (|| -> Result<db::ReasoningOverrideCols, String> {
        Ok(db::ReasoningOverrideCols {
            budget_standard: budget(&form.budget_standard)?,
            budget_deep: budget(&form.budget_deep)?,
            budget_max: budget(&form.budget_max)?,
            effort_standard: effort(&form.effort_standard)?,
            effort_deep: effort(&form.effort_deep)?,
            effort_max: effort(&form.effort_max)?,
        })
    })() {
        Ok(c) => c,
        Err(e) => return toast(FlashKind::Error, e),
    };
    let toml = form.defaults_toml.trim();
    if !toml.is_empty()
        && let Err(err) = merge::parse_defaults(&form.defaults_toml)
    {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-invalid-toml",
                &i18n::args([("err", err.to_string().into())]),
            ),
        );
    }

    let tri = |s: &str| -> Option<bool> {
        match s.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    };
    let fb = |s: &str| -> Option<String> {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    let fields = db::AllFields {
        defaults_toml: form.defaults_toml.clone(),
        reasoning_style,
        overrides,
        context_window,
        input_price,
        output_price,
        pricing_unit,
        capabilities: db::ModelCapabilities {
            vision: tri(&form.cap_vision),
            audio_input: tri(&form.cap_audio_input),
            pdf_input: tri(&form.cap_pdf_input),
            tools: tri(&form.cap_tools),
            parallel_tools: tri(&form.cap_parallel_tools),
            structured_output: tri(&form.cap_structured_output),
            fallback_vision: fb(&form.fallback_vision),
            fallback_tools: fb(&form.fallback_tools),
        },
    };
    match db::set_all(&state.db, &form.model_name, &fields).await {
        Ok(()) => toast(
            FlashKind::Success,
            t_args(
                lang,
                "admin-saved-model",
                &i18n::args([("model", form.model_name.clone().into())]),
            ),
        ),
        Err(err) => toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-upsert-error",
                &i18n::args([("err", err.to_string().into())]),
            ),
        ),
    }
}

/// POST /admin/models/clear — delete a model's stored overrides entirely,
/// returning it to the backend's built-in behaviour. `model_name` rides in the
/// body (see [`models_save`]).
pub async fn models_clear(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: ClearForm = match serde_urlencoded::from_bytes(&body) {
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
    match db::delete(&state.db, &form.model_name).await {
        Ok(()) => toast(
            FlashKind::Success,
            t_args(
                lang,
                "admin-cleared-defaults",
                &i18n::args([("model", form.model_name.clone().into())]),
            ),
        ),
        Err(err) => toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-delete-error",
                &i18n::args([("err", err.to_string().into())]),
            ),
        ),
    }
}

/// POST /admin/models/defaults — set (or clear) the default model pre-selected
/// for a feature (chat / voice-transcription / image / embedding). An empty
/// `model` clears the override, restoring the "first advertised model"
/// behaviour. Resolved against the live set at use-time, so a stale id degrades
/// gracefully.
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

/// POST /admin/models/search — persist the web-search backend settings.
///
/// The Brave key follows the same convention as backend API keys: a blank
/// field keeps the stored value (so re-saving the form doesn't wipe a secret
/// the operator can't read back), and an explicit `clear_brave_key` checkbox
/// is the only way to remove it.
pub async fn models_search_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: SearchForm = match serde_urlencoded::from_bytes(&body) {
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
    let Some(provider) = SearchProvider::from_wire(&form.provider) else {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-search-unknown-provider",
                &i18n::args([("provider", form.provider.clone().into())]),
            ),
        );
    };

    let db_err = |err: gateway_core::server::db::DbError| {
        toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-error",
                &i18n::args([("err", err.to_string().into())]),
            ),
        )
    };
    if let Err(err) = search_settings::set_provider(&state.db, provider).await {
        return db_err(err);
    }
    if let Err(err) = search_settings::set_searxng_url(&state.db, &form.searxng_url).await {
        return db_err(err);
    }
    // Blank + no clear request = leave the stored key alone.
    let key = form.brave_api_key.trim();
    if form.clear_brave_key.is_some() {
        if let Err(err) = search_settings::set_brave_key(&state.db, &state.crypto, "").await {
            return db_err(err);
        }
    } else if !key.is_empty()
        && let Err(err) = search_settings::set_brave_key(&state.db, &state.crypto, key).await
    {
        return db_err(err);
    }

    toast(FlashKind::Success, t(lang, "admin-search-saved"))
}

/// POST /admin/upstreams/reload — rebuild the upstream registry from the DB
/// topology (pools, backends, fallbacks) and re-spawn health probes. The "Apply
/// changes" button on `/admin/upstreams`. On success it clears the in-memory
/// dirty counter and patches the `topologyDirty` signal to 0 so the apply bar
/// disappears without a reload.
pub async fn upstreams_reload(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    use session_core::chrome::{Flash, sse_response, sse_toast};

    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }

    let snapshot = match gateway_core::server::db::upstreams_config::load_snapshot(&state.db).await
    {
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
            gateway_core::server::upstreams::health::spawn(state.upstreams.clone()).await;
            let pool_count = snapshot.pools.len();
            let backend_count = snapshot.backends.len();
            tracing::info!(
                pools = pool_count,
                backends = backend_count,
                "upstream registry reloaded from DB"
            );
            state.topology_dirty_reset();
            sse_response(&[
                sse_toast(&Flash {
                    kind: FlashKind::Success,
                    message: t_args(
                        lang,
                        "admin-reloaded",
                        &i18n::args([
                            ("pools", pool_count.to_string().into()),
                            ("backends", backend_count.to_string().into()),
                        ]),
                    ),
                }),
                super::dirty_signal(0),
            ])
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

// ---------------------------------------------------------------------------
// Form structs
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct DefaultsForm {
    feature: String,
    #[serde(default)]
    model: String,
}

#[derive(serde::Deserialize)]
struct ClearForm {
    model_name: String,
}

#[derive(serde::Deserialize)]
struct SearchForm {
    provider: String,
    #[serde(default)]
    searxng_url: String,
    #[serde(default)]
    brave_api_key: String,
    /// Present (as `Some("1")`) only when the checkbox was ticked — an
    /// unchecked checkbox isn't submitted at all.
    #[serde(default)]
    clear_brave_key: Option<String>,
}

/// The consolidated per-model save form. Every field is optional / blank =
/// "clear this facet"; the row itself is kept (delete it via `models_clear`).
#[derive(Default, serde::Deserialize)]
struct SaveForm {
    model_name: String,
    #[serde(default)]
    input_price: String,
    #[serde(default)]
    output_price: String,
    #[serde(default)]
    pricing_unit: String,
    #[serde(default)]
    price_only: String,
    #[serde(default)]
    context_window: String,
    #[serde(default)]
    reasoning_style: String,
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
    #[serde(default)]
    defaults_toml: String,
}

// ---------------------------------------------------------------------------
// Row view models
// ---------------------------------------------------------------------------

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
    pricing_unit: db::PricingUnit,
    cap_vision: Option<bool>,
    cap_audio_input: Option<bool>,
    cap_pdf_input: Option<bool>,
    cap_tools: Option<bool>,
    cap_parallel_tools: Option<bool>,
    cap_structured_output: Option<bool>,
    fallback_vision: Option<String>,
    fallback_tools: Option<String>,
}

impl ModelRow {
    fn has_price(&self) -> bool {
        self.input_price.is_some() || self.output_price.is_some()
    }
    fn has_budget(&self) -> bool {
        self.budget_standard.is_some()
            || self.budget_deep.is_some()
            || self.budget_max.is_some()
            || !self.effort_standard.is_empty()
            || !self.effort_deep.is_empty()
            || !self.effort_max.is_empty()
    }
    fn has_caps(&self) -> bool {
        self.cap_vision.is_some()
            || self.cap_audio_input.is_some()
            || self.cap_pdf_input.is_some()
            || self.cap_tools.is_some()
            || self.cap_parallel_tools.is_some()
            || self.cap_structured_output.is_some()
            || self.fallback_vision.is_some()
            || self.fallback_tools.is_some()
    }
    fn configured(&self) -> bool {
        self.has_price()
            || self.has_budget()
            || self.has_caps()
            || self.context_window.is_some()
            || !self.toml.trim().is_empty()
            || !self.reasoning_style.is_empty()
    }
}

struct OtherModelRow {
    name: String,
    kind_label: &'static str,
    input_price: Option<f64>,
    output_price: Option<f64>,
    pricing_unit: db::PricingUnit,
}

struct AliasRow {
    name: String,
    target: String,
    kind_label: &'static str,
}

struct FeatureDefaultRow {
    feature: Feature,
    label_key: &'static str,
    available: Vec<String>,
    current: Option<String>,
}

// ---------------------------------------------------------------------------
// Default-models card (unchanged behaviour)
// ---------------------------------------------------------------------------

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

fn render_defaults_card(lang: Lang, rows: &[FeatureDefaultRow]) -> Html {
    if rows.is_empty() {
        return html! { (String::new()) }.to_html();
    }
    let action = "/admin/models/defaults";
    let selects: Vec<Html> = rows
        .iter()
        .map(|row| {
            let current_served = row
                .current
                .as_deref()
                .filter(|c| row.available.iter().any(|a| a == c));
            let mut opts: Vec<Html> = Vec::with_capacity(row.available.len() + 1);
            let first_label = t(lang, "admin-defaults-first-option");
            opts.push(super::select_option(
                "",
                &first_label,
                current_served.is_none(),
            ));
            for m in &row.available {
                opts.push(super::select_option(
                    m,
                    m,
                    current_served == Some(m.as_str()),
                ));
            }
            html! {
                form(method: "post", action: (action), class: "flex flex-col gap-1 m-0") {
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
                div(class: "grid grid-cols-1 md:grid-cols-2 gap-4") {
                    for s in selects.iter() { (s.clone()) }
                }
            }
        }
    }
    .to_html()
}

/// The "Web search" card: which backend answers `search_web`, plus its
/// credentials. Posts to `/admin/models/search`.
///
/// These settings used to be environment variables (`SEARCH_PROVIDER`,
/// `SEARXNG_URL`, `BRAVE_SEARCH_API_KEY`); they now live in the DB with the
/// Brave key sealed at rest, like every other gateway secret.
fn render_search_card(lang: Lang, view: &SearchSettingsView) -> Html {
    let action = "/admin/models/search";
    let provider_opts = [
        super::select_option(
            SearchProvider::Searxng.as_str(),
            &t(lang, "admin-search-provider-searxng"),
            view.provider == SearchProvider::Searxng,
        ),
        super::select_option(
            SearchProvider::Brave.as_str(),
            &t(lang, "admin-search-provider-brave"),
            view.provider == SearchProvider::Brave,
        ),
    ];
    // Whether a key exists is safe to show; the key itself never is.
    let key_state = if view.brave_key_set {
        t(lang, "admin-search-brave-key-set")
    } else {
        t(lang, "admin-search-brave-key-unset")
    };
    let url_value = view.searxng_url.clone().unwrap_or_default();
    // Only offer to remove a key when there is one. Rendering it next to
    // "No key stored." invites the operator to tick a box that does nothing,
    // and reads as though something is stored after all.
    let clear_key = if view.brave_key_set {
        super::bool_checkbox(
            "clear_brave_key",
            "1",
            &t(lang, "admin-search-brave-key-clear"),
            false,
            false,
        )
    } else {
        plait::html! {}.to_html()
    };
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                header(class: "flex flex-col gap-1") {
                    h2(class: "card-title text-base") { (t(lang, "admin-search-heading")) }
                    p(class: "text-base-content/70 text-sm") { (t(lang, "admin-search-intro")) }
                }
                form(method: "post", action: (action), class: "flex flex-col gap-3 m-0") {
                    div(class: "grid grid-cols-1 md:grid-cols-2 gap-4") {
                        label(class: "flex flex-col gap-1") {
                            span(class: "label-text text-xs") { (t(lang, "admin-search-provider-label")) }
                            select(name: "provider", class: "select select-bordered select-sm w-full") {
                                for o in provider_opts.iter() { (o.clone()) }
                            }
                        }
                        label(class: "flex flex-col gap-1") {
                            span(class: "label-text text-xs") { (t(lang, "admin-search-searxng-url-label")) }
                            input(
                                type: "url",
                                name: "searxng_url",
                                value: (url_value),
                                placeholder: (t(lang, "admin-search-searxng-url-placeholder")),
                                class: "input input-bordered input-sm w-full"
                            );
                        }
                    }
                    div(class: "flex flex-col gap-1") {
                        span(class: "label-text text-xs") { (t(lang, "admin-search-brave-key-label")) }
                        input(
                            type: "password",
                            name: "brave_api_key",
                            autocomplete: "off",
                            placeholder: (t(lang, "admin-search-brave-key-placeholder")),
                            class: "input input-bordered input-sm w-full"
                        );
                        span(class: "text-base-content/60 text-xs") { (key_state) }
                        (clear_key)
                    }
                    div(class: "flex justify-end") {
                        button(type: "submit", class: "btn btn-primary btn-sm") {
                            (t(lang, "admin-search-save"))
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

// ---------------------------------------------------------------------------
// Page body + model list
// ---------------------------------------------------------------------------

/// Inline grid template shared by the list header and every row, so the columns
/// line up. Inline because the shipped CSS bundle carries no arbitrary
/// `grid-cols-[…]` utility.
const ROW_GRID: &str = "display:grid;grid-template-columns:minmax(170px,1.6fr) 82px 108px 84px 128px minmax(110px,1.1fr) 18px;gap:10px;align-items:center";

/// The deployment-wide settings cards that sit above the model list. Grouped
/// into one struct so adding a card doesn't grow `render_models_body`'s
/// parameter list (which is already at clippy's ceiling).
struct PageSettings<'a> {
    defaults: &'a [FeatureDefaultRow],
    search: &'a SearchSettingsView,
}

fn render_models_body(
    lang: Lang,
    currency: &str,
    settings: PageSettings<'_>,
    rows: &[ModelRow],
    aliases: &[AliasRow],
    other: &[OtherModelRow],
    all_models: &[String],
) -> Html {
    html! {
        section(class: "max-w-5xl mx-auto p-4 sm:p-6 flex flex-col gap-4") {
            header(class: "flex flex-col gap-1") {
                h1(class: "text-2xl font-bold") { (t(lang, "admin-heading")) }
                p(class: "text-base-content/70 text-sm max-w-2xl") {
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
            (render_defaults_card(lang, settings.defaults))
            (render_search_card(lang, settings.search))
            (render_model_list(lang, currency, rows, aliases, other, all_models))
        }
    }
    .to_html()
}

fn render_model_list(
    lang: Lang,
    currency: &str,
    rows: &[ModelRow],
    aliases: &[AliasRow],
    other: &[OtherModelRow],
    all_models: &[String],
) -> Html {
    if rows.is_empty() && aliases.is_empty() && other.is_empty() {
        return html! {
            div(class: "alert") {
                (icons::info(18))
                span { (t(lang, "admin-no-models")) }
            }
        }
        .to_html();
    }
    let chat_items: Vec<Html> = rows
        .iter()
        .map(|r| render_chat_item(lang, currency, r, all_models))
        .collect();
    let alias_items: Vec<Html> = aliases.iter().map(|a| render_alias_item(lang, a)).collect();
    let other_items: Vec<Html> = other
        .iter()
        .map(|m| render_other_item(lang, currency, m))
        .collect();

    html! {
        article(class: "card border border-base-300 bg-base-100", "data-signals": "{mfilter: '', mkind: 'all'}") {
            // Toolbar: text filter + mutually-exclusive kind chips.
            div(class: "card-body gap-3 pb-0") {
                div(class: "flex gap-2 items-center flex-wrap") {
                    input(
                        type: "text", "data-bind": "mfilter",
                        placeholder: (t(lang, "admin-filter-placeholder")),
                        "aria-label": (t(lang, "admin-filter-placeholder")),
                        class: "input input-bordered input-sm w-60 max-w-full"
                    );
                    (kind_chip(lang, "admin-filter-all", "all"))
                    (kind_chip(lang, "admin-filter-chat", "chat"))
                    (kind_chip(lang, "admin-filter-other", "other"))
                    (kind_chip(lang, "admin-filter-aliases", "alias"))
                    (kind_chip(lang, "admin-filter-configured", "configured"))
                }
            }
            div(class: "card-body gap-0 pt-3 overflow-x-auto") {
                // Header row.
                div(
                    class: "text-[11px] uppercase tracking-wide text-base-content/50 border-b border-base-300 pb-2",
                    style: (ROW_GRID)
                ) {
                    span { (t(lang, "admin-col-model")) }
                    span { (t(lang, "admin-col-kind")) }
                    span { (t(lang, "admin-col-price")) }
                    span { (t(lang, "admin-col-context")) }
                    span { (t(lang, "admin-col-reasoning")) }
                    span { (t(lang, "admin-col-configured")) }
                    span {}
                }
                for it in chat_items.iter() { (it.clone()) }
                for it in alias_items.iter() { (it.clone()) }
                for it in other_items.iter() { (it.clone()) }
            }
        }
    }
    .to_html()
}

/// A mutually-exclusive kind filter chip. Clicking sets `$mkind`; the active
/// chip gets `btn-active` via `data-class`.
fn kind_chip(lang: Lang, label_key: &str, value: &str) -> Html {
    let click = format!("$mkind = '{value}'");
    let active = format!("{{'btn-active': $mkind === '{value}'}}");
    let label = t(lang, label_key);
    html! {
        button(type: "button", class: "btn btn-xs", "data-on:click": (click), "data-class": (active)) {
            (label)
        }
    }
    .to_html()
}

/// The per-row `data-show` expression: matches the kind chip (or "configured")
/// AND the text filter (case-insensitive substring on the model name).
fn row_show(group: &str, configured: bool, name: &str) -> String {
    let name_json =
        serde_json::to_string(&name.to_lowercase()).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "($mkind === 'all' || $mkind === '{group}' || ($mkind === 'configured' && {configured})) \
         && {name_json}.includes($mfilter.toLowerCase())"
    )
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

fn render_chat_item(lang: Lang, currency: &str, row: &ModelRow, all_models: &[String]) -> Html {
    let show = row_show("chat", row.configured(), &row.name);
    let price = fmt_price_pair(lang, row.input_price, row.output_price, row.pricing_unit);
    let context = row
        .context_window
        .map(fmt_context)
        .unwrap_or_else(|| t(lang, "admin-value-default"));
    let context_dim = row.context_window.is_none();
    let reasoning = reasoning_summary(lang, &row.reasoning_style, &row.name);
    let cfg_badges = configured_badges(lang, row);

    html! {
        details(class: "group border-b border-base-300", "data-show": (show)) {
            summary(class: "cursor-pointer select-none py-2 hover:bg-base-200/40 list-none [&::-webkit-details-marker]:hidden") {
                div(style: (ROW_GRID)) {
                    span(class: "font-mono text-sm font-semibold break-all") { (row.name.clone()) }
                    span { span(class: "badge badge-secondary badge-sm") { "chat" } }
                    (price_cell(&price))
                    (value_cell(&context, context_dim))
                    span(class: "text-xs") { (reasoning) }
                    (cfg_badges)
                    span(class: "text-base-content/40 transition-transform group-open:rotate-180") { (icons::chevron_down(14)) }
                }
            }
            div(class: "bg-base-200/40 border-t border-base-300 p-3 flex flex-col gap-3") {
                (render_chat_editor(lang, currency, row, all_models))
            }
        }
    }
    .to_html()
}

fn render_other_item(lang: Lang, currency: &str, m: &OtherModelRow) -> Html {
    let configured = m.input_price.is_some() || m.output_price.is_some();
    let show = row_show("other", configured, &m.name);
    let price = fmt_price_pair(lang, m.input_price, m.output_price, m.pricing_unit);
    let na = t(lang, "admin-value-na");
    let cfg = if configured {
        html! { span(class: "badge badge-ghost badge-sm") { (t(lang, "admin-badge-price")) } }
            .to_html()
    } else {
        html! { span(class: "text-xs text-base-content/40") { (t(lang, "admin-not-configured")) } }
            .to_html()
    };
    html! {
        details(class: "group border-b border-base-300", "data-show": (show)) {
            summary(class: "cursor-pointer select-none py-2 hover:bg-base-200/40 list-none [&::-webkit-details-marker]:hidden") {
                div(style: (ROW_GRID)) {
                    span(class: "font-mono text-sm font-semibold break-all") { (m.name.clone()) }
                    span { span(class: "badge badge-secondary badge-sm") { (m.kind_label) } }
                    (price_cell(&price))
                    (value_cell(&na, true))
                    span(class: "text-xs text-base-content/40") { (na.clone()) }
                    span(class: "flex flex-wrap gap-1") { (cfg) }
                    span(class: "text-base-content/40 transition-transform group-open:rotate-180") { (icons::chevron_down(14)) }
                }
            }
            div(class: "bg-base-200/40 border-t border-base-300 p-3 flex flex-col gap-3") {
                (render_price_only_editor(
                    lang,
                    currency,
                    &m.name,
                    m.input_price,
                    m.output_price,
                    m.pricing_unit,
                ))
            }
        }
    }
    .to_html()
}

fn render_alias_item(lang: Lang, a: &AliasRow) -> Html {
    let show = row_show("alias", false, &a.name);
    html! {
        div(class: "border-b border-base-300 opacity-70", "data-show": (show)) {
            div(style: (ROW_GRID), class: "py-2") {
                span(class: "font-mono text-sm break-all text-base-content/70") {
                    (a.name.clone())
                    " "
                    span(class: "text-base-content/40") { "→ " (a.target.clone()) }
                }
                span { span(class: "badge badge-info badge-sm") { (t(lang, "admin-alias-chip")) } }
                span(class: "text-xs text-base-content/40") { (a.kind_label) }
                span {}
                span {}
                span(class: "text-xs text-base-content/40") { (t(lang, "admin-alias-inherits")) }
                span {}
            }
        }
    }
    .to_html()
}

// ---------------------------------------------------------------------------
// Cell formatters
// ---------------------------------------------------------------------------

fn fmt_price_pair(
    lang: Lang,
    input: Option<f64>,
    output: Option<f64>,
    unit: db::PricingUnit,
) -> Option<String> {
    match (input, output) {
        (None, None) => None,
        (i, o) => {
            let f = |p: Option<f64>| p.map(|n| format!("{n}")).unwrap_or_else(|| "—".into());
            Some(format!(
                "{} / {} {}",
                f(i),
                f(o),
                pricing_unit_label(lang, unit)
            ))
        }
    }
}

fn fmt_context(n: i64) -> String {
    if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

fn pricing_unit_for_kind(kind: &str) -> db::PricingUnit {
    match kind {
        "image" => db::PricingUnit::Images,
        "speech" => db::PricingUnit::Characters,
        "transcription" => db::PricingUnit::Seconds,
        _ => db::PricingUnit::Tokens,
    }
}

fn displayed_pricing_unit(kind: &str, stored: Option<db::PricingUnit>) -> db::PricingUnit {
    stored.unwrap_or_else(|| pricing_unit_for_kind(kind))
}

fn pricing_unit_label(lang: Lang, unit: db::PricingUnit) -> String {
    t(
        lang,
        match unit {
            db::PricingUnit::Tokens => "admin-price-unit-tokens",
            db::PricingUnit::Images => "admin-price-unit-images",
            db::PricingUnit::Characters => "admin-price-unit-characters",
            db::PricingUnit::Seconds => "admin-price-unit-seconds",
        },
    )
}

fn price_cell(price: &Option<String>) -> Html {
    match price {
        Some(p) => html! { span(class: "text-xs tabular-nums") { (p.clone()) } }.to_html(),
        None => {
            html! { span(class: "text-xs tabular-nums text-base-content/40") { "—" } }.to_html()
        }
    }
}

fn value_cell(value: &str, dim: bool) -> Html {
    let value = value.to_string();
    if dim {
        html! { span(class: "text-xs tabular-nums text-base-content/40") { (value) } }.to_html()
    } else {
        html! { span(class: "text-xs tabular-nums") { (value) } }.to_html()
    }
}

/// Collapsed-row reasoning summary: the explicit style's label, or
/// "Auto → <resolved>" when left on auto (name detection).
fn reasoning_summary(lang: Lang, explicit: &str, model: &str) -> String {
    if explicit.is_empty() {
        let resolved = ReasoningStyle::resolve(None, model);
        t_args(
            lang,
            "admin-reasoning-auto-resolved",
            &i18n::args([("style", reasoning_style_short(resolved).into())]),
        )
    } else {
        let style = ReasoningStyle::resolve(Some(explicit), model);
        reasoning_style_short(style).to_string()
    }
}

fn reasoning_style_short(style: ReasoningStyle) -> &'static str {
    match style {
        ReasoningStyle::None => "none",
        ReasoningStyle::Qwen => "Qwen",
        ReasoningStyle::OpenAi => "OpenAI",
        ReasoningStyle::Glm => "GLM",
        ReasoningStyle::Anthropic => "Anthropic",
    }
}

/// The "configured" cell: one small badge per configured facet, or a dim "not
/// configured".
fn configured_badges(lang: Lang, row: &ModelRow) -> Html {
    if !row.configured() {
        return html! { span(class: "text-xs text-base-content/40") { (t(lang, "admin-not-configured")) } }
            .to_html();
    }
    let badge = |label: String| -> Html {
        html! { span(class: "badge badge-ghost badge-sm") { (label) } }.to_html()
    };
    let mut out: Vec<Html> = Vec::new();
    if row.has_price() {
        out.push(badge(t(lang, "admin-badge-price")));
    }
    if row.context_window.is_some() {
        out.push(badge(t(lang, "admin-badge-ctx")));
    }
    if row.has_budget() {
        out.push(badge(t(lang, "admin-badge-budget")));
    }
    if row.has_caps() {
        out.push(badge(t(lang, "admin-badge-caps")));
    }
    if !row.toml.trim().is_empty() {
        out.push(badge(t(lang, "admin-badge-toml")));
    }
    html! {
        span(class: "flex flex-wrap gap-1") {
            for b in out.iter() { (b.clone()) }
        }
    }
    .to_html()
}

// ---------------------------------------------------------------------------
// Editors
// ---------------------------------------------------------------------------

fn render_chat_editor(lang: Lang, currency: &str, row: &ModelRow, all_models: &[String]) -> Html {
    let action = "/admin/models/save";
    let post = format!("@post('{action}', {{contentType: 'form'}})");
    let clear = "@post('/admin/models/clear', {contentType: 'form'})".to_string();
    let in_val = row.input_price.map(|n| n.to_string()).unwrap_or_default();
    let out_val = row.output_price.map(|n| n.to_string()).unwrap_or_default();
    let ctx_val = row
        .context_window
        .map(|n| n.to_string())
        .unwrap_or_default();
    let price_label = t_args(
        lang,
        "admin-price-label",
        &i18n::args([
            ("cur", currency.to_string().into()),
            ("unit", pricing_unit_label(lang, row.pricing_unit).into()),
        ]),
    );

    html! {
        form(method: "post", action: (action), "data-on:submit__prevent": (post), class: "flex flex-col gap-3 m-0") {
            input(type: "hidden", name: "model_name", value: (row.name.clone()));
            input(type: "hidden", name: "pricing_unit", value: (row.pricing_unit.as_str()));
            div(class: "grid grid-cols-1 sm:grid-cols-2 gap-3") {
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "admin-price-in-label")) " (" (price_label.clone()) ")" }
                    input(type: "number", name: "input_price", value: (in_val), min: "0", step: "any",
                        placeholder: (t(lang, "admin-price-in-placeholder")), class: "input input-bordered input-sm");
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "admin-price-out-label")) " (" (price_label) ")" }
                    input(type: "number", name: "output_price", value: (out_val), min: "0", step: "any",
                        placeholder: (t(lang, "admin-price-out-placeholder")), class: "input input-bordered input-sm");
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "admin-context-window-full-label")) }
                    input(type: "number", name: "context_window", value: (ctx_val), min: "1",
                        placeholder: (t(lang, "admin-context-window-placeholder")), class: "input input-bordered input-sm");
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "admin-reasoning-style-label")) }
                    (reasoning_style_select(lang, &row.reasoning_style))
                }
            }
            (render_reasoning_controls(lang, row))
            (render_capabilities(lang, row, all_models))
            label(class: "flex flex-col gap-1") {
                span(class: "text-xs opacity-70") { (t(lang, "admin-toml-defaults-label")) }
                textarea(
                    name: "defaults_toml",
                    class: "textarea textarea-bordered font-mono text-sm w-full leading-relaxed",
                    rows: "6", spellcheck: "false",
                    placeholder: (toml_placeholder(lang))
                ) { (row.toml.clone()) }
            }
            div(class: "flex items-center gap-2") {
                button(type: "button", class: "btn btn-ghost btn-xs", "data-on:click": (clear)) {
                    (t(lang, "admin-clear-overrides"))
                }
                span(class: "flex-1") {}
                button(type: "button", class: "btn btn-ghost btn-sm",
                    "data-on:click": "el.closest('details').open = false") { (t(lang, "admin-cancel")) }
                button(type: "submit", class: "btn btn-primary btn-sm") {
                    (icons::check(14))
                    span { (t(lang, "admin-save-model")) }
                }
            }
        }
    }
    .to_html()
}

fn render_price_only_editor(
    lang: Lang,
    currency: &str,
    name: &str,
    input_price: Option<f64>,
    output_price: Option<f64>,
    pricing_unit: db::PricingUnit,
) -> Html {
    let action = "/admin/models/save";
    let post = format!("@post('{action}', {{contentType: 'form'}})");
    let in_val = input_price.map(|n| n.to_string()).unwrap_or_default();
    let out_val = output_price.map(|n| n.to_string()).unwrap_or_default();
    let price_label = t_args(
        lang,
        "admin-price-label",
        &i18n::args([
            ("cur", currency.to_string().into()),
            ("unit", pricing_unit_label(lang, pricing_unit).into()),
        ]),
    );
    let name = name.to_string();
    html! {
        form(method: "post", action: (action), "data-on:submit__prevent": (post), class: "flex flex-col gap-3 m-0") {
            input(type: "hidden", name: "model_name", value: (name));
            input(type: "hidden", name: "pricing_unit", value: (pricing_unit.as_str()));
            input(type: "hidden", name: "price_only", value: "1");
            div(class: "grid grid-cols-1 sm:grid-cols-2 gap-3") {
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "admin-price-in-label")) " (" (price_label.clone()) ")" }
                    input(type: "number", name: "input_price", value: (in_val), min: "0", step: "any",
                        placeholder: (t(lang, "admin-price-in-placeholder")), class: "input input-bordered input-sm");
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "admin-price-out-label")) " (" (price_label) ")" }
                    input(type: "number", name: "output_price", value: (out_val), min: "0", step: "any",
                        placeholder: (t(lang, "admin-price-out-placeholder")), class: "input input-bordered input-sm");
                }
            }
            p(class: "text-xs text-base-content/60 m-0") { (t(lang, "admin-other-price-note")) }
            div(class: "flex items-center gap-2 justify-end") {
                button(type: "button", class: "btn btn-ghost btn-sm",
                    "data-on:click": "el.closest('details').open = false") { (t(lang, "admin-cancel")) }
                button(type: "submit", class: "btn btn-primary btn-sm") {
                    (icons::check(14))
                    span { (t(lang, "admin-save-model")) }
                }
            }
        }
    }
    .to_html()
}

fn reasoning_style_select(lang: Lang, current: &str) -> Html {
    let options: &[(&str, &str)] = &[
        ("", "admin-reasoning-auto"),
        ("none", "admin-reasoning-none"),
        ("qwen", "admin-reasoning-qwen"),
        ("openai", "admin-reasoning-openai"),
        ("glm", "admin-reasoning-glm"),
        ("anthropic", "admin-reasoning-anthropic"),
    ];
    let opts: Vec<Html> = options
        .iter()
        .map(|(value, key)| super::select_option(value, &t(lang, key), *value == current))
        .collect();
    html! {
        select(name: "reasoning_style", "aria-label": (t(lang, "admin-reasoning-style-aria")),
            class: "select select-bordered select-sm") {
            for o in opts.iter() { (o.clone()) }
        }
    }
    .to_html()
}

/// Adaptive per-effort controls, rendered from the *resolved* reasoning style
/// (explicit choice, else name detection): token-budget styles get number
/// inputs, effort-level styles get `reasoning_effort` selects, styles without
/// reasoning render nothing. Changing the style and saving re-renders with the
/// right controls.
fn render_reasoning_controls(lang: Lang, row: &ModelRow) -> Html {
    let explicit = (!row.reasoning_style.is_empty()).then_some(row.reasoning_style.as_str());
    let style = ReasoningStyle::resolve(explicit, &row.name);

    let (controls, hint) = if style.uses_token_budget() {
        let num = |name: &str, label_key: &str, val: &Option<i64>| {
            let v = val.map(|n| n.to_string()).unwrap_or_default();
            html! {
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, label_key)) }
                    input(type: "number", name: (name), value: (v), min: "1",
                        placeholder: (t(lang, "admin-budget-placeholder")),
                        class: "input input-bordered input-sm");
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
            let mut opts: Vec<Html> = vec![super::select_option(
                "",
                &t(lang, "admin-effort-default-option"),
                current.is_empty(),
            )];
            for lvl in levels {
                opts.push(super::select_option(lvl, lvl, *lvl == current));
            }
            html! {
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, label_key)) }
                    select(name: (name), class: "select select-bordered select-sm") {
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
        return html! {}.to_html();
    };

    html! {
        div(class: "flex flex-col gap-2 border-t border-base-300 pt-3") {
            span(class: "text-xs text-base-content/60") { (hint) }
            div(class: "grid grid-cols-1 sm:grid-cols-3 gap-3") {
                (controls)
            }
        }
    }
    .to_html()
}

/// Capability tri-states (Vision / Tools / Structured output / Audio / PDF /
/// Parallel tools) + vision/tools fallback selects. Part of the one big form —
/// no auto-save of its own.
fn render_capabilities(lang: Lang, row: &ModelRow, all_models: &[String]) -> Html {
    html! {
        div(class: "flex flex-col gap-2 border-t border-base-300 pt-3") {
            span(class: "text-xs text-base-content/60") { (t(lang, "admin-capabilities-heading")) }
            div(class: "grid grid-cols-1 sm:grid-cols-3 gap-2") {
                (cap_tri_select(lang, "cap_vision", &t(lang, "admin-cap-vision"), row.cap_vision))
                (cap_tri_select(lang, "cap_tools", &t(lang, "admin-cap-tools"), row.cap_tools))
                (cap_tri_select(lang, "cap_structured_output", &t(lang, "admin-cap-structured-output"), row.cap_structured_output))
                (cap_tri_select(lang, "cap_audio_input", &t(lang, "admin-cap-audio-input"), row.cap_audio_input))
                (cap_tri_select(lang, "cap_pdf_input", &t(lang, "admin-cap-pdf-input"), row.cap_pdf_input))
                (cap_tri_select(lang, "cap_parallel_tools", &t(lang, "admin-cap-parallel-tools"), row.cap_parallel_tools))
            }
            div(class: "grid grid-cols-1 sm:grid-cols-2 gap-2") {
                (cap_fb_select(lang, "fallback_vision", &t(lang, "admin-cap-fallback-vision"), &row.fallback_vision, all_models))
                (cap_fb_select(lang, "fallback_tools", &t(lang, "admin-cap-fallback-tools"), &row.fallback_tools, all_models))
            }
        }
    }
    .to_html()
}

fn cap_tri_select(lang: Lang, name: &str, label: &str, val: Option<bool>) -> Html {
    let opts = [
        super::select_option("", &t(lang, "admin-cap-unknown"), val.is_none()),
        super::select_option("true", &t(lang, "admin-cap-enabled"), val == Some(true)),
        super::select_option("false", &t(lang, "admin-cap-disabled"), val == Some(false)),
    ];
    html! {
        label(class: "flex flex-col gap-1") {
            span(class: "text-xs opacity-70") { (label) }
            select(name: (name), class: "select select-bordered select-sm") {
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
) -> Html {
    let mut opts: Vec<Html> = vec![super::select_option(
        "",
        &t(lang, "admin-cap-no-fallback"),
        current.is_none(),
    )];
    for m in all_models {
        opts.push(super::select_option(
            m,
            m,
            current.as_deref() == Some(m.as_str()),
        ));
    }
    html! {
        label(class: "flex flex-col gap-1") {
            span(class: "text-xs opacity-70") { (label) }
            select(name: (name), class: "select select-bordered select-sm") {
                for o in opts.iter() { (o.clone()) }
            }
        }
    }
    .to_html()
}

fn toml_placeholder(lang: Lang) -> String {
    format!(
        "{}\n\
         # temperature      = 0.7\n\
         # top_p            = 0.95\n\
         # max_tokens       = 2048\n\
         # stop             = [\"<|im_end|>\"]\n",
        t(lang, "admin-toml-placeholder-header")
    )
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

    fn chat_row(name: &str) -> ModelRow {
        ModelRow {
            name: name.to_string(),
            toml: String::new(),
            reasoning_style: String::new(),
            budget_standard: None,
            budget_deep: None,
            budget_max: None,
            effort_standard: String::new(),
            effort_deep: String::new(),
            effort_max: String::new(),
            context_window: None,
            input_price: None,
            output_price: None,
            pricing_unit: db::PricingUnit::Tokens,
            cap_vision: None,
            cap_audio_input: None,
            cap_pdf_input: None,
            cap_tools: None,
            cap_parallel_tools: None,
            cap_structured_output: None,
            fallback_vision: None,
            fallback_tools: None,
        }
    }

    #[test]
    fn defaults_card_wires_select_to_endpoint_and_marks_current() {
        let rows = vec![row(&["glm-4.5", "glm-4.7"], Some("glm-4.7"))];
        let html = render_defaults_card(Lang::En, &rows).to_string();
        assert!(
            html.contains(r#"action="/admin/models/defaults""#),
            "{html}"
        );
        assert!(
            html.contains("@post(") && html.contains("/admin/models/defaults"),
            "{html}"
        );
        assert!(html.contains(r#"name="feature" value="chat""#), "{html}");
        assert!(
            html.contains(r#"value="glm-4.7" selected="selected""#),
            "{html}"
        );
    }

    fn search_view(provider: SearchProvider, url: Option<&str>, key: bool) -> SearchSettingsView {
        SearchSettingsView {
            provider,
            searxng_url: url.map(str::to_owned),
            brave_key_set: key,
        }
    }

    #[test]
    fn search_card_posts_to_its_endpoint_and_marks_the_current_provider() {
        let html = render_search_card(
            Lang::En,
            &search_view(SearchProvider::Brave, Some("https://s.example"), false),
        )
        .to_string();
        assert!(html.contains(r#"action="/admin/models/search""#), "{html}");
        assert!(
            html.contains(r#"value="brave" selected="selected""#),
            "{html}"
        );
        // Exactly one option is preselected — the bool-attr trap would render
        // `selected="false"` on the other, which browsers still honour.
        assert_eq!(html.matches(r#"selected="selected""#).count(), 1, "{html}");
        assert!(html.contains(r#"value="https://s.example""#), "{html}");
    }

    #[test]
    fn search_card_defaults_to_searxng_when_nothing_is_stored() {
        let html = render_search_card(Lang::En, &search_view(SearchProvider::Searxng, None, false))
            .to_string();
        assert!(
            html.contains(r#"value="searxng" selected="selected""#),
            "{html}"
        );
        assert_eq!(html.matches(r#"selected="selected""#).count(), 1, "{html}");
    }

    #[test]
    fn search_card_key_field_is_write_only_and_reports_whether_a_key_exists() {
        let with_key =
            render_search_card(Lang::En, &search_view(SearchProvider::Brave, None, true))
                .to_string();
        assert!(with_key.contains(r#"type="password""#), "{with_key}");
        // The field must never be pre-filled — not even with a placeholder
        // that looks like a value.
        assert!(
            !with_key.contains(r#"name="brave_api_key" value="#),
            "key field must have no value attribute: {with_key}"
        );
        assert!(with_key.contains("A key is stored"), "{with_key}");

        let without =
            render_search_card(Lang::En, &search_view(SearchProvider::Brave, None, false))
                .to_string();
        assert!(without.contains("No key stored"), "{without}");
    }

    #[test]
    fn search_card_offers_no_clear_checkbox_without_a_stored_key() {
        // It used to render regardless, so "No key stored." sat next to a
        // "Remove the stored key" box — a control that does nothing, and one
        // that reads as though a key is stored after all.
        let html = render_search_card(Lang::En, &search_view(SearchProvider::Brave, None, false))
            .to_string();
        assert!(html.contains("No key stored"), "{html}");
        assert!(
            !html.contains(r#"name="clear_brave_key""#),
            "nothing to remove, so the box must be absent: {html}"
        );
    }

    #[test]
    fn search_card_clear_checkbox_starts_unchecked() {
        let html = render_search_card(Lang::En, &search_view(SearchProvider::Brave, None, true))
            .to_string();
        assert!(html.contains(r#"name="clear_brave_key""#), "{html}");
        // Unchecked means the attribute is absent entirely (see bool_checkbox).
        assert!(!html.contains("checked="), "{html}");
    }

    #[test]
    fn search_form_treats_an_absent_checkbox_as_no_clear() {
        let form: SearchForm =
            serde_urlencoded::from_str("provider=brave&searxng_url=&brave_api_key=").unwrap();
        assert!(form.clear_brave_key.is_none());
        let ticked: SearchForm =
            serde_urlencoded::from_str("provider=brave&clear_brave_key=1").unwrap();
        assert_eq!(ticked.clear_brave_key.as_deref(), Some("1"));
    }

    #[test]
    fn defaults_card_selects_first_available_when_unset_or_stale() {
        for current in [None, Some("no-longer-served")] {
            let rows = vec![row(&["a", "b"], current)];
            let html = render_defaults_card(Lang::En, &rows).to_string();
            assert!(html.contains(r#"value="" selected="selected""#), "{html}");
            assert_eq!(html.matches(r#"selected="selected""#).count(), 1, "{html}");
        }
    }

    /// The consolidated chat editor posts every facet to the single save
    /// endpoint, carries the hidden model_name, and offers Clear (delete).
    #[test]
    fn chat_editor_wires_all_fields_to_save_endpoint() {
        let mut r = chat_row("qwen-32b");
        r.input_price = Some(0.15);
        r.context_window = Some(262_144);
        r.reasoning_style = "qwen".into();
        let html = render_chat_item(Lang::En, "USD", &r, &["glm-4.6".into()]).to_string();
        assert!(
            html.contains(r#"action="/admin/models/save""#),
            "save action: {html}"
        );
        assert!(
            html.contains(r#"name="model_name" value="qwen-32b""#),
            "model_name: {html}"
        );
        assert!(
            html.contains(r#"name="input_price""#),
            "price field: {html}"
        );
        assert!(
            html.contains(r#"name="context_window""#),
            "context field: {html}"
        );
        assert!(
            html.contains(r#"name="defaults_toml""#),
            "toml field: {html}"
        );
        assert!(html.contains(r#"name="cap_vision""#), "caps field: {html}");
        // Qwen style → token-budget inputs, not effort selects.
        assert!(
            html.contains(r#"name="budget_standard""#),
            "budget inputs: {html}"
        );
        assert!(html.contains("/admin/models/clear"), "clear action: {html}");
        // Explicit qwen style must be the selected option.
        assert!(
            html.contains(r#"value="qwen" selected="selected""#),
            "style selected: {html}"
        );
    }

    /// A GLM-detected model (auto style) renders effort selects, not budget
    /// inputs — controls follow the resolved style.
    #[test]
    fn glm_model_renders_effort_selects() {
        let r = chat_row("glm-4.6");
        let html = render_chat_item(Lang::En, "USD", &r, &[]).to_string();
        assert!(
            html.contains(r#"name="effort_standard""#),
            "effort selects: {html}"
        );
        assert!(
            !html.contains(r#"name="budget_standard""#),
            "no budget inputs: {html}"
        );
    }

    /// The other-kind (price-only) editor posts to the same save endpoint with
    /// just the price fields + model_name.
    #[test]
    fn other_kind_editor_is_price_only() {
        let m = OtherModelRow {
            name: "gpt-image-1".into(),
            kind_label: "image",
            input_price: Some(10.0),
            output_price: Some(40.0),
            pricing_unit: db::PricingUnit::Images,
        };
        let html = render_other_item(Lang::En, "USD", &m).to_string();
        assert!(html.contains(r#"action="/admin/models/save""#), "{html}");
        assert!(
            html.contains(r#"name="model_name" value="gpt-image-1""#),
            "{html}"
        );
        assert!(html.contains(r#"name="input_price""#), "{html}");
        assert!(
            !html.contains(r#"name="reasoning_style""#),
            "no reasoning: {html}"
        );
        assert!(html.contains(r#"name="price_only" value="1""#), "{html}");
    }

    #[test]
    fn stored_token_unit_is_not_coerced_for_image_models() {
        assert_eq!(
            displayed_pricing_unit("image", Some(db::PricingUnit::Tokens)),
            db::PricingUnit::Tokens
        );
    }

    #[test]
    fn pricing_unit_labels_are_localized() {
        assert_eq!(
            pricing_unit_label(Lang::De, db::PricingUnit::Tokens),
            "1 Mio. Tokens"
        );
        assert_eq!(
            pricing_unit_label(Lang::En, db::PricingUnit::Seconds),
            "second"
        );
    }

    /// A configured model shows its facet badges; an unconfigured one shows the
    /// dim "not configured".
    #[test]
    fn configured_badges_reflect_stored_facets() {
        let mut r = chat_row("m");
        r.input_price = Some(1.0);
        r.context_window = Some(1000);
        let html = configured_badges(Lang::En, &r).to_string();
        assert!(html.contains("PRICE") || html.contains(&t(Lang::En, "admin-badge-price")));
        let empty = configured_badges(Lang::En, &chat_row("m2")).to_string();
        assert!(
            empty.contains(&t(Lang::En, "admin-not-configured")),
            "{empty}"
        );
    }

    /// The alias row is dimmed, non-expandable, and carries no editor/endpoint.
    #[test]
    fn alias_row_is_readonly() {
        let a = AliasRow {
            name: "glm".into(),
            target: "glm-5.2".into(),
            kind_label: "chat",
        };
        let html = render_alias_item(Lang::En, &a).to_string();
        assert!(
            !html.contains("/admin/models/save"),
            "no save endpoint: {html}"
        );
        assert!(html.contains("glm-5.2"), "shows target: {html}");
    }
}
