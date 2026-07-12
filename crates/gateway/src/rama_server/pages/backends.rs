// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/backends` — read-only operator view of the upstream pools.
//!
//! One card per pool (kind + picker strategy), and inside it one row
//! per backend: a health badge, the live in-flight load against the
//! backend's `max_inflight` cap, and the set of models it currently
//! advertises. Everything is a snapshot of the runtime state the
//! health probe in [`crate::server::upstreams::health`] maintains —
//! `is_healthy()`, `inflight()`, and `models_snapshot()` — so the page
//! is purely observational; there are no actions to take here.
//!
//! Gated on the `admin` role via [`super::require_admin_or_403`], same
//! as `/admin/models`. The sidebar entry is conditional on that role,
//! so non-admins never see the page exists.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};

use super::{
    NavItem, checkbox_on, fetch_sidebar_chat, field, is_admin, nav_or_html_page, parse_csv,
    parse_u32, read_form, require_admin_or_403, toast,
};
use session_core::chrome::{FlashKind, NavSections, Theme, is_datastar_request};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

use crate::rama_server::state::RamaState;
use crate::server::db::upstreams_config::{self, AliasRow, BackendRow};
use crate::server::db::usage;
use crate::server::upstreams::{AliasStatus, PickerStrategy, PoolKind};

/// Sparkline window: `BUCKETS` buckets of `BUCKET_MINUTES` each =
/// the last hour, in 5-minute steps.
const BUCKET_MINUTES: i64 = 5;
const BUCKETS: i64 = 12;

/// GET /admin/backends — one card per pool, each listing its backends
/// with health, in-flight load, and advertised models. Pools are
/// sorted by name (the registry holds them in a `HashMap`, so without
/// this the card order would flap between renders).
pub async fn backends_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    // Recent per-backend request activity for the sparkline (last hour in
    // 5-min buckets). Best-effort: a metrics hiccup just yields flat lines.
    let now = jiff::Timestamp::now();
    let rates = usage::recent_buckets_by_backend(&state.db, now, BUCKET_MINUTES, BUCKETS)
        .await
        .unwrap_or_default();

    let mut pools: Vec<PoolView> = state
        .upstreams
        .pools()
        .into_iter()
        .map(|pool| {
            let backends = pool
                .backends
                .iter()
                .map(|b| {
                    let mut models: Vec<String> = b.models_snapshot().into_iter().collect();
                    models.sort();
                    BackendView {
                        name: b.name.clone(),
                        base_url: b.base_url.clone(),
                        healthy: b.is_healthy(),
                        inflight: b.inflight(),
                        max_inflight: b.max_inflight,
                        models,
                        aliases: b.alias_status(),
                        recent: rates
                            .get(&b.name)
                            .cloned()
                            .unwrap_or_else(|| vec![0; BUCKETS as usize]),
                    }
                })
                .collect();
            PoolView {
                name: pool.name.clone(),
                kind: pool.kind,
                strategy: pool.strategy,
                fallback_offline: pool.fallback_offline().map(str::to_string),
                backends,
            }
        })
        .collect();
    pools.sort_by(|a, b| a.name.cmp(&b.name));

    // Global unknown-model fallbacks (`[fallback]`), per kind — shown once in
    // the page header since they're not pool-scoped.
    let unknown_fallbacks: Vec<(&'static str, String)> = [
        PoolKind::Chat,
        PoolKind::Transcription,
        PoolKind::Embedding,
        PoolKind::Image,
    ]
    .into_iter()
    .filter_map(|k| {
        state
            .upstreams
            .fallback_model(k)
            .map(|m| (kind_label(k), m.to_string()))
    })
    .collect();

    // DB-backed topology for the CRUD editor (distinct from the runtime health
    // view above): the rows the admin edits, which take effect on "Apply
    // changes" (POST /admin/upstreams/reload).
    let mut db_backends = upstreams_config::list_backends(&state.db)
        .await
        .unwrap_or_default();
    db_backends.sort_by(|a, b| a.name.cmp(&b.name));

    let body = render_backends_body(lang, &pools, &unknown_fallbacks, &db_backends);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        lang,
        nav,
        NavItem::Backends,
        &t(lang, "backends-page-title"),
        &user.email,
        is_admin(&state, &user),
        session.impersonator_id.is_some(),
        body,
        "/admin/backends",
        &chat,
    )
}

struct PoolView {
    name: String,
    kind: PoolKind,
    strategy: PickerStrategy,
    /// The pool's `fallback_offline` backup model, if configured.
    fallback_offline: Option<String>,
    backends: Vec<BackendView>,
}

struct BackendView {
    name: String,
    base_url: String,
    healthy: bool,
    inflight: u32,
    max_inflight: u32,
    models: Vec<String>,
    /// Configured aliases (client-facing names) + their live state.
    aliases: Vec<AliasStatus>,
    /// Request counts per 5-min bucket over the last hour, oldest → newest
    /// (always `BUCKETS` long; all-zero when idle).
    recent: Vec<i64>,
}

impl BackendView {
    /// A healthy backend that's at its in-flight cap still rejects new
    /// requests with a 503 until a slot frees — surface that as its own
    /// state so "up but can't take work" doesn't read as "up".
    fn saturated(&self) -> bool {
        self.healthy && self.inflight >= self.max_inflight
    }
}

/// Snake-case labels matching the TOML the operator wrote in
/// `gateway.toml` — `kind = "chat"`, `strategy = "least_inflight"` —
/// so what's on screen lines up with what's in the config file.
fn kind_label(kind: PoolKind) -> &'static str {
    match kind {
        PoolKind::Chat => "chat",
        PoolKind::Transcription => "transcription",
        PoolKind::Embedding => "embedding",
        PoolKind::Image => "image",
        PoolKind::Speech => "speech",
    }
}

fn strategy_label(strategy: PickerStrategy) -> &'static str {
    match strategy {
        PickerStrategy::RoundRobin => "round_robin",
        PickerStrategy::LeastInflight => "least_inflight",
    }
}

fn render_backends_body(
    lang: Lang,
    pools: &[PoolView],
    unknown_fallbacks: &[(&'static str, String)],
    db_backends: &[BackendRow],
) -> Html {
    let total: usize = pools.iter().map(|p| p.backends.len()).sum();
    let healthy = pools
        .iter()
        .flat_map(|p| &p.backends)
        .filter(|b| b.healthy)
        .count();
    let down = total - healthy;
    let summary = t_args(
        lang,
        "backends-summary",
        &i18n::args([
            ("total", total.to_string().into()),
            ("healthy", healthy.to_string().into()),
            ("down", down.to_string().into()),
        ]),
    );
    let unknown_fallback_line = if unknown_fallbacks.is_empty() {
        None
    } else {
        Some(
            unknown_fallbacks
                .iter()
                .map(|(kind, model)| format!("{kind} → {model}"))
                .collect::<Vec<_>>()
                .join(" · "),
        )
    };

    let cards: Vec<Html> = pools.iter().map(|p| render_pool_card(lang, p)).collect();
    html! {
        section(class: "max-w-5xl mx-auto p-4 sm:p-6 flex flex-col gap-4") {
            header(class: "flex flex-col gap-1") {
                h1(class: "text-2xl font-bold") { (t(lang, "backends-heading")) }
                p(class: "text-base-content/70 text-sm") {
                    (t(lang, "backends-description-prefix"))
                    " "
                    code(class: "text-xs") { "/models" }
                    " "
                    (t(lang, "backends-description-suffix"))
                }
                if total > 0 {
                    p(class: "text-base-content/60 text-sm tabular-nums") { (summary) }
                }
                if let Some(line) = unknown_fallback_line.as_deref() {
                    p(class: "text-base-content/60 text-sm") {
                        (t(lang, "backends-unknown-fallback-prefix"))
                        " "
                        span(class: "font-mono") { (line.to_string()) }
                    }
                }
            }
            if pools.is_empty() {
                div(class: "alert") {
                    (icons::info(18))
                    span {
                        (t(lang, "backends-empty-prefix"))
                        " "
                        code(class: "text-xs") { "[upstream_pools.<name>]" }
                        " "
                        (t(lang, "backends-empty-suffix"))
                    }
                }
            } else {
                div(class: "flex flex-col gap-4") {
                    for c in cards.iter() {
                        (c.clone())
                    }
                }
            }
            (render_backends_admin(lang, db_backends))
        }
    }
    .to_html()
}

/// The DB-backed backend editor: an "Apply changes" reload button, an "Add
/// backend" form, and one edit/delete form per stored backend. Edits write to
/// the DB (`upstreams_config`) but only take effect on the registry once the
/// admin clicks "Apply changes" — so every save toast reminds them to.
fn render_backends_admin(lang: Lang, db_backends: &[BackendRow]) -> Html {
    let reload = "@post('/admin/upstreams/reload')";
    let cards: Vec<Html> = db_backends
        .iter()
        .map(|b| render_backend_form(lang, Some(b)))
        .collect();
    html! {
        section(class: "flex flex-col gap-4 mt-6") {
            header(class: "flex items-center justify-between gap-3 flex-wrap") {
                div(class: "flex flex-col gap-1") {
                    h2(class: "text-xl font-bold") { (t(lang, "backends-manage-heading")) }
                    p(class: "text-base-content/70 text-sm") { (t(lang, "backends-manage-description")) }
                }
                button(class: "btn btn-warning btn-sm", "data-on:click": (reload)) {
                    (icons::check(14))
                    span { (t(lang, "backends-apply-changes")) }
                }
            }
            (render_backend_form(lang, None))
            if !cards.is_empty() {
                div(class: "flex flex-col gap-4") {
                    for c in cards.iter() {
                        (c.clone())
                    }
                }
            }
        }
    }
    .to_html()
}

/// Editor form for one backend — empty (`existing = None`) for "Add backend",
/// pre-filled for an existing row. The `name` is the primary key, so it is
/// read-only when editing (rename = delete + re-add). Submits to
/// `/admin/backends/save` (upsert); an existing row also gets a delete form.
fn render_backend_form(lang: Lang, existing: Option<&BackendRow>) -> Html {
    let action = "/admin/backends/save";
    let post = format!("@post('{action}', {{contentType: 'form'}})");
    let is_edit = existing.is_some();
    let name = existing.map(|b| b.name.clone()).unwrap_or_default();
    let base_url = existing.map(|b| b.base_url.clone()).unwrap_or_default();
    let api_key_env = existing
        .and_then(|b| b.api_key_env.clone())
        .unwrap_or_default();
    // Whether a sealed key is already stored — drives the field placeholder. The
    // secret itself is never sent back to the browser.
    let has_key = existing.map(|b| b.api_key_ct.is_some()).unwrap_or(false);
    let key_placeholder = if has_key {
        t(lang, "backends-field-api-key-keep")
    } else {
        t(lang, "backends-field-api-key-placeholder")
    };
    let weight = existing.map(|b| b.weight).unwrap_or(1).to_string();
    let max_inflight = existing.map(|b| b.max_inflight).unwrap_or(16).to_string();
    let health_path = existing
        .map(|b| b.health_path.clone())
        .unwrap_or_else(|| "/models".to_string());
    let probe = existing.map(|b| b.probe_models).unwrap_or(true);
    let supports_edit = existing.map(|b| b.supports_edit).unwrap_or(false);
    let models = existing.map(|b| b.models.join(", ")).unwrap_or_default();
    let aliases = existing
        .map(|b| alias_lines(&b.aliases))
        .unwrap_or_default();
    let title = if is_edit {
        name.clone()
    } else {
        t(lang, "backends-add-heading")
    };
    // `readonly`/`checked` are presence-based (see `super::select_option`), so
    // these go through standalone helpers that emit the attribute only when set.
    let name_field = super::pk_name_input(&name, "gpu-01", is_edit);
    let probe_box = super::bool_checkbox(
        "probe_models",
        "on",
        &t(lang, "backends-field-probe-models"),
        probe,
        false,
    );
    let edit_box = super::bool_checkbox(
        "supports_edit",
        "on",
        &t(lang, "backends-field-supports-edit"),
        supports_edit,
        false,
    );
    let delete_form = is_edit.then(|| {
        super::render_delete_form(
            "/admin/backends/delete",
            &name,
            &t(lang, "backends-delete-backend"),
        )
    });
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                h3(class: "card-title text-base font-mono break-all") { (title) }
                form(
                    method: "post",
                    action: (action),
                    "data-on:submit__prevent": (post.clone()),
                    class: "flex flex-col gap-3 m-0"
                ) {
                    div(class: "grid grid-cols-1 sm:grid-cols-2 gap-3") {
                        label(class: "form-control gap-1") {
                            span(class: "text-xs opacity-70") { (t(lang, "backends-field-name")) }
                            (name_field)
                        }
                        label(class: "form-control gap-1") {
                            span(class: "text-xs opacity-70") { (t(lang, "backends-field-base-url")) }
                            input(
                                type: "text", name: "base_url", value: (base_url),
                                class: "input input-bordered input-sm font-mono",
                                required: "required",
                                placeholder: "http://gpu-01:8000/v1"
                            );
                        }
                        label(class: "form-control gap-1") {
                            span(class: "text-xs opacity-70") { (t(lang, "backends-field-api-key")) }
                            input(
                                type: "password", name: "api_key", value: "",
                                autocomplete: "off",
                                class: "input input-bordered input-sm font-mono",
                                placeholder: (key_placeholder)
                            );
                        }
                        label(class: "form-control gap-1") {
                            span(class: "text-xs opacity-70") { (t(lang, "backends-field-api-key-env")) }
                            input(
                                type: "text", name: "api_key_env", value: (api_key_env),
                                class: "input input-bordered input-sm font-mono",
                                placeholder: "GPU01_KEY"
                            );
                        }
                        label(class: "form-control gap-1") {
                            span(class: "text-xs opacity-70") { (t(lang, "backends-field-health-path")) }
                            input(
                                type: "text", name: "health_path", value: (health_path),
                                class: "input input-bordered input-sm font-mono",
                                placeholder: "/models"
                            );
                        }
                        label(class: "form-control gap-1") {
                            span(class: "text-xs opacity-70") { (t(lang, "backends-field-weight")) }
                            input(
                                type: "number", name: "weight", value: (weight), min: "1",
                                class: "input input-bordered input-sm"
                            );
                        }
                        label(class: "form-control gap-1") {
                            span(class: "text-xs opacity-70") { (t(lang, "backends-field-max-inflight")) }
                            input(
                                type: "number", name: "max_inflight", value: (max_inflight), min: "1",
                                class: "input input-bordered input-sm"
                            );
                        }
                    }
                    label(class: "form-control gap-1") {
                        span(class: "text-xs opacity-70") { (t(lang, "backends-field-models")) }
                        input(
                            type: "text", name: "models", value: (models),
                            class: "input input-bordered input-sm font-mono",
                            placeholder: "qwen-32b, qwen-7b"
                        );
                    }
                    label(class: "form-control gap-1") {
                        span(class: "text-xs opacity-70") { (t(lang, "backends-field-aliases")) }
                        textarea(
                            name: "aliases",
                            class: "textarea textarea-bordered textarea-sm font-mono",
                            rows: "2",
                            placeholder: "fast=qwen-7b\nsmart=qwen-32b"
                        ) { (aliases) }
                    }
                    div(class: "flex flex-wrap gap-4") {
                        (probe_box)
                        (edit_box)
                    }
                    div(class: "flex justify-end") {
                        button(type: "submit", class: "btn btn-primary btn-sm") {
                            (icons::check(14))
                            span { (t(lang, if is_edit { "backends-save-backend" } else { "backends-add-backend" })) }
                        }
                    }
                }
                if let Some(df) = delete_form.as_ref() {
                    (df.clone())
                }
            }
        }
    }
    .to_html()
}

/// Serialise a backend's aliases back into the textarea format
/// (`name=target` per line, or a bare `name` for a target-less alias).
fn alias_lines(aliases: &[AliasRow]) -> String {
    aliases
        .iter()
        .map(|a| match &a.target {
            Some(t) => format!("{}={t}", a.alias),
            None => a.alias.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_pool_card(lang: Lang, pool: &PoolView) -> Html {
    let rows: Vec<Html> = pool
        .backends
        .iter()
        .map(|b| render_backend_row(lang, b))
        .collect();
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                header(class: "flex items-center justify-between gap-3 flex-wrap") {
                    h2(class: "card-title text-base font-mono break-all") { (pool.name.clone()) }
                    div(class: "flex items-center gap-2 flex-wrap") {
                        span(class: "badge badge-secondary") { (kind_label(pool.kind)) }
                        span(class: "badge badge-ghost font-mono") { (strategy_label(pool.strategy)) }
                        if let Some(model) = pool.fallback_offline.as_deref() {
                            span(
                                class: "badge badge-warning badge-outline font-mono",
                                title: (t(lang, "backends-fallback-offline-title"))
                            ) {
                                (t_args(
                                    lang,
                                    "backends-fallback-offline-badge",
                                    &i18n::args([("model", model.to_string().into())])
                                ))
                            }
                        }
                    }
                }
                if pool.backends.is_empty() {
                    p(class: "text-base-content/60 text-sm") { (t(lang, "backends-pool-empty")) }
                } else {
                    div(class: "flex flex-col gap-2") {
                        for r in rows.iter() {
                            (r.clone())
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

fn render_backend_row(lang: Lang, b: &BackendView) -> Html {
    // One badge collapses health + saturation: down (probe failing) >
    // saturated (up but at cap) > up. The in-flight bar to the right
    // shows the load that drives the saturated state.
    let (status_class, status_label) = if !b.healthy {
        ("badge badge-error", t(lang, "backends-status-down"))
    } else if b.saturated() {
        ("badge badge-warning", t(lang, "backends-status-saturated"))
    } else {
        ("badge badge-success", t(lang, "backends-status-up"))
    };
    let load = format!("{} / {}", b.inflight, b.max_inflight);
    let bar_class = if b.saturated() {
        "progress progress-warning w-24"
    } else {
        "progress progress-primary w-24"
    };
    let inflight = b.inflight.to_string();
    let max_inflight = b.max_inflight.to_string();
    let models = b.models.clone();
    // Recent activity: last 3 / 6 / 12 five-minute buckets = 15 / 30 / 60 min.
    let tail = |n: usize| -> i64 { b.recent.iter().rev().take(n).sum() };
    let c15 = tail(3);
    let c30 = tail(6);
    let c60: i64 = b.recent.iter().sum();
    let spark = sparkline_svg(&b.recent);
    // Alias chips: map form shows "name → target"; a bare alias disabled by
    // multi-model ambiguity is flagged; an active bare alias shows just its name.
    let aliases: Vec<(String, &'static str, String)> = b
        .aliases
        .iter()
        .map(|a| match (&a.target, a.disabled) {
            (Some(target), _) => (
                format!("{} → {target}", a.name),
                "badge badge-info badge-sm font-mono",
                t_args(
                    lang,
                    "backends-alias-target-title",
                    &i18n::args([("target", target.to_string().into())]),
                ),
            ),
            (None, true) => (
                t_args(
                    lang,
                    "backends-alias-disabled-label",
                    &i18n::args([("name", a.name.clone().into())]),
                ),
                "badge badge-warning badge-sm font-mono",
                t(lang, "backends-alias-disabled-title"),
            ),
            (None, false) => (
                a.name.clone(),
                "badge badge-info badge-sm font-mono",
                t(lang, "backends-alias-bare-title"),
            ),
        })
        .collect();
    html! {
        div(class: "flex flex-col gap-2 rounded-lg border border-base-300 p-3") {
            div(class: "flex items-center justify-between gap-3 flex-wrap") {
                div(class: "flex items-center gap-2 min-w-0") {
                    span(class: (status_class)) { (status_label) }
                    div(class: "min-w-0") {
                        div(class: "text-sm font-medium font-mono break-all") { (b.name.clone()) }
                        div(class: "text-xs text-base-content/60 font-mono break-all") {
                            (b.base_url.clone())
                        }
                    }
                }
                // Inflight bar, and directly underneath it the recent
                // request rate: a 1-hour sparkline (5-min buckets) + the
                // 15/30/60-minute totals. Right-aligned to sit under the bar.
                div(class: "flex flex-col items-end gap-1 shrink-0") {
                    div(class: "flex items-center gap-2") {
                        span(class: "text-xs text-base-content/60 tabular-nums") {
                            (t_args(lang, "backends-inflight-label", &i18n::args([("load", load.clone().into())])))
                        }
                        progress(
                            class: (bar_class),
                            value: (inflight),
                            max: (max_inflight)
                        ) {}
                    }
                    div(class: "flex items-center gap-2 text-base-content/50") {
                        span(class: "text-xs tabular-nums whitespace-nowrap") {
                            (t_args(
                                lang,
                                "backends-activity-summary",
                                &i18n::args([
                                    ("m15", c15.to_string().into()),
                                    ("m30", c30.to_string().into()),
                                    ("m60", c60.to_string().into()),
                                ])
                            ))
                        }
                        span(class: "text-primary") { #(spark.clone()) }
                    }
                }
            }
            div(class: "flex flex-wrap gap-1") {
                if models.is_empty() {
                    span(class: "text-xs text-base-content/50 italic") {
                        (t(lang, "backends-no-models"))
                    }
                } else {
                    for m in models.iter() {
                        span(class: "badge badge-ghost badge-sm font-mono") { (m.clone()) }
                    }
                }
            }
            if !aliases.is_empty() {
                div(class: "flex flex-wrap gap-1 items-center") {
                    span(class: "text-xs text-base-content/50") { (t(lang, "backends-aliases-label")) }
                    for (label, class, title) in aliases.iter() {
                        span(class: (*class), title: (title.clone())) { (label.clone()) }
                    }
                }
            }
        }
    }
    .to_html()
}

/// A tiny inline-SVG bar sparkline of `values` (oldest → newest), inheriting
/// the surrounding text color via `currentColor`. Bars are normalised to the
/// window's own max; idle buckets render as faint stubs so an all-zero series
/// still reads as a flat baseline rather than a blank gap. No JS, no chart
/// library — same self-contained-SVG approach as `icons`.
fn sparkline_svg(values: &[i64]) -> String {
    const BAR_W: i64 = 6;
    const GAP: i64 = 2;
    const H: i64 = 20;
    let n = values.len().max(1) as i64;
    let width = n * (BAR_W + GAP) - GAP;
    let max = values.iter().copied().max().unwrap_or(0).max(1);
    let mut bars = String::new();
    for (i, &v) in values.iter().enumerate() {
        let bh = if v <= 0 {
            2
        } else {
            (((v as f64 / max as f64) * (H as f64 - 2.0)).round() as i64).clamp(2, H)
        };
        let x = i as i64 * (BAR_W + GAP);
        let y = H - bh;
        let opacity = if v <= 0 { "0.2" } else { "0.85" };
        bars.push_str(&format!(
            "<rect x=\"{x}\" y=\"{y}\" width=\"{BAR_W}\" height=\"{bh}\" rx=\"1\" \
             fill=\"currentColor\" opacity=\"{opacity}\"/>"
        ));
    }
    format!(
        "<svg width=\"{width}\" height=\"{H}\" viewBox=\"0 0 {width} {H}\" fill=\"none\" \
         class=\"inline-block shrink-0 align-middle\" aria-hidden=\"true\">{bars}</svg>"
    )
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
    match upstreams_config::upsert_backend(&state.db, &row).await {
        Ok(()) => toast(
            FlashKind::Success,
            t_args(
                lang,
                "backends-saved",
                &i18n::args([("name", name.to_string().into())]),
            ),
        ),
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
        Ok(()) => toast(
            FlashKind::Success,
            t_args(
                lang,
                "backends-deleted",
                &i18n::args([("name", name.to_string().into())]),
            ),
        ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;

    fn sample_backend() -> BackendRow {
        BackendRow {
            name: "gpu-01".into(),
            base_url: "http://gpu-01:8000/v1".into(),
            api_key_env: Some("GPU01_KEY".into()),
            api_key_ct: None,
            api_key_nonce: None,
            weight: 2,
            max_inflight: 32,
            health_path: "/models".into(),
            probe_models: true,
            supports_edit: false,
            models: vec!["qwen-32b".into(), "qwen-7b".into()],
            aliases: vec![
                AliasRow {
                    alias: "fast".into(),
                    target: Some("qwen-7b".into()),
                },
                AliasRow {
                    alias: "bare".into(),
                    target: None,
                },
            ],
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        }
    }

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

    /// The "Add backend" form must post to the save route via datastar and label
    /// its submit as an add — the UI-directive ↔ endpoint contract.
    #[test]
    fn add_form_wires_to_save_endpoint() {
        let html = render_backend_form(Lang::En, None).to_string();
        assert!(
            html.contains(r#"action="/admin/backends/save""#),
            "form must post to the save route: {html}"
        );
        assert!(
            html.contains("@post(") && html.contains("/admin/backends/save"),
            "submit must datastar-post to the save route: {html}"
        );
        // A fresh form has no delete affordance.
        assert!(
            !html.contains("/admin/backends/delete"),
            "add form must not render a delete form: {html}"
        );
    }

    /// An edit form pre-fills values, locks the primary-key `name`, reflects the
    /// stored checkbox state, and offers a delete posting to the delete route.
    #[test]
    fn edit_form_prefills_locks_name_and_offers_delete() {
        let b = sample_backend();
        let html = render_backend_form(Lang::En, Some(&b)).to_string();
        assert!(
            html.contains(r#"name="name" value="gpu-01""#),
            "name must be pre-filled: {html}"
        );
        assert!(
            html.contains("readonly=\"readonly\""),
            "name must be read-only on edit: {html}"
        );
        assert!(
            html.contains(r#"name="probe_models""#) && html.contains("checked=\"checked\""),
            "probe_models is true → checkbox checked: {html}"
        );
        assert!(
            html.contains(r#"action="/admin/backends/delete""#),
            "edit form must offer a delete: {html}"
        );
        assert!(
            html.contains("qwen-32b, qwen-7b"),
            "models must be comma-joined: {html}"
        );
    }
}
