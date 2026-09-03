// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Usage statistics pages.
//!
//! `/usage` — every signed-in user's own usage (scoped to their `user_id`),
//! gated by [`super::require_session_or_redirect`]. Admins additionally get an
//! in-page "All users" toggle that switches the same renderer to an
//! all-users + per-backend breakdown (gated on the `admin` role); there is
//! no separate `/admin/usage` route.
//!
//! A period (today … last month) + optional source/backend filters drive a
//! server-side aggregation (`server::db::usage`). The filter bar is a plain
//! GET form that auto-submits on change, so the view is fully reconstructable
//! from the URL and needs no client state.
//!
//! "Requests" counts upstream **backend calls**: a tool-using turn makes
//! several, so a user's request total is ≥ their turn total. The page says
//! so inline.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};
use serde::Deserialize;

use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_session_or_redirect};
use session_core::chrome::{NavSections, Theme, is_datastar_request};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

use gateway_core::server::db::limits::Dimension;
use gateway_core::server::db::usage::{self, Aggregates, Filter, GroupCount, Period};
use gateway_core::server::limits::LimitStatus;
use gateway_runtime::rama_server::state::RamaState;

/// Query string for the filter bar. All optional; empty strings collapse to
/// "no filter". `scope=all` is the admin "all users" view (ignored for
/// non-admins, who only ever see their own data).
#[derive(Debug, Default, Deserialize)]
struct UsageQuery {
    #[serde(default)]
    period: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    backend: Option<String>,
    /// Drill down to one API token. `''` is the token-less traffic (chat and
    /// scheduled runs), which is a real selection rather than "no filter" —
    /// hence the explicit `none` sentinel below.
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

/// `GET /usage` — every signed-in user's own usage, with an admin-only
/// "All users" toggle (`?scope=all`) that widens it to the whole roster.
pub async fn usage_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = require_session!(state, req);
    let admin = is_admin(&state, &user);

    let q: UsageQuery = req
        .uri()
        .query()
        .and_then(|s| serde_urlencoded::from_str(s).ok())
        .unwrap_or_default();
    // "All users" is admin-only; a non-admin passing ?scope=all is ignored.
    let show_all = admin && q.scope.as_deref() == Some("all");
    let period = Period::parse(q.period.as_deref());
    // Period boundaries are taken in the viewer's timezone; fall back to UTC.
    let tz = session
        .timezone
        .clone()
        .or_else(|| user.timezone.clone())
        .unwrap_or_else(|| "UTC".to_string());
    let now = jiff::Timestamp::now();
    let bounds = usage::period_bounds(period, &tz, now);

    let filter = Filter {
        source: q.source.clone().filter(|s| !s.is_empty()),
        backend: q.backend.clone().filter(|s| !s.is_empty()),
        // Scoped to the caller unless an admin asked for all users.
        user_id: (!show_all).then(|| user.id.clone()),
        // `none` selects the rows that carry no token at all. An empty value
        // means "every token", so the two cannot share a spelling.
        token_id: match q.token.as_deref() {
            None | Some("") => None,
            Some(NO_TOKEN_FILTER) => Some(String::new()),
            Some(id) => Some(id.to_string()),
        },
    };
    let retention = state.config().usage.retention_days;
    let agg = usage::aggregate(&state.db, bounds, &filter, retention, now, show_all)
        .await
        .unwrap_or_default();
    let backends = usage::distinct_backends(&state.db, bounds)
        .await
        .unwrap_or_default();
    // Unfiltered, and scoped to the caller unless this is the all-users view
    // — so the picker still offers every other token once one is selected.
    let tokens = usage::distinct_tokens(&state.db, bounds, (!show_all).then_some(user.id.as_str()))
        .await
        .unwrap_or_default();

    // The viewer's own in-force limits + current usage, for the progress bars.
    // Always about the caller (not the whole roster), so shown only in the
    // self view. Empty when unlimited or enforcement is off.
    let role_ids = state.role_ids_for(&user.roles);
    let limit_status = state.enforcer.statuses(&user.id, &role_ids).await;

    // Models that saw traffic this window but have no configured price → their
    // spend is silently under-counted. Surface them so the gap is visible.
    let priced = gateway_core::server::db::model_defaults::all_prices(&state.db)
        .await
        .unwrap_or_default();
    let unpriced: Vec<String> = agg
        .by_model
        .iter()
        .filter(|g| {
            !priced.contains_key(&g.key)
                && (g.total_tokens > 0 || g.input_units > 0.0 || g.output_units > 0.0)
        })
        .map(|g| g.key.clone())
        .collect();

    let title = if show_all {
        t(lang, "usage-title-all")
    } else {
        t(lang, "usage-title-mine")
    };
    let body = render_body(
        lang,
        admin,
        show_all,
        state.usage.is_enabled(),
        &state.config().usage.currency,
        &tz,
        period,
        &filter,
        &backends,
        &tokens,
        &agg,
        &limit_status,
        &unpriced,
    );
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    {
        let pctx = super::PageCtx {
            theme,
            lang,
            nav,
            datastar,
            user_email: user.email.clone(),
            is_admin: admin,
            skills_enabled: state.user_skills_enabled(),
            impersonating: session.impersonator_id.is_some(),
        };
        nav_or_html_page(&pctx, NavItem::Usage, &title, body, "/usage", &chat)
    }
}

/// Source filter options: `(value, i18n key)`. Empty value = "all".
const SOURCE_OPTIONS: [(&str, &str); 4] = [
    ("", "usage-source-all"),
    (
        // keep in sync with UsageSource::V1Api.as_str()
        "v1_api",
        "usage-source-api",
    ),
    ("chat", "usage-source-chat"),
    ("scheduled", "usage-source-scheduled"),
];

#[allow(clippy::too_many_arguments)]
fn render_body(
    lang: Lang,
    admin: bool,
    show_all: bool,
    metrics_on: bool,
    currency: &str,
    tz: &str,
    period: Period,
    filter: &Filter,
    backends: &[String],
    tokens: &[(String, String)],
    agg: &Aggregates,
    limit_status: &[LimitStatus],
    unpriced: &[String],
) -> Html {
    // Only surface money once there's priced spend in this window, so
    // deployments that never set per-model prices see the page unchanged.
    let show_cost = agg.summary.total_cost > 0.0;
    // The caller's own limit bars (self view only — the all-users view is
    // about the whole roster, not the admin's personal budget).
    let bars = if show_all || limit_status.is_empty() {
        html! {}.to_html()
    } else {
        render_limit_bars(lang, currency, tz, limit_status)
    };
    // Warn when some traffic this window hit an unpriced model — its spend is
    // missing from the cost figures until a price is set in /admin/models.
    let unpriced_notice = if unpriced.is_empty() {
        html! {}.to_html()
    } else {
        let warn = t_args(
            lang,
            "usage-unpriced-warning",
            &i18n::args([("models", unpriced.join(", ").into())]),
        );
        html! {
            div(class: "alert alert-warning") {
                (icons::alert(18))
                span { (warn) }
            }
        }
        .to_html()
    };
    let heading = if show_all {
        t(lang, "usage-heading-all")
    } else {
        t(lang, "usage-heading-mine")
    };
    let blurb = if show_all {
        t(lang, "usage-blurb-all")
    } else {
        t(lang, "usage-blurb-mine")
    };

    // When metrics are switched off (`[usage].enabled = false`), the page
    // still renders but the numbers are frozen — say so rather than letting
    // empty/stale tables read as "no traffic".
    let disabled_notice = if metrics_on {
        html! {}.to_html()
    } else {
        html! {
            div(class: "alert alert-warning") {
                (icons::alert(18))
                span {
                    (t(lang, "usage-metrics-disabled-prefix"))
                    code(class: "text-xs") { "[usage].enabled = false" }
                    (t(lang, "usage-metrics-disabled-suffix"))
                }
            }
        }
        .to_html()
    };

    let filter_bar = render_filter_bar(lang, period, filter, backends, tokens, show_all);
    // Empty fragment for non-admins (no "All users" toggle).
    let scope_toggle = if admin {
        render_scope_toggle(lang, show_all, period, filter)
    } else {
        html! {}.to_html()
    };
    let stats = render_stats(lang, show_all, show_cost, currency, &agg.summary);

    // The all-users view gets a leading per-user table; everyone gets the
    // dimension splits.
    let mut tables: Vec<Html> = Vec::new();
    if show_all {
        tables.push(render_table(
            lang,
            "usage-table-by-user",
            "usage-key-user",
            show_cost,
            currency,
            &agg.by_user,
        ));
    }
    tables.push(render_table(
        lang,
        "usage-table-by-backend",
        "usage-key-backend",
        show_cost,
        currency,
        &agg.by_backend,
    ));
    tables.push(render_table(
        lang,
        "usage-table-by-source",
        "usage-key-source",
        show_cost,
        currency,
        &agg.by_source,
    ));
    tables.push(render_table(
        lang,
        "usage-table-by-model",
        "usage-key-model",
        show_cost,
        currency,
        &agg.by_model,
    ));
    // Relabelled before rendering: the generic row renderer shows `label`, and
    // for tokens that is a name that may be missing or a '' key that means
    // something specific.
    let by_token: Vec<GroupCount> = agg
        .by_token
        .iter()
        .map(|r| GroupCount {
            label: token_label(lang, &r.key, &r.label),
            ..r.clone()
        })
        .collect();
    tables.push(render_table(
        lang,
        "usage-table-by-token",
        "usage-key-token",
        show_cost,
        currency,
        &by_token,
    ));

    html! {
        section(class: "max-w-5xl mx-auto p-4 sm:p-6 flex flex-col gap-4") {
            header(class: "flex flex-col gap-2") {
                div(class: "flex items-start justify-between gap-3 flex-wrap") {
                    h1(class: "text-2xl font-bold") { (heading) }
                    (scope_toggle)
                }
                p(class: "text-base-content/70 text-sm") { (blurb) }
            }
            (disabled_notice)
            (unpriced_notice)
            (bars)
            (filter_bar)
            (stats)
            div(class: "grid grid-cols-1 lg:grid-cols-2 gap-4") {
                for t in tables.iter() {
                    (t.clone())
                }
            }
        }
    }
    .to_html()
}

/// Build a `/usage` query string carrying the current filters plus the
/// given scope. Empty filters are omitted.
fn usage_href(scope_all: bool, period: Period, filter: &Filter) -> String {
    let mut q = format!("/usage?period={}", period.as_str());
    if scope_all {
        q.push_str("&scope=all");
    }
    if let Some(s) = filter.source.as_deref().filter(|s| !s.is_empty()) {
        q.push_str("&source=");
        q.push_str(s);
    }
    if let Some(b) = filter.backend.as_deref().filter(|s| !s.is_empty()) {
        q.push_str("&backend=");
        q.push_str(b);
    }
    // The token drill-down is a filter like the others, and the scope toggle
    // claims to preserve them all. `Some("")` is a real selection (the
    // token-less rows) and spells itself `none` in the query string.
    match filter.token_id.as_deref() {
        None => {}
        Some("") => q.push_str(&format!("&token={NO_TOKEN_FILTER}")),
        Some(id) => {
            q.push_str("&token=");
            q.push_str(id);
        }
    }
    q
}

/// Admin-only segmented toggle between the caller's own usage and the
/// whole-roster view. Each side is a link preserving the current filters,
/// so flipping scope doesn't reset the period/source/backend.
fn render_scope_toggle(lang: Lang, show_all: bool, period: Period, filter: &Filter) -> Html {
    let mine_href = usage_href(false, period, filter);
    let all_href = usage_href(true, period, filter);
    let mine_class = if show_all {
        "join-item btn btn-sm"
    } else {
        "join-item btn btn-sm btn-active btn-primary"
    };
    let all_class = if show_all {
        "join-item btn btn-sm btn-active btn-primary"
    } else {
        "join-item btn btn-sm"
    };
    let mine_label = t(lang, "usage-toggle-mine");
    let all_label = t(lang, "usage-toggle-all");
    html! {
        div(class: "join") {
            a(href: (mine_href), class: (mine_class), "data-on:click__prevent": (super::nav_get_directive(&mine_href))) { (mine_label) }
            a(href: (all_href), class: (all_class), "data-on:click__prevent": (super::nav_get_directive(&all_href))) { (all_label) }
        }
    }
    .to_html()
}

/// A `<option>` with `selected` set only when chosen — plait has no
/// conditional-attribute form, so we branch (matching `scheduled.rs`).
fn opt(value: &str, label: &str, selected: bool) -> Html {
    let value = value.to_string();
    let label = label.to_string();
    if selected {
        html! { option(value: (value), selected: "selected") { (label) } }.to_html()
    } else {
        html! { option(value: (value)) { (label) } }.to_html()
    }
}

/// Query value that selects traffic with no API token (the chat UI and
/// scheduled runs). An empty `?token=` means "every token", so the two need
/// different spellings.
const NO_TOKEN_FILTER: &str = "none";

/// Label a token by its `(key, name)`. The rollup keys token-less traffic as
/// '', and rows written before the token name was denormalised (or after the
/// token was deleted) carry no name — fall back to the id rather than
/// rendering a blank option nobody can pick out of a list.
fn token_label(lang: Lang, key: &str, name: &str) -> String {
    if key.is_empty() {
        return t(lang, "usage-token-none");
    }
    if name.is_empty() {
        return key.to_string();
    }
    name.to_string()
}

fn render_filter_bar(
    lang: Lang,
    period: Period,
    filter: &Filter,
    backends: &[String],
    tokens: &[(String, String)],
    show_all: bool,
) -> Html {
    let cur_source = filter.source.clone().unwrap_or_default();
    let cur_backend = filter.backend.clone().unwrap_or_default();
    // `None` = no filter; `Some("")` = the token-less rows, which the query
    // string spells `none`.
    let cur_token = match filter.token_id.as_deref() {
        None => String::new(),
        Some("") => NO_TOKEN_FILTER.to_string(),
        Some(id) => id.to_string(),
    };
    // Native GET submit on change — no datastar dependency; the URL fully
    // describes the view, and the server re-renders.
    let on_change = "evt.target.form.requestSubmit()";

    let period_opts: Vec<Html> = Period::ALL
        .iter()
        .map(|p| opt(p.as_str(), p.label(), *p == period))
        .collect();
    let source_opts: Vec<Html> = SOURCE_OPTIONS
        .iter()
        .map(|(value, key)| opt(value, &t(lang, key), *value == cur_source))
        .collect();
    let all_backends_label = t(lang, "usage-backend-all");
    let mut backend_opts: Vec<Html> = vec![opt("", &all_backends_label, cur_backend.is_empty())];
    for b in backends {
        backend_opts.push(opt(b, b, *b == cur_backend));
    }
    // Only tokens that actually saw traffic in this window are offered — the
    // list is a drill-down into what is on screen, not a token manager.
    let all_tokens_label = t(lang, "usage-token-all");
    let mut token_opts: Vec<Html> = vec![opt("", &all_tokens_label, cur_token.is_empty())];
    let mut selected_offered = cur_token.is_empty();
    for (key, label) in tokens {
        let value = if key.is_empty() {
            NO_TOKEN_FILTER
        } else {
            key.as_str()
        };
        let selected = value == cur_token;
        selected_offered |= selected;
        token_opts.push(opt(value, &token_label(lang, key, label), selected));
    }
    // A token with no traffic in this period isn't in the list, and a select
    // with nothing selected displays its first option — the picker would read
    // "All tokens" while the URL still filters, and the next change would
    // silently drop the drill-down. Offer the selection explicitly instead.
    if !selected_offered {
        token_opts.push(opt(&cur_token, &cur_token, true));
    }
    // Preserve the admin "all users" scope across filter changes (the
    // form's GET would otherwise drop it). Empty for the self view.
    let scope_value = if show_all { "all" } else { "" };
    let period_label = t(lang, "usage-filter-period");
    let source_label = t(lang, "usage-filter-source");
    let backend_label = t(lang, "usage-filter-backend");
    let token_label = t(lang, "usage-filter-token");
    let apply_label = t(lang, "usage-apply");

    html! {
        form(method: "get", action: "/usage", class: "flex flex-wrap items-end gap-3") {
            input(type: "hidden", name: "scope", value: (scope_value));
            label(class: "flex flex-col gap-1") {
                span(class: "label-text text-xs text-base-content/60") { (period_label) }
                select(name: "period", class: "select select-bordered select-sm", "data-on:change": (on_change)) {
                    for o in period_opts.iter() { (o.clone()) }
                }
            }
            label(class: "flex flex-col gap-1") {
                span(class: "label-text text-xs text-base-content/60") { (source_label) }
                select(name: "source", class: "select select-bordered select-sm", "data-on:change": (on_change)) {
                    for o in source_opts.iter() { (o.clone()) }
                }
            }
            label(class: "flex flex-col gap-1") {
                span(class: "label-text text-xs text-base-content/60") { (backend_label) }
                select(name: "backend", class: "select select-bordered select-sm", "data-on:change": (on_change)) {
                    for o in backend_opts.iter() { (o.clone()) }
                }
            }
            label(class: "flex flex-col gap-1") {
                span(class: "label-text text-xs text-base-content/60") { (token_label) }
                select(name: "token", class: "select select-bordered select-sm", "data-on:change": (on_change)) {
                    for o in token_opts.iter() { (o.clone()) }
                }
            }
            // Fallback for clients without JS: an explicit apply.
            noscript {
                button(type: "submit", class: "btn btn-sm btn-primary") { (apply_label) }
            }
        }
    }
    .to_html()
}

fn render_stats(
    lang: Lang,
    show_all: bool,
    show_cost: bool,
    currency: &str,
    s: &usage::Summary,
) -> Html {
    let requests = super::fmt_int(s.requests);
    let tokens = super::fmt_int(s.total_tokens);
    let errors = super::fmt_int(s.errors);
    let users = super::fmt_int(s.unique_users);
    let cost = super::fmt_cost(s.total_cost, currency);
    let requests_title = t(lang, "usage-stat-requests-title");
    let requests_desc = t(lang, "usage-stat-requests-desc");
    let tokens_title = t(lang, "usage-stat-tokens-title");
    let tokens_desc = t(lang, "usage-stat-tokens-desc");
    let cost_title = t(lang, "usage-stat-cost-title");
    let cost_desc = t(lang, "usage-stat-cost-desc");
    let users_title = t(lang, "usage-stat-users-title");
    let users_desc = t(lang, "usage-stat-users-desc");
    let errors_title = t(lang, "usage-stat-errors-title");
    let errors_desc = t(lang, "usage-stat-errors-desc");
    html! {
        div(class: "stats stats-vertical sm:stats-horizontal shadow bg-base-100 border border-base-300 w-full") {
            div(class: "stat") {
                div(class: "stat-title") { (requests_title) }
                div(class: "stat-value text-2xl tabular-nums") { (requests) }
                div(class: "stat-desc") { (requests_desc) }
            }
            div(class: "stat") {
                div(class: "stat-title") { (tokens_title) }
                div(class: "stat-value text-2xl tabular-nums") { (tokens) }
                div(class: "stat-desc") { (tokens_desc) }
            }
            if show_cost {
                div(class: "stat") {
                    div(class: "stat-title") { (cost_title) }
                    div(class: "stat-value text-2xl tabular-nums") { (cost) }
                    div(class: "stat-desc") { (cost_desc) }
                }
            }
            if show_all {
                div(class: "stat") {
                    div(class: "stat-title") { (users_title) }
                    div(class: "stat-value text-2xl tabular-nums") { (users) }
                    div(class: "stat-desc") { (users_desc) }
                }
            }
            div(class: "stat") {
                div(class: "stat-title") { (errors_title) }
                div(class: "stat-value text-2xl tabular-nums") { (errors) }
                div(class: "stat-desc") { (errors_desc) }
            }
        }
    }
    .to_html()
}

/// The caller's own limit bars — Claude-style: a filled track per in-force
/// limit, with `used / limit`, percent, and the next refresh time.
fn render_limit_bars(lang: Lang, currency: &str, tz: &str, statuses: &[LimitStatus]) -> Html {
    let zone = jiff::tz::TimeZone::get(tz).unwrap_or(jiff::tz::TimeZone::UTC);
    let bars: Vec<Html> = statuses
        .iter()
        .map(|s| render_limit_bar(lang, currency, &zone, s))
        .collect();
    html! {
        div(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3 p-4") {
                h2(class: "card-title text-base") { (t(lang, "usage-limits-heading")) }
                div(class: "flex flex-col gap-3") {
                    for b in bars.iter() { (b.clone()) }
                }
            }
        }
    }
    .to_html()
}

fn render_limit_bar(
    lang: Lang,
    currency: &str,
    zone: &jiff::tz::TimeZone,
    s: &LimitStatus,
) -> Html {
    let width = (s.fraction() * 100.0).round() as i64;
    let bar_class = if s.exceeded() {
        "bg-error"
    } else if s.fraction() >= 0.9 {
        "bg-warning"
    } else {
        "bg-primary"
    };
    let scope = s
        .model
        .clone()
        .unwrap_or_else(|| t(lang, "limits-all-models"));
    let title = format!(
        "{} · {} · {}",
        super::dim_label(lang, s.dimension, None),
        scope,
        super::win_label(lang, s.window),
    );
    let amounts = format!(
        "{} / {}",
        fmt_amount(s.dimension, s.used, currency),
        fmt_amount(s.dimension, s.limit, currency),
    );
    let refresh = s
        .refreshes_at
        .to_zoned(zone.clone())
        .strftime("%b %-d, %H:%M")
        .to_string();
    let used_label = t_args(
        lang,
        "usage-limit-used",
        &i18n::args([("percent", s.percent().to_string().into())]),
    );
    let refresh_label = t_args(
        lang,
        "usage-limit-refreshes",
        &i18n::args([("time", refresh.into())]),
    );
    html! {
        div(class: "flex flex-col gap-1") {
            div(class: "flex items-baseline justify-between gap-2 text-sm") {
                span(class: "font-medium") { (title) }
                span(class: "opacity-70 tabular-nums") { (amounts) }
            }
            div(class: "h-2 w-full rounded bg-base-300 overflow-hidden") {
                div(class: (format!("h-full {bar_class}")), style: (format!("width: {width}%"))) {}
            }
            div(class: "flex items-baseline justify-between gap-2 text-xs opacity-60") {
                span { (used_label) }
                span { (refresh_label) }
            }
        }
    }
    .to_html()
}

/// Format a limit amount for its dimension: cost with currency, else a
/// grouped integer.
fn fmt_amount(dim: Dimension, v: f64, currency: &str) -> String {
    match dim {
        Dimension::Cost => super::fmt_cost(v, currency),
        _ => super::fmt_int(v as i64),
    }
}

fn render_table(
    lang: Lang,
    title_key: &str,
    key_header_key: &str,
    show_cost: bool,
    currency: &str,
    rows: &[GroupCount],
) -> Html {
    let title = t(lang, title_key);
    let key_header = t(lang, key_header_key);
    let no_activity = t(lang, "usage-no-activity");
    let col_requests = t(lang, "usage-col-requests");
    let col_tokens = t(lang, "usage-col-tokens");
    let col_cost = t(lang, "usage-col-cost");
    let col_errors = t(lang, "usage-col-errors");
    let body: Vec<Html> = rows
        .iter()
        .map(|r| render_row(r, show_cost, currency))
        .collect();
    html! {
        div(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-2 p-4") {
                h2(class: "card-title text-base") { (title) }
                if rows.is_empty() {
                    p(class: "text-base-content/60 text-sm") { (no_activity) }
                } else {
                    div(class: "overflow-x-auto") {
                        table(class: "table table-sm") {
                            thead {
                                tr {
                                    th { (key_header) }
                                    th(class: "text-right") { (col_requests) }
                                    th(class: "text-right") { (col_tokens) }
                                    if show_cost {
                                        th(class: "text-right") { (col_cost) }
                                    }
                                    th(class: "text-right") { (col_errors) }
                                }
                            }
                            tbody {
                                for r in body.iter() { (r.clone()) }
                            }
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

fn render_row(r: &GroupCount, show_cost: bool, currency: &str) -> Html {
    let label = if r.label.is_empty() {
        "—".to_string()
    } else {
        r.label.clone()
    };
    let requests = super::fmt_int(r.requests);
    let tokens = super::fmt_int(r.total_tokens);
    let cost = super::fmt_cost(r.cost, currency);
    let errors = super::fmt_int(r.errors);
    let err_class = if r.errors > 0 {
        "text-right tabular-nums text-error"
    } else {
        "text-right tabular-nums"
    };
    html! {
        tr {
            td(class: "font-mono break-all max-w-xs") { (label) }
            td(class: "text-right tabular-nums") { (requests) }
            td(class: "text-right tabular-nums") { (tokens) }
            if show_cost {
                td(class: "text-right tabular-nums") { (cost) }
            }
            td(class: (err_class)) { (errors) }
        }
    }
    .to_html()
}

#[cfg(test)]
mod tests {

    #[test]
    fn source_options_match_enum_strings() {
        // The hardcoded filter values must match the enum's wire strings,
        // else the dropdown silently filters nothing.
        use super::SOURCE_OPTIONS;
        use gateway_core::server::db::usage::UsageSource;
        assert!(
            SOURCE_OPTIONS
                .iter()
                .any(|(v, _)| *v == UsageSource::V1Api.as_str())
        );
        assert!(
            SOURCE_OPTIONS
                .iter()
                .any(|(v, _)| *v == UsageSource::Chat.as_str())
        );
        assert!(
            SOURCE_OPTIONS
                .iter()
                .any(|(v, _)| *v == UsageSource::Scheduled.as_str())
        );
    }
}
