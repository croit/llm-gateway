// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/upstreams` — the merged pools + backends operator page.
//!
//! Replaces the old `/admin/pools` (topology CRUD) and `/admin/backends`
//! (read-only health) pages with one surface:
//!
//!   - One card per pool: a header (name, kind + strategy badges, an optional
//!     "offline → model" warning, and GDPR/NDA/limits compliance indicators),
//!     then one **live health row** per assigned backend (status badge,
//!     base URL, advertised models + alias chips, in-flight bar, 15/30/60-minute
//!     request counts + a sparkline). Each health row is a `<summary>` that
//!     expands an inline backend editor; a separate "✎ Edit pool" `<details>`
//!     holds the pool editor.
//!   - An "Unassigned" group card for backends not in any pool.
//!   - The global unknown-model fallback editor (auto-saving selects).
//!   - Add-pool / Add-backend forms revealed by the header buttons (datastar
//!     `$addForm` signal — only one open at a time).
//!
//! Topology edits are written to the DB by the `pools_*` / `backends_*` POST
//! handlers (in [`super::pools`] / [`super::backends`], paths unchanged) but the
//! runtime registry only picks them up on "Apply changes" (POST
//! `/admin/upstreams/reload`). The drift between the two is tracked in memory
//! ([`RamaState::topology_dirty_count`]) and surfaced as a sticky amber apply
//! bar, kept live via the `topologyDirty` datastar signal.
//!
//! The old GET routes 302-redirect here (see [`pools_redirect`] /
//! [`backends_redirect`]). Gated on the `admin` role via
//! [`super::require_admin_or_403`], same as the other operator pages.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response, StatusCode, header};

use super::pools::{KINDS, STRATEGIES};
use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_admin_or_403};
use session_core::chrome::{NavSections, Theme, is_datastar_request};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

use gateway_core::server::db::upstreams_config::{self, BackendRow, PoolRow};
use gateway_core::server::db::usage;
use gateway_core::server::upstreams::AliasStatus;
use gateway_runtime::rama_server::state::RamaState;

/// Sparkline window: `BUCKETS` buckets of `BUCKET_MINUTES` each = the last hour
/// in 5-minute steps (same as the old `/admin/backends` view).
const BUCKET_MINUTES: i64 = 5;
const BUCKETS: i64 = 12;

/// GET /admin/upstreams — the merged pools + backends page.
pub async fn upstreams_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    // DB topology (what the admin edits): pools + backends + fallbacks.
    let snapshot = upstreams_config::load_snapshot(&state.db)
        .await
        .unwrap_or_default();
    let mut pools = snapshot.pools.clone();
    pools.sort_by(|a, b| a.sort_order.cmp(&b.sort_order).then(a.name.cmp(&b.name)));
    let mut backend_names: Vec<String> = snapshot.backends.keys().cloned().collect();
    backend_names.sort();
    let all_models = state.upstreams.all_models();

    // Runtime health, keyed by backend name (what the gateway currently serves).
    let health = live_health(&state).await;
    let dirty = state.topology_dirty_count();

    let body = render_body(
        lang,
        &pools,
        &snapshot.backends,
        &snapshot.fallbacks,
        &backend_names,
        &all_models,
        &health,
        dirty,
    );
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
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
        nav_or_html_page(
            &pctx,
            NavItem::Upstreams,
            &t(lang, "upstreams-page-title"),
            body,
            "/admin/upstreams",
            &chat,
        )
    }
}

/// GET /admin/pools and GET /admin/backends — the two pages merged into
/// `/admin/upstreams`; permanently redirect stale bookmarks there. 302 (temporary
/// per the API contract) so a client re-issues the GET against the new path.
pub async fn pools_redirect(_state: State<Arc<RamaState>>, _req: Request) -> Response {
    redirect_302("/admin/upstreams")
}

/// See [`pools_redirect`].
pub async fn backends_redirect(_state: State<Arc<RamaState>>, _req: Request) -> Response {
    redirect_302("/admin/upstreams")
}

fn redirect_302(to: &str) -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, to)
        .body("".into())
        .unwrap()
}

// ---------------------------------------------------------------------------
// Runtime health snapshot
// ---------------------------------------------------------------------------

/// Live per-backend runtime state, keyed by backend name. Sourced from the
/// health probe the registry maintains (`is_healthy`, `inflight`,
/// `models_snapshot`, `alias_status`) plus the recent per-backend request rate.
struct BackendHealth {
    healthy: bool,
    saturated: bool,
    inflight: u32,
    max_inflight: u32,
    /// Models the backend actually serves (effective set: probe restricted by
    /// the pool/backend allowlist, or the whole probe when no list is set).
    served: Vec<String>,
    /// Models the backend reports via `/models` but the allowlist withholds —
    /// shown struck-through so the operator sees what the endpoint offers vs.
    /// what this pool actually exposes. Empty unless a list is filtering.
    withheld: Vec<String>,
    aliases: Vec<AliasStatus>,
    /// Request counts per 5-min bucket over the last hour, oldest → newest.
    recent: Vec<i64>,
}

async fn live_health(state: &RamaState) -> HashMap<String, BackendHealth> {
    let now = jiff::Timestamp::now();
    let rates = usage::recent_buckets_by_backend(&state.db, now, BUCKET_MINUTES, BUCKETS)
        .await
        .unwrap_or_default();
    let mut map: HashMap<String, BackendHealth> = HashMap::new();
    for pool in state.upstreams.pools() {
        for b in &pool.backends {
            let mut served: Vec<String> = b.models_snapshot().into_iter().collect();
            served.sort();
            let mut withheld: Vec<String> = b.withheld_models().into_iter().collect();
            withheld.sort();
            map.insert(
                b.name.clone(),
                BackendHealth {
                    healthy: b.is_healthy(),
                    saturated: b.is_healthy() && b.inflight() >= b.max_inflight,
                    inflight: b.inflight(),
                    max_inflight: b.max_inflight,
                    served,
                    withheld,
                    aliases: b.alias_status(),
                    recent: rates
                        .get(&b.name)
                        .cloned()
                        .unwrap_or_else(|| vec![0; BUCKETS as usize]),
                },
            );
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Page body
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_body(
    lang: Lang,
    pools: &[PoolRow],
    backends: &HashMap<String, BackendRow>,
    fallbacks: &HashMap<String, String>,
    backend_names: &[String],
    all_models: &[String],
    health: &HashMap<String, BackendHealth>,
    dirty: u32,
) -> Html {
    // Backends referenced by at least one pool → the rest go in "Unassigned".
    let assigned: HashSet<&str> = pools
        .iter()
        .flat_map(|p| p.backends.iter().map(String::as_str))
        .collect();
    let mut unassigned: Vec<&BackendRow> = backends
        .values()
        .filter(|b| !assigned.contains(b.name.as_str()))
        .collect();
    unassigned.sort_by(|a, b| a.name.cmp(&b.name));

    // Pool names for the backend editor's single "Pool" select.
    let pool_names: Vec<String> = pools.iter().map(|p| p.name.clone()).collect();

    let pool_cards: Vec<Html> = pools
        .iter()
        .enumerate()
        .map(|(i, p)| render_pool_card(lang, i, p, backends, backend_names, health, &pool_names))
        .collect();

    // Signals: `addForm` drives which add form is open (one at a time);
    // `topologyDirty` drives the apply bar; `addPoolKind` gates the speech-only
    // voices field in the add-pool form. Declared once on the container.
    let signals = format!("{{addForm: '', addPoolKind: 'chat', topologyDirty: {dirty}}}");

    html! {
        section(
            class: "max-w-5xl mx-auto p-4 sm:p-6 flex flex-col gap-4",
            "data-signals": (signals)
        ) {
            header(class: "flex items-start justify-between gap-3 flex-wrap") {
                div(class: "flex flex-col gap-1") {
                    h1(class: "text-2xl font-bold") { (t(lang, "upstreams-heading")) }
                    p(class: "text-base-content/70 text-sm max-w-2xl") {
                        (t(lang, "upstreams-description"))
                    }
                }
                div(class: "flex gap-2 shrink-0") {
                    button(
                        type: "button",
                        class: "btn btn-sm",
                        "data-on:click": "$addForm = 'pool'"
                    ) {
                        (icons::plus(14))
                        span { (t(lang, "upstreams-add-pool")) }
                    }
                    button(
                        type: "button",
                        class: "btn btn-sm",
                        "data-on:click": "$addForm = 'backend'"
                    ) {
                        (icons::plus(14))
                        span { (t(lang, "upstreams-add-backend")) }
                    }
                }
            }

            (render_apply_bar(lang))

            // Add forms (hidden until a header button reveals them).
            (render_add_pool_card(lang, backend_names))
            (render_add_backend_card(lang, &pool_names))

            if pool_cards.is_empty() && unassigned.is_empty() {
                div(class: "alert") {
                    (icons::info(18))
                    span { (t(lang, "upstreams-empty")) }
                }
            } else {
                div(class: "flex flex-col gap-4") {
                    for c in pool_cards.iter() { (c.clone()) }
                    if !unassigned.is_empty() {
                        (render_unassigned_card(lang, &unassigned, health, &pool_names))
                    }
                }
            }

            (render_fallbacks_card(lang, fallbacks, all_models))
        }
    }
    .to_html()
}

/// The sticky amber "N unapplied changes" bar. Visibility + counter bind to the
/// `topologyDirty` signal (initialised on the container, kept live by the
/// save/delete/reload responses), so it appears the moment an edit is saved and
/// clears itself when the registry is reloaded. `top`/`z-index` are inline
/// because the shipped CSS bundle carries no `top-0`/`z-30` utility.
fn render_apply_bar(lang: Lang) -> Html {
    let reload = "@post('/admin/upstreams/reload')";
    html! {
        div(
            class: "alert alert-warning sticky flex items-center gap-3",
            style: "top: 0.75rem; z-index: 30",
            role: "status",
            "data-show": "$topologyDirty > 0"
        ) {
            (icons::alert(18))
            span(class: "flex-1 text-sm") {
                strong { span("data-text": "$topologyDirty") {} " " (t(lang, "upstreams-apply-count")) }
                " "
                (t(lang, "upstreams-apply-note"))
            }
            button(class: "btn btn-sm", "data-on:click": (reload)) {
                (icons::check(14))
                span { (t(lang, "backends-apply-changes")) }
            }
        }
    }
    .to_html()
}

// ---------------------------------------------------------------------------
// Pool card (health rows + inline editors)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_pool_card(
    lang: Lang,
    idx: usize,
    pool: &PoolRow,
    backends: &HashMap<String, BackendRow>,
    backend_names: &[String],
    health: &HashMap<String, BackendHealth>,
    pool_names: &[String],
) -> Html {
    let offline_badge = pool.fallback_offline.as_deref().map(|m| {
        html! {
            span(
                class: "badge badge-warning badge-outline font-mono",
                title: (t(lang, "backends-fallback-offline-title"))
            ) {
                (t_args(
                    lang,
                    "backends-fallback-offline-badge",
                    &i18n::args([("model", m.to_string().into())])
                ))
            }
        }
        .to_html()
    });

    // One health row per assigned backend: a status block plus an "Edit
    // backend" toggle holding the inline editor. A DB backend not (yet) in the
    // runtime registry shows a "pending apply" status.
    let rows: Vec<Html> = pool
        .backends
        .iter()
        .enumerate()
        .filter_map(|(bi, bn)| {
            backends.get(bn).map(|row| {
                let sig = format!("dpb{idx}_{bi}");
                render_backend_details(
                    lang,
                    row,
                    health.get(bn.as_str()),
                    &sig,
                    Some(pool.name.as_str()),
                    pool_names,
                )
            })
        })
        .collect();

    let del_sig = format!("dp{idx}");
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                header(class: "flex items-center gap-2 flex-wrap") {
                    span(class: "font-mono font-bold text-base") { (pool.name.clone()) }
                    span(class: "badge badge-secondary") { (pool.kind.clone()) }
                    span(class: "badge badge-ghost font-mono") { (pool.strategy.clone()) }
                    if let Some(b) = offline_badge.as_ref() { (b.clone()) }
                    (compliance_indicators(lang, pool.compliance_gdpr, pool.compliance_nda, pool.enforce_limits))
                    span(class: "flex-1") {}
                    (two_step_delete(
                        "/admin/pools/delete", &pool.name,
                        &t(lang, "pools-delete-pool"), &t(lang, "upstreams-delete-confirm"), &del_sig
                    ))
                }
                if rows.is_empty() {
                    p(class: "text-base-content/60 text-sm") { (t(lang, "backends-pool-empty")) }
                } else {
                    div(class: "flex flex-col gap-2") {
                        for r in rows.iter() { (r.clone()) }
                    }
                }
                (render_pool_editor(lang, pool, backend_names))
            }
        }
    }
    .to_html()
}

/// GDPR / NDA / limits indicators for the pool header. ✓ (success) when the
/// flag is set, ✕ (error) when not — the same advisory signal the chat UI uses.
fn compliance_indicators(lang: Lang, gdpr: bool, nda: bool, enforce: bool) -> Html {
    let item = |on: bool, label: String| -> Html {
        let (mark, cls) = if on {
            ("\u{2713} ", "text-success")
        } else {
            ("\u{2717} ", "text-error")
        };
        html! { span(class: (cls)) { (mark.to_string()) (label) } }.to_html()
    };
    html! {
        span(class: "flex items-center gap-3 text-xs") {
            (item(gdpr, t(lang, "upstreams-comp-gdpr")))
            (item(nda, t(lang, "upstreams-comp-nda")))
            (item(enforce, t(lang, "upstreams-comp-limits")))
        }
    }
    .to_html()
}

/// One backend health row: a static status block (badge, base URL, models,
/// in-flight bar, activity + sparkline) followed by an explicit "Edit backend"
/// `<details>` toggle that reveals the backend editor form. `health = None`
/// renders a muted "pending apply" row (the backend is in the DB but not yet in
/// the runtime registry).
fn render_backend_details(
    lang: Lang,
    row: &BackendRow,
    health: Option<&BackendHealth>,
    del_sig: &str,
    current_pool: Option<&str>,
    pool_names: &[String],
) -> Html {
    let (status_class, status_label) = match health {
        None => ("badge badge-ghost", t(lang, "upstreams-backend-pending")),
        Some(h) if !h.healthy => ("badge badge-error", t(lang, "backends-status-down")),
        Some(h) if h.saturated => ("badge badge-warning", t(lang, "backends-status-saturated")),
        Some(_) => ("badge badge-success", t(lang, "backends-status-up")),
    };
    let inflight = health.map(|h| h.inflight).unwrap_or(0).to_string();
    let max_inflight = health
        .map(|h| h.max_inflight)
        .unwrap_or(row.max_inflight)
        .to_string();
    let load = format!("{inflight} / {max_inflight}");
    let bar_class = match health {
        Some(h) if h.saturated => "progress progress-warning w-24",
        _ => "progress progress-primary w-24",
    };
    let recent: Vec<i64> = health.map(|h| h.recent.clone()).unwrap_or_default();
    let tail = |n: usize| -> i64 { recent.iter().rev().take(n).sum() };
    let c15 = tail(3);
    let c30 = tail(6);
    let c60: i64 = recent.iter().sum();
    let spark = sparkline_svg(&recent);
    let served: Vec<String> = health.map(|h| h.served.clone()).unwrap_or_default();
    let withheld: Vec<String> = health.map(|h| h.withheld.clone()).unwrap_or_default();
    let withheld_title = t(lang, "upstreams-model-withheld-title");
    let aliases = health
        .map(|h| alias_chips(lang, &h.aliases))
        .unwrap_or_default();
    let base_url = row.base_url.clone();
    let name = row.name.clone();

    html! {
        div(class: "rounded-lg border border-base-300 bg-base-100") {
            // Live status row — a static display (was formerly the sole expand
            // affordance; the huge served-models list buried the disclosure).
            div(class: "px-3 py-2 flex flex-col gap-2") {
                div(class: "flex items-center justify-between gap-3 flex-wrap") {
                    div(class: "flex items-center gap-2 min-w-0") {
                        span(class: (status_class)) { (status_label) }
                        div(class: "min-w-0") {
                            div(class: "text-sm font-medium font-mono break-all") { (name) }
                            div(class: "text-xs text-base-content/60 font-mono break-all") { (base_url) }
                        }
                    }
                    div(class: "flex flex-col items-end gap-1 shrink-0") {
                        div(class: "flex items-center gap-2") {
                            span(class: "text-xs text-base-content/60 tabular-nums") {
                                (t_args(lang, "backends-inflight-label", &i18n::args([("load", load.clone().into())])))
                            }
                            progress(class: (bar_class), value: (inflight), max: (max_inflight)) {}
                        }
                        div(class: "flex items-center gap-2 text-base-content/50") {
                            span(class: "text-xs tabular-nums whitespace-nowrap") {
                                (t_args(
                                    lang, "backends-activity-summary",
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
                if !served.is_empty() || !withheld.is_empty() || !aliases.is_empty() {
                    (render_backend_model_chips(lang, del_sig, &served, &withheld, &aliases, &withheld_title))
                }
            }
            // Explicit "Edit backend" toggle (mirrors "Edit pool") so the
            // editor is discoverable regardless of how tall the status row is.
            details(class: "border-t border-base-300") {
                summary(class: "cursor-pointer select-none px-3 py-2 text-sm font-medium") {
                    (t(lang, "upstreams-edit-backend"))
                }
                div(class: "border-t border-base-300 p-3") {
                    (render_backend_form(lang, row, del_sig, current_pool, pool_names))
                }
            }
        }
    }
    .to_html()
}

/// The served/alias/withheld model chip row under a backend's status.
///
/// Active (served) models and alias chips are **always** shown in full. The
/// withheld ("inactive") set — models the endpoint advertises but the pool's
/// allowlist doesn't serve — can run to hundreds of entries (e.g. an OpenAI
/// backend), so it collapses behind a clickable `+N inactive` pill; clicking it
/// reveals the full struck-through list inline (and a "hide" pill to re-collapse
/// it). Toggled by a per-row datastar signal (`inact_<del_sig>`, unique because
/// `del_sig` is unique per rendered backend row) so several rows expand
/// independently, with no page reload. Isolated in this helper so the `html!`
/// `for`/attribute closures never capture the caller's locals (see the note on
/// `select_option`).
fn render_backend_model_chips(
    lang: Lang,
    del_sig: &str,
    served: &[String],
    withheld: &[String],
    aliases: &[Html],
    withheld_title: &str,
) -> Html {
    let sig = format!("inact_{del_sig}");
    let signals = format!("{{{sig}: false}}");
    let show = format!("${sig}");
    let hide = format!("!${sig}");
    let open = format!("${sig} = true");
    let close = format!("${sig} = false");
    let pill_label = t_args(
        lang,
        "upstreams-models-inactive-pill",
        &i18n::args([("count", withheld.len().to_string().into())]),
    );
    let hide_label = t(lang, "upstreams-models-inactive-hide");

    // Build every child up front via the standalone chip/pill helpers below
    // (they take `&str`, so the `html!` closures only capture Copy references —
    // moving the `String` locals into the macro's `FnMut` child closures here
    // would fail to compile, E0507).
    let mut children: Vec<Html> = served.iter().map(|m| served_chip(m)).collect();
    children.extend(aliases.iter().cloned());
    if !withheld.is_empty() {
        children.push(inactive_toggle_pill(
            &pill_label,
            &hide,
            &open,
            withheld_title,
        ));
        // Each withheld chip is gated by the row signal (data-show), so the
        // collapsed row is just the active chips + the pill.
        children.extend(
            withheld
                .iter()
                .map(|m| withheld_chip(m, &show, withheld_title)),
        );
        children.push(inactive_toggle_pill(&hide_label, &show, &close, ""));
    }

    html! {
        div(class: "flex flex-wrap gap-1 items-center", "data-signals": (signals)) {
            for c in children.iter() { (c.clone()) }
        }
    }
    .to_html()
}

/// A served (active) model chip. Standalone so the `html!` never captures a
/// caller local (see `select_option`).
fn served_chip(model: &str) -> Html {
    html! { span(class: "badge badge-ghost badge-sm font-mono") { (model.to_string()) } }.to_html()
}

/// A withheld (inactive) model chip — struck-through and muted, gated by the
/// row's collapse signal via `show_expr` (a datastar `data-show` expression).
fn withheld_chip(model: &str, show_expr: &str, title: &str) -> Html {
    html! {
        span(
            class: "badge badge-ghost badge-sm font-mono line-through opacity-50",
            "data-show": (show_expr.to_string()),
            title: (title.to_string())
        ) { (model.to_string()) }
    }
    .to_html()
}

/// A clickable pill toggling the withheld-models collapse: `show_expr` is the
/// datastar `data-show` guard (visible only in one state) and `click_expr`
/// flips the row signal. Used for both the `+N inactive` (collapsed) pill and
/// the "hide" (expanded) pill. `title` is optional (`""` = none).
fn inactive_toggle_pill(label: &str, show_expr: &str, click_expr: &str, title: &str) -> Html {
    html! {
        button(
            type: "button",
            class: "badge badge-ghost badge-sm cursor-pointer",
            "data-show": (show_expr.to_string()),
            "data-on:click": (click_expr.to_string()),
            title: (title.to_string())
        ) { (label.to_string()) }
    }
    .to_html()
}

/// Alias chips for a backend's live alias set (map form "name → target", a
/// disabled bare alias flagged, an active bare alias just named).
fn alias_chips(lang: Lang, aliases: &[AliasStatus]) -> Vec<Html> {
    aliases
        .iter()
        .map(|a| {
            let (label, class, title) = match (&a.target, a.disabled) {
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
            };
            html! { span(class: (class), title: (title)) { (label) } }.to_html()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Editors — pool + backend forms
// ---------------------------------------------------------------------------

/// The "✎ Edit pool" collapsible holding the pool editor form. Native
/// `<details>`; the `<summary>` is the toggle.
fn render_pool_editor(lang: Lang, pool: &PoolRow, backend_names: &[String]) -> Html {
    html! {
        details(class: "rounded-lg border border-base-300 bg-base-200/40") {
            summary(class: "cursor-pointer select-none px-3 py-2 text-sm font-medium") {
                (t(lang, "upstreams-edit-pool"))
            }
            div(class: "border-t border-base-300 p-3") {
                (render_pool_form(lang, Some(pool), pool.sort_order, backend_names, false))
            }
        }
    }
    .to_html()
}

/// The Add-pool card, hidden until `$addForm === 'pool'`.
fn render_add_pool_card(lang: Lang, backend_names: &[String]) -> Html {
    let next = backend_names.len() as i64; // fresh sort_order slot
    html! {
        article(
            class: "card border border-base-300 bg-base-200/40",
            "data-show": "$addForm === 'pool'"
        ) {
            div(class: "card-body gap-3") {
                h2(class: "card-title text-base") { (t(lang, "pools-add-heading")) }
                (render_pool_form(lang, None, next, backend_names, true))
            }
        }
    }
    .to_html()
}

/// The Add-backend card, hidden until `$addForm === 'backend'`.
fn render_add_backend_card(lang: Lang, pool_names: &[String]) -> Html {
    html! {
        article(
            class: "card border border-base-300 bg-base-200/40",
            "data-show": "$addForm === 'backend'"
        ) {
            div(class: "card-body gap-3") {
                h2(class: "card-title text-base") { (t(lang, "backends-add-heading")) }
                (render_backend_form_add(lang, pool_names))
            }
        }
    }
    .to_html()
}

/// Editor form for one pool — empty (`existing = None`) for "Add pool",
/// pre-filled otherwise. `name` is the primary key, so it is read-only when
/// editing. `is_add` gates the two add-only affordances: a Cancel that hides the
/// add card, and the speech-only voices field driven by the `addPoolKind`
/// signal (edit forms render the voices field only for speech pools).
fn render_pool_form(
    lang: Lang,
    existing: Option<&PoolRow>,
    sort_order: i64,
    backend_names: &[String],
    is_add: bool,
) -> Html {
    let action = "/admin/pools/save";
    let post = format!("@post('{action}', {{contentType: 'form'}})");
    let is_edit = existing.is_some();
    let name = existing.map(|p| p.name.clone()).unwrap_or_default();
    let kind = existing
        .map(|p| p.kind.clone())
        .unwrap_or_else(|| "chat".into());
    let strategy = existing
        .map(|p| p.strategy.clone())
        .unwrap_or_else(|| "least_inflight".into());
    let fallback_offline = existing
        .and_then(|p| p.fallback_offline.clone())
        .unwrap_or_default();
    let gdpr = existing.map(|p| p.compliance_gdpr).unwrap_or(true);
    let nda = existing.map(|p| p.compliance_nda).unwrap_or(true);
    let enforce = existing.map(|p| p.enforce_limits).unwrap_or(true);
    let models = existing.map(|p| p.models.join(", ")).unwrap_or_default();
    let allowed_groups = existing
        .map(|p| p.allowed_groups.join(", "))
        .unwrap_or_default();
    let voices = existing
        .map(|p| super::pools::voice_lines(&p.voices))
        .unwrap_or_default();
    let assigned: Vec<String> = existing.map(|p| p.backends.clone()).unwrap_or_default();
    let sort_order_str = sort_order.to_string();
    let is_speech = kind == "speech";

    let kind_opts = options_for(KINDS, &kind);
    let strategy_opts = options_for(STRATEGIES, &strategy);
    let name_field = super::pk_name_input(&name, "chat-eu", is_edit);
    let backend_boxes: Vec<Html> = backend_names
        .iter()
        .map(|bn| super::bool_checkbox("backends", bn, bn, assigned.iter().any(|a| a == bn), true))
        .collect();
    let gdpr_box = super::bool_checkbox(
        "compliance_gdpr",
        "on",
        &t(lang, "pools-field-gdpr"),
        gdpr,
        false,
    );
    let nda_box = super::bool_checkbox(
        "compliance_nda",
        "on",
        &t(lang, "pools-field-nda"),
        nda,
        false,
    );
    let enforce_box = super::bool_checkbox(
        "enforce_limits",
        "on",
        &t(lang, "pools-field-enforce-limits"),
        enforce,
        false,
    );
    // The kind select binds the add-pool signal so the voices field can appear
    // only for speech pools; edit forms have no signal (server decides).
    let kind_select = pool_kind_select(&kind_opts, is_add);
    let voices_field = render_pool_voices_field(lang, &voices, is_add, is_speech);
    let save_key = if is_edit {
        "pools-save-pool"
    } else {
        "pools-add-pool"
    };
    let cancel = is_add.then(|| pool_cancel_button(lang));

    html! {
        form(
            method: "post",
            action: (action),
            "data-on:submit__prevent": (post),
            class: "flex flex-col gap-3 m-0"
        ) {
            input(type: "hidden", name: "sort_order", value: (sort_order_str));
            div(class: "grid grid-cols-1 sm:grid-cols-3 gap-3") {
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "pools-field-name")) }
                    (name_field)
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "pools-field-kind")) }
                    (kind_select)
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "pools-field-strategy")) }
                    select(name: "strategy", class: "select select-bordered select-sm w-full") {
                        for o in strategy_opts.iter() { (o.clone()) }
                    }
                }
            }
            label(class: "flex flex-col gap-1") {
                span(class: "text-xs opacity-70") { (t(lang, "pools-field-fallback-offline")) }
                input(
                    type: "text", name: "fallback_offline", value: (fallback_offline),
                    class: "input input-bordered input-sm font-mono w-full",
                    placeholder: (t(lang, "pools-field-fallback-offline-placeholder"))
                );
            }
            label(class: "flex flex-col gap-1") {
                span(class: "text-xs opacity-70") { (t(lang, "pools-field-models")) }
                input(
                    type: "text", name: "models", value: (models),
                    class: "input input-bordered input-sm font-mono w-full",
                    placeholder: "qwen-32b, glm-4.6"
                );
                span(class: "text-xs text-base-content/50") { (t(lang, "pools-field-models-hint")) }
            }
            label(class: "flex flex-col gap-1") {
                span(class: "text-xs opacity-70") { (t(lang, "pools-field-allowed-groups")) }
                input(
                    type: "text", name: "allowed_groups", value: (allowed_groups),
                    class: "input input-bordered input-sm font-mono w-full",
                    placeholder: "developers, network_admin"
                );
                span(class: "text-xs text-base-content/50") { (t(lang, "pools-field-allowed-groups-hint")) }
            }
            (voices_field)
            fieldset(class: "flex flex-col gap-1") {
                span(class: "text-xs opacity-70") { (t(lang, "pools-field-backends")) }
                if backend_boxes.is_empty() {
                    span(class: "text-xs text-base-content/50 italic") { (t(lang, "pools-no-backends")) }
                } else {
                    div(class: "flex flex-wrap gap-x-4 gap-y-1") {
                        for b in backend_boxes.iter() { (b.clone()) }
                    }
                }
            }
            div(class: "flex flex-wrap gap-4") {
                (gdpr_box)
                (nda_box)
                (enforce_box)
            }
            div(class: "flex justify-end gap-2") {
                if let Some(c) = cancel.as_ref() { (c.clone()) }
                button(type: "submit", class: "btn btn-primary btn-sm") {
                    (icons::check(14))
                    span { (t(lang, save_key)) }
                }
            }
        }
    }
    .to_html()
}

/// The pool kind `<select>`; the add-pool variant binds `addPoolKind` so the
/// voices field can reveal itself for speech. Standalone helper so the
/// conditional `data-bind` attribute doesn't force the caller's locals into the
/// macro's per-attribute closures.
fn pool_kind_select(kind_opts: &[Html], is_add: bool) -> Html {
    if is_add {
        html! {
            select(
                name: "kind", class: "select select-bordered select-sm w-full",
                "data-bind": "addPoolKind"
            ) {
                for o in kind_opts.iter() { (o.clone()) }
            }
        }
        .to_html()
    } else {
        html! {
            select(name: "kind", class: "select select-bordered select-sm w-full") {
                for o in kind_opts.iter() { (o.clone()) }
            }
        }
        .to_html()
    }
}

/// The voices textarea. Speech-only: edit forms render it only when the pool is
/// already a speech pool; the add form always renders it but hides it behind
/// `data-show="$addPoolKind === 'speech'"`. Returns empty when an edit form's
/// pool isn't speech.
fn render_pool_voices_field(lang: Lang, voices: &str, is_add: bool, is_speech: bool) -> Html {
    if !is_add && !is_speech {
        return html! {}.to_html();
    }
    let voices = voices.to_string();
    let label = t(lang, "pools-field-voices");
    if is_add {
        html! {
            label(class: "flex flex-col gap-1", "data-show": "$addPoolKind === 'speech'") {
                span(class: "text-xs opacity-70") { (label) }
                textarea(
                    name: "voices", class: "textarea textarea-bordered textarea-sm font-mono w-full",
                    rows: "2", placeholder: "de=de-voice\nen=en-voice"
                ) { (voices) }
            }
        }
        .to_html()
    } else {
        html! {
            label(class: "flex flex-col gap-1") {
                span(class: "text-xs opacity-70") { (label) }
                textarea(
                    name: "voices", class: "textarea textarea-bordered textarea-sm font-mono w-full",
                    rows: "2", placeholder: "de=de-voice\nen=en-voice"
                ) { (voices) }
            }
        }
        .to_html()
    }
}

/// A "Cancel" button that hides the add card (`$addForm = ''`).
fn pool_cancel_button(lang: Lang) -> Html {
    html! {
        button(type: "button", class: "btn btn-ghost btn-sm", "data-on:click": "$addForm = ''") {
            (t(lang, "upstreams-cancel"))
        }
    }
    .to_html()
}

/// Backend editor form for an existing backend (inside a health row's
/// `<details>`). Carries the two-step delete + a Cancel that closes the parent
/// `<details>`.
fn render_backend_form(
    lang: Lang,
    existing: &BackendRow,
    del_sig: &str,
    current_pool: Option<&str>,
    pool_names: &[String],
) -> Html {
    let fields = backend_form_fields(lang, Some(existing), current_pool, pool_names, None);
    let delete = two_step_delete(
        "/admin/backends/delete",
        &existing.name,
        &t(lang, "backends-delete-backend"),
        &t(lang, "upstreams-delete-confirm"),
        del_sig,
    );
    html! {
        form(
            method: "post",
            action: "/admin/backends/save",
            "data-on:submit__prevent": "@post('/admin/backends/save', {contentType: 'form'})",
            class: "flex flex-col gap-3 m-0"
        ) {
            (fields)
            div(class: "flex items-center gap-2") {
                (delete)
                span(class: "flex-1") {}
                button(
                    type: "button", class: "btn btn-ghost btn-sm",
                    "data-on:click": "el.closest('details').open = false"
                ) { (t(lang, "upstreams-cancel")) }
                button(type: "submit", class: "btn btn-primary btn-sm") {
                    (icons::check(14))
                    span { (t(lang, "backends-save-backend")) }
                }
            }
        }
    }
    .to_html()
}

/// Backend editor form for the Add-backend card (no delete; Cancel hides the
/// card via `$addForm`).
fn render_backend_form_add(lang: Lang, pool_names: &[String]) -> Html {
    let fields = backend_form_fields(
        lang,
        None,
        None,
        pool_names,
        Some(ADD_BACKEND_POOL_SELECT_ID),
    );
    html! {
        form(
            method: "post",
            action: "/admin/backends/save",
            "data-on:submit__prevent": "@post('/admin/backends/save', {contentType: 'form'})",
            class: "flex flex-col gap-3 m-0"
        ) {
            (fields)
            div(class: "flex justify-end gap-2") {
                button(
                    type: "button", class: "btn btn-ghost btn-sm",
                    "data-on:click": "$addForm = ''"
                ) { (t(lang, "upstreams-cancel")) }
                button(type: "submit", class: "btn btn-primary btn-sm") {
                    (icons::check(14))
                    span { (t(lang, "backends-add-backend")) }
                }
            }
        }
    }
    .to_html()
}

/// Stable id of the Add-backend form's Pool `<select>`. The select is rendered
/// once at page load; the pool save/delete handlers use this id to patch its
/// options in place (via [`add_backend_pool_select_patch`]) so a just-created
/// pool is immediately selectable without a full reload.
pub(super) const ADD_BACKEND_POOL_SELECT_ID: &str = "add-backend-pool";

/// The single "Pool" `<select>` for the backend editor: "(none)" plus one
/// option per pool, preselecting `current_pool`. `id` is set only on the
/// Add-backend instance (so it is patch-targetable — see
/// [`ADD_BACKEND_POOL_SELECT_ID`]); the per-backend edit instances render
/// without an id to keep ids unique on the page.
fn render_pool_select(
    lang: Lang,
    id: Option<&str>,
    current_pool: Option<&str>,
    pool_names: &[String],
) -> Html {
    let mut opts = vec![super::select_option(
        "",
        &t(lang, "backends-field-pool-none"),
        current_pool.is_none(),
    )];
    for p in pool_names {
        opts.push(super::select_option(p, p, current_pool == Some(p.as_str())));
    }
    // Two standalone branches so the `html!` macro never has to express a
    // conditional `id` attribute (see the note on `select_option`).
    match id {
        Some(id) => html! {
            select(id: (id.to_string()), name: "pool", class: "select select-bordered select-sm w-full") {
                for o in opts.iter() { (o.clone()) }
            }
        }
        .to_html(),
        None => html! {
            select(name: "pool", class: "select select-bordered select-sm w-full") {
                for o in opts.iter() { (o.clone()) }
            }
        }
        .to_html(),
    }
}

/// A `datastar-patch-elements` event re-rendering the Add-backend form's Pool
/// `<select>` from the current pool list. Emitted by the pool save/delete
/// handlers so a just-created (still-unapplied) pool becomes selectable
/// immediately: the select is rendered once at page load and would otherwise
/// keep its stale options until a reload. Morphs the element in place
/// (`outer`), matched by [`ADD_BACKEND_POOL_SELECT_ID`].
pub(super) fn add_backend_pool_select_patch(
    lang: Lang,
    pool_names: &[String],
) -> rama::bytes::Bytes {
    let select =
        render_pool_select(lang, Some(ADD_BACKEND_POOL_SELECT_ID), None, pool_names).to_string();
    session_core::chrome::sse_patch(
        Some(&format!("#{ADD_BACKEND_POOL_SELECT_ID}")),
        Some("outer"),
        &select,
    )
}

/// The shared field grid for the backend editor (add + edit). The primary-key
/// `name` is read-only when editing. `pool_select_id` tags the Pool `<select>`
/// (Add-backend form only) so it can be patched in place on pool save/delete.
fn backend_form_fields(
    lang: Lang,
    existing: Option<&BackendRow>,
    current_pool: Option<&str>,
    pool_names: &[String],
    pool_select_id: Option<&str>,
) -> Html {
    let is_edit = existing.is_some();
    let name = existing.map(|b| b.name.clone()).unwrap_or_default();
    let base_url = existing.map(|b| b.base_url.clone()).unwrap_or_default();
    let api_key_env = existing
        .and_then(|b| b.api_key_env.clone())
        .unwrap_or_default();
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
        .map(|b| super::backends::alias_lines(&b.aliases))
        .unwrap_or_default();
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
    // Single "Pool" select: "(none)" plus one option per pool. Preselects the
    // backend's current pool so a plain round-trip save doesn't move it. See
    // `set_backend_pool` for the single-pool tradeoff this implies.
    let pool_select = render_pool_select(lang, pool_select_id, current_pool, pool_names);
    html! {
        div(class: "flex flex-col gap-3") {
            div(class: "grid grid-cols-1 sm:grid-cols-2 gap-3") {
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "backends-field-name")) }
                    (name_field)
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "backends-field-base-url")) }
                    input(
                        type: "text", name: "base_url", value: (base_url),
                        class: "input input-bordered input-sm font-mono w-full",
                        required: "required", placeholder: "http://gpu-01:8000/v1"
                    );
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "backends-field-api-key")) }
                    input(
                        type: "password", name: "api_key", value: "", autocomplete: "off",
                        class: "input input-bordered input-sm font-mono w-full", placeholder: (key_placeholder)
                    );
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "backends-field-api-key-env")) }
                    input(
                        type: "text", name: "api_key_env", value: (api_key_env),
                        class: "input input-bordered input-sm font-mono w-full", placeholder: "GPU01_KEY"
                    );
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "backends-field-health-path")) }
                    input(
                        type: "text", name: "health_path", value: (health_path),
                        class: "input input-bordered input-sm font-mono w-full", placeholder: "/models"
                    );
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "backends-field-weight")) }
                    input(
                        type: "number", name: "weight", value: (weight), min: "1",
                        class: "input input-bordered input-sm w-full"
                    );
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "backends-field-max-inflight")) }
                    input(
                        type: "number", name: "max_inflight", value: (max_inflight), min: "1",
                        class: "input input-bordered input-sm w-full"
                    );
                }
                label(class: "flex flex-col gap-1") {
                    span(class: "text-xs opacity-70") { (t(lang, "backends-field-pool")) }
                    (pool_select)
                    span(class: "text-xs text-base-content/50") { (t(lang, "backends-field-pool-hint")) }
                }
            }
            label(class: "flex flex-col gap-1") {
                span(class: "text-xs opacity-70") { (t(lang, "backends-field-models")) }
                input(
                    type: "text", name: "models", value: (models),
                    class: "input input-bordered input-sm font-mono w-full", placeholder: "qwen-32b, qwen-7b"
                );
            }
            label(class: "flex flex-col gap-1") {
                span(class: "text-xs opacity-70") { (t(lang, "backends-field-aliases")) }
                textarea(
                    name: "aliases", class: "textarea textarea-bordered textarea-sm font-mono w-full",
                    rows: "2", placeholder: "fast=qwen-7b\nsmart=qwen-32b"
                ) { (aliases) }
            }
            div(class: "flex flex-wrap gap-4") {
                (probe_box)
                (edit_box)
            }
        }
    }
    .to_html()
}

// ---------------------------------------------------------------------------
// Unassigned backends + fallbacks
// ---------------------------------------------------------------------------

fn render_unassigned_card(
    lang: Lang,
    backends: &[&BackendRow],
    health: &HashMap<String, BackendHealth>,
    pool_names: &[String],
) -> Html {
    let rows: Vec<Html> = backends
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let sig = format!("dub{i}");
            // Unassigned → no current pool preselected.
            render_backend_details(lang, b, health.get(b.name.as_str()), &sig, None, pool_names)
        })
        .collect();
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                header(class: "flex flex-col gap-1") {
                    h2(class: "card-title text-base") { (t(lang, "upstreams-unassigned-heading")) }
                    p(class: "text-base-content/70 text-sm") { (t(lang, "upstreams-unassigned-description")) }
                }
                div(class: "flex flex-col gap-2") {
                    for r in rows.iter() { (r.clone()) }
                }
            }
        }
    }
    .to_html()
}

/// The global unknown-model fallback editor: one auto-saving `<select>` per
/// fallback-capable kind, populated from the advertised model set. Selecting
/// "(none)" clears the fallback for that kind. Unchanged from the old pools page.
fn render_fallbacks_card(
    lang: Lang,
    fallbacks: &HashMap<String, String>,
    all_models: &[String],
) -> Html {
    let selects: Vec<Html> = super::pools::FALLBACK_KINDS
        .iter()
        .map(|kind| {
            fallback_select(
                lang,
                kind,
                fallbacks.get(*kind).map(String::as_str),
                all_models,
            )
        })
        .collect();
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                h2(class: "card-title text-base") { (t(lang, "pools-fallbacks-heading")) }
                p(class: "text-base-content/70 text-sm") { (t(lang, "pools-fallbacks-description")) }
                div(class: "grid grid-cols-1 sm:grid-cols-2 gap-3") {
                    for s in selects.iter() { (s.clone()) }
                }
            }
        }
    }
    .to_html()
}

fn fallback_select(lang: Lang, kind: &str, current: Option<&str>, all_models: &[String]) -> Html {
    let action = "/admin/pools/fallback";
    let post = format!("@post('{action}', {{contentType: 'form'}})");
    let mut opts: Vec<Html> = vec![super::select_option(
        "",
        &t(lang, "admin-cap-no-fallback"),
        false,
    )];
    for m in all_models {
        opts.push(super::select_option(m, m, current == Some(m.as_str())));
    }
    html! {
        form(method: "post", action: (action), class: "m-0") {
            input(type: "hidden", name: "kind", value: (kind.to_string()));
            label(class: "flex flex-col gap-1") {
                span(class: "text-xs opacity-70 font-mono") { (kind.to_string()) }
                select(name: "model", class: "select select-bordered select-sm w-full", "data-on:change": (post)) {
                    for o in opts.iter() { (o.clone()) }
                }
            }
        }
    }
    .to_html()
}

// ---------------------------------------------------------------------------
// Shared bits
// ---------------------------------------------------------------------------

/// Two-step delete: the first click arms the button ("Really delete?"), the
/// second posts the delete; it auto-disarms after 3s. Implemented with a
/// per-instance datastar signal (`sig`), so no `window.confirm` and no custom
/// JS file. The two labels are presence-toggled via `data-show` (the plait
/// bool-attr trap doesn't apply — `data-show` is a normal string attribute).
fn two_step_delete(action: &str, name: &str, label: &str, confirm: &str, sig: &str) -> Html {
    let signals = format!("{{{sig}: false}}");
    let click = format!(
        "${sig} ? @post('{action}', {{contentType: 'form'}}) : \
         (${sig} = true, setTimeout(() => ${sig} = false, 3000))"
    );
    let show_label = format!("!${sig}");
    let show_confirm = format!("${sig}");
    let action = action.to_string();
    let name = name.to_string();
    let label = label.to_string();
    let confirm = confirm.to_string();
    html! {
        form(method: "post", action: (action), class: "m-0 inline-block", "data-signals": (signals)) {
            input(type: "hidden", name: "name", value: (name));
            button(type: "button", class: "btn btn-ghost btn-xs text-error", "data-on:click": (click)) {
                (icons::trash(12))
                span("data-show": (show_label)) { (label) }
                span("data-show": (show_confirm)) { (confirm) }
            }
        }
    }
    .to_html()
}

/// Build `<option>`s for a `<select>`, marking `current` as selected. The
/// option label is the raw snake_case value (matching the badges above).
fn options_for(values: &[&str], current: &str) -> Vec<Html> {
    values
        .iter()
        .map(|v| super::select_option(v, v, *v == current))
        .collect()
}

/// A tiny inline-SVG bar sparkline of `values` (oldest → newest), inheriting
/// the surrounding text color via `currentColor`. Idle buckets render as faint
/// stubs so an all-zero series reads as a flat baseline. No JS, no chart lib.
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

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;

    fn sample_pool() -> PoolRow {
        PoolRow {
            name: "chat-eu".into(),
            kind: "chat".into(),
            strategy: "round_robin".into(),
            fallback_offline: Some("glm-4.6".into()),
            compliance_gdpr: false,
            compliance_nda: true,
            enforce_limits: true,
            sort_order: 3,
            allowed_groups: vec!["developers".into()],
            backends: vec!["gpu-01".into()],
            models: vec!["qwen-32b".into()],
            voices: vec![],
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        }
    }

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
            aliases: vec![],
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        }
    }

    /// The add-pool form posts to the save route via datastar, has no delete,
    /// and reveals the voices field only for speech (data-show on addPoolKind).
    #[test]
    fn add_pool_form_wires_to_save_and_gates_voices() {
        let html = render_pool_form(Lang::En, None, 0, &["gpu-01".into()], true).to_string();
        assert!(
            html.contains(r#"action="/admin/pools/save""#),
            "form must post to the save route: {html}"
        );
        assert!(
            html.contains("@post(") && html.contains("/admin/pools/save"),
            "submit must datastar-post to the save route: {html}"
        );
        assert!(
            html.contains(r#"data-bind="addPoolKind""#),
            "add-pool kind select must bind the signal: {html}"
        );
        // Attribute values are HTML-escaped by plait (`'` → `&#39;`), so match
        // the unescaped prefix of the gate expression.
        assert!(
            html.contains("$addPoolKind === "),
            "voices field must be gated on the pool kind: {html}"
        );
    }

    /// An edit pool form marks the stored kind/strategy selected, checks the
    /// assigned backend, reflects the unchecked GDPR flag, locks the name, and
    /// arms a two-step delete posting to the delete route.
    #[test]
    fn edit_pool_card_reflects_stored_state() {
        let p = sample_pool();
        let mut backends = HashMap::new();
        backends.insert("gpu-01".to_string(), sample_backend());
        let html = render_pool_card(
            Lang::En,
            0,
            &p,
            &backends,
            &["gpu-01".into(), "gpu-02".into()],
            &HashMap::new(),
            &["chat-eu".into()],
        )
        .to_string();
        assert!(
            html.contains(r#"value="chat" selected="selected""#),
            "stored kind must be selected: {html}"
        );
        assert!(
            html.contains(r#"value="round_robin" selected="selected""#),
            "stored strategy must be selected: {html}"
        );
        assert!(
            html.contains(r#"name="backends" value="gpu-01" checked="checked""#),
            "assigned backend must be checked: {html}"
        );
        assert!(
            html.contains(r#"name="compliance_nda" value="on" checked="checked""#),
            "nda true → checked: {html}"
        );
        assert!(
            html.contains(r#"name="compliance_gdpr" value="on" class="checkbox checkbox-sm""#),
            "gdpr false → not checked: {html}"
        );
        assert!(
            html.contains("readonly=\"readonly\""),
            "name must be read-only on edit: {html}"
        );
        assert!(
            html.contains(r#"action="/admin/pools/delete""#),
            "pool card must offer a delete: {html}"
        );
    }

    /// An edit backend form pre-fills values, locks the primary-key name,
    /// reflects the stored checkbox, and arms a two-step delete.
    #[test]
    fn edit_backend_form_prefills_locks_name_and_offers_delete() {
        let b = sample_backend();
        let html = render_backend_form(Lang::En, &b, "db0", Some("chat-eu"), &["chat-eu".into()])
            .to_string();
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
            "probe_models true → checked: {html}"
        );
        assert!(
            html.contains(r#"action="/admin/backends/delete""#),
            "edit form must offer a delete: {html}"
        );
        assert!(
            html.contains("qwen-32b, qwen-7b"),
            "models must be comma-joined: {html}"
        );
        assert!(
            html.contains(r#"action="/admin/backends/save""#),
            "form must post to the save route: {html}"
        );
        // Single "Pool" select present and preselecting the backend's pool.
        assert!(
            html.contains(r#"name="pool""#),
            "backend form must offer a pool select: {html}"
        );
        assert!(
            html.contains(r#"value="chat-eu" selected="selected""#),
            "current pool must be preselected: {html}"
        );
    }

    /// The per-kind fallback select auto-saves to the fallback route and marks
    /// the current model selected.
    #[test]
    fn fallback_select_wires_and_marks_current() {
        let models = vec!["qwen-32b".to_string(), "glm-4.6".to_string()];
        let html = fallback_select(Lang::En, "chat", Some("glm-4.6"), &models).to_string();
        assert!(
            html.contains(r#"action="/admin/pools/fallback""#),
            "must post to the fallback route: {html}"
        );
        assert!(
            html.contains(r#"name="kind" value="chat""#),
            "kind discriminator must be present: {html}"
        );
        assert!(
            html.contains(r#"value="glm-4.6" selected="selected""#),
            "current model must be selected: {html}"
        );
    }

    /// The apply bar binds visibility + counter to the `topologyDirty` signal
    /// and its button posts the reload — the dirty-state ↔ endpoint contract.
    #[test]
    fn apply_bar_binds_signal_and_posts_reload() {
        let html = render_apply_bar(Lang::En).to_string();
        // `>` is HTML-escaped to `&gt;` inside the attribute value.
        assert!(
            html.contains("$topologyDirty &gt; 0"),
            "apply bar visibility must bind the signal: {html}"
        );
        assert!(
            html.contains(r#"data-text="$topologyDirty""#),
            "counter must bind the signal: {html}"
        );
        assert!(
            html.contains("/admin/upstreams/reload"),
            "apply button must post the reload route: {html}"
        );
    }

    /// The body seeds `topologyDirty` from the server-side count so the bar is
    /// correct on initial load (not only after a datastar event).
    #[test]
    fn body_seeds_dirty_signal_from_count() {
        let html = render_body(
            Lang::En,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &[],
            &[],
            &HashMap::new(),
            2,
        )
        .to_string();
        assert!(
            html.contains("topologyDirty: 2"),
            "container must seed the dirty count: {html}"
        );
    }

    /// The Add-backend form's Pool select carries the stable patch id and lists
    /// the current pools, so pool save/delete can refresh it in place.
    #[test]
    fn add_backend_pool_select_carries_patch_id_and_lists_pools() {
        let html =
            render_add_backend_card(Lang::En, &["chat-eu".into(), "voxtral".into()]).to_string();
        assert!(
            html.contains(&format!(r#"id="{ADD_BACKEND_POOL_SELECT_ID}""#)),
            "add-backend pool select must carry the patch id: {html}"
        );
        assert!(
            html.contains(r#"value="voxtral""#),
            "add-backend pool select must list the pools: {html}"
        );
    }

    /// A pool save/delete emits a `datastar-patch-elements` event that morphs the
    /// Add-backend Pool select (by id) to the fresh pool list — so a newly
    /// created, still-unapplied pool is immediately selectable there without a
    /// reload. Pins the wiring behind tasks: create pool → visible in backend form.
    #[test]
    fn pool_select_patch_targets_add_backend_select_by_id() {
        let bytes = add_backend_pool_select_patch(Lang::En, &["chat-eu".into(), "newpool".into()]);
        let s = String::from_utf8(bytes.to_vec()).expect("utf8");
        assert!(
            s.contains("event: datastar-patch-elements"),
            "must be a patch-elements event: {s}"
        );
        assert!(
            s.contains(&format!(r#"id="{ADD_BACKEND_POOL_SELECT_ID}""#)),
            "patch must carry the select id so it morphs in place: {s}"
        );
        assert!(
            s.contains("newpool"),
            "patch must include the freshly-created pool: {s}"
        );
    }

    /// A backend row (pooled or unassigned) exposes an explicit "Edit backend"
    /// toggle that reveals the editor — the editor must not be buried inside the
    /// live status/models summary (which is huge for many-model backends).
    #[test]
    fn backend_row_has_explicit_edit_toggle_revealing_editor() {
        let b = sample_backend();
        let html = render_backend_details(
            Lang::En,
            &b,
            None,
            "db0",
            Some("chat-eu"),
            &["chat-eu".into()],
        )
        .to_string();
        assert!(
            html.contains(&t(Lang::En, "upstreams-edit-backend")),
            "must offer an explicit Edit backend toggle: {html}"
        );
        assert!(
            html.contains("<details") && html.contains("<summary"),
            "editor must sit behind a details/summary toggle: {html}"
        );
        assert!(
            html.contains(r#"action="/admin/backends/save""#)
                && html.contains(r#"name="name" value="gpu-01""#),
            "the toggle must reveal the pre-filled backend editor: {html}"
        );
    }

    /// Served (active) models are always shown; the withheld ("inactive") set
    /// collapses behind a clickable `+N inactive` pill and each withheld chip is
    /// gated by the row's toggle signal so the collapsed row stays compact.
    #[test]
    fn withheld_models_collapse_behind_inactive_pill() {
        let b = sample_backend();
        let health = BackendHealth {
            healthy: true,
            saturated: false,
            inflight: 0,
            max_inflight: 16,
            served: vec!["gpt-4o-mini-tts".into()],
            withheld: vec!["gpt-4".into(), "gpt-3.5-turbo".into(), "davinci-002".into()],
            aliases: vec![],
            recent: vec![],
        };
        let html = render_backend_details(
            Lang::En,
            &b,
            Some(&health),
            "dpb0_0",
            Some("chat-eu"),
            &["chat-eu".into()],
        )
        .to_string();
        assert!(
            html.contains("gpt-4o-mini-tts"),
            "served (active) model must always be shown: {html}"
        );
        assert!(
            html.contains("+3 inactive"),
            "pill must show the withheld count: {html}"
        );
        assert!(
            html.contains("{inact_dpb0_0: false}"),
            "row must declare its own collapse signal: {html}"
        );
        assert!(
            html.contains(r#"data-show="$inact_dpb0_0""#),
            "withheld chips must be gated by the row signal (collapsed by default): {html}"
        );
        assert!(
            html.contains("davinci-002"),
            "withheld chips must still be rendered (revealed on expand): {html}"
        );
    }
}
