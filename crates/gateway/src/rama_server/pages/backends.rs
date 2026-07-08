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

use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_admin_or_403};
use session_core::chrome::{NavSections, Theme, is_datastar_request};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

use crate::rama_server::state::RamaState;
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

    let body = render_backends_body(lang, &pools, &unknown_fallbacks);
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
        }
    }
    .to_html()
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
