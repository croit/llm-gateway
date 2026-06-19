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
use session_core::chrome::{Theme, is_datastar_request};
use session_core::icons;

use crate::rama_server::state::RamaState;
use crate::server::upstreams::{PickerStrategy, PoolKind};

/// GET /admin/backends — one card per pool, each listing its backends
/// with health, in-flight load, and advertised models. Pools are
/// sorted by name (the registry holds them in a `HashMap`, so without
/// this the card order would flap between renders).
pub async fn backends_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

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
                    }
                })
                .collect();
            PoolView {
                name: pool.name.clone(),
                kind: pool.kind,
                strategy: pool.strategy,
                backends,
            }
        })
        .collect();
    pools.sort_by(|a, b| a.name.cmp(&b.name));

    let body = render_backends_body(&pools);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        NavItem::Backends,
        "Upstream backends — LLM Gateway",
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
    backends: Vec<BackendView>,
}

struct BackendView {
    name: String,
    base_url: String,
    healthy: bool,
    inflight: u32,
    max_inflight: u32,
    models: Vec<String>,
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
    }
}

fn strategy_label(strategy: PickerStrategy) -> &'static str {
    match strategy {
        PickerStrategy::RoundRobin => "round_robin",
        PickerStrategy::LeastInflight => "least_inflight",
    }
}

fn render_backends_body(pools: &[PoolView]) -> Html {
    let total: usize = pools.iter().map(|p| p.backends.len()).sum();
    let healthy = pools
        .iter()
        .flat_map(|p| &p.backends)
        .filter(|b| b.healthy)
        .count();
    let down = total - healthy;
    let summary = format!("{total} backends · {healthy} healthy · {down} down");

    let cards: Vec<Html> = pools.iter().map(render_pool_card).collect();
    html! {
        section(class: "max-w-5xl mx-auto p-4 sm:p-6 flex flex-col gap-4") {
            header(class: "flex flex-col gap-1") {
                h1(class: "text-2xl font-bold") { "Upstream backends" }
                p(class: "text-base-content/70 text-sm") {
                    "Live view of the configured upstream pools — health, in-flight load \
                     against each backend's cap, and the models each one currently \
                     advertises. Read-only: routing is driven entirely by what the \
                     backends report on their "
                    code(class: "text-xs") { "/models" }
                    " probe."
                }
                if total > 0 {
                    p(class: "text-base-content/60 text-sm tabular-nums") { (summary) }
                }
            }
            if pools.is_empty() {
                div(class: "alert") {
                    (icons::info(18))
                    span {
                        "No upstream pools configured. Add an "
                        code(class: "text-xs") { "[upstream_pools.<name>]" }
                        " block to gateway.toml and restart."
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

fn render_pool_card(pool: &PoolView) -> Html {
    let rows: Vec<Html> = pool.backends.iter().map(render_backend_row).collect();
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                header(class: "flex items-center justify-between gap-3 flex-wrap") {
                    h2(class: "card-title text-base font-mono break-all") { (pool.name.clone()) }
                    div(class: "flex items-center gap-2") {
                        span(class: "badge badge-secondary") { (kind_label(pool.kind)) }
                        span(class: "badge badge-ghost font-mono") { (strategy_label(pool.strategy)) }
                    }
                }
                if pool.backends.is_empty() {
                    p(class: "text-base-content/60 text-sm") { "No backends in this pool." }
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

fn render_backend_row(b: &BackendView) -> Html {
    // One badge collapses health + saturation: down (probe failing) >
    // saturated (up but at cap) > up. The in-flight bar to the right
    // shows the load that drives the saturated state.
    let (status_class, status_label) = if !b.healthy {
        ("badge badge-error", "down")
    } else if b.saturated() {
        ("badge badge-warning", "saturated")
    } else {
        ("badge badge-success", "up")
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
                div(class: "flex items-center gap-2 shrink-0") {
                    span(class: "text-xs text-base-content/60 tabular-nums") {
                        "inflight " (load)
                    }
                    progress(
                        class: (bar_class),
                        value: (inflight),
                        max: (max_inflight)
                    ) {}
                }
            }
            div(class: "flex flex-wrap gap-1") {
                if models.is_empty() {
                    span(class: "text-xs text-base-content/50 italic") {
                        "no models advertised"
                    }
                } else {
                    for m in models.iter() {
                        span(class: "badge badge-ghost badge-sm font-mono") { (m.clone()) }
                    }
                }
            }
        }
    }
    .to_html()
}
