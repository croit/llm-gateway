// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Usage statistics pages.
//!
//! Two surfaces over the same renderer:
//!
//!   * `/usage` — every signed-in user's own usage (scoped to their
//!     `user_id`), gated by [`super::require_session_or_redirect`].
//!   * `/admin/usage` — all users + backends, gated on the `admin` role via
//!     [`super::require_admin_or_403`]; adds a per-user breakdown.
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
use session_core::chrome::{Theme, is_datastar_request};
use session_core::icons;

use crate::rama_server::state::RamaState;
use crate::server::db::usage::{self, Aggregates, Filter, GroupCount, Period};

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
    #[serde(default)]
    scope: Option<String>,
}

/// `GET /usage` — every signed-in user's own usage, with an admin-only
/// "All users" toggle (`?scope=all`) that widens it to the whole roster.
pub async fn usage_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
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
    };
    let retention = state.config.usage.retention_days;
    let agg = usage::aggregate(&state.db, bounds, &filter, retention, now, show_all)
        .await
        .unwrap_or_default();
    let backends = usage::distinct_backends(&state.db, bounds)
        .await
        .unwrap_or_default();

    let title = if show_all {
        "Usage — all users — LLM Gateway"
    } else {
        "Your usage — LLM Gateway"
    };
    let body = render_body(
        admin,
        show_all,
        state.usage.is_enabled(),
        period,
        &filter,
        &backends,
        &agg,
    );
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        NavItem::Usage,
        title,
        &user.email,
        admin,
        session.impersonator_id.is_some(),
        body,
        "/usage",
        &chat,
    )
}

/// Source filter options: `(value, label)`. Empty value = "all".
const SOURCE_OPTIONS: [(&str, &str); 4] = [
    ("", "All sources"),
    (
        // keep in sync with UsageSource::V1Api.as_str()
        "v1_api",
        "API (/v1)",
    ),
    ("chat", "Chat UI"),
    ("scheduled", "Scheduled"),
];

fn render_body(
    admin: bool,
    show_all: bool,
    metrics_on: bool,
    period: Period,
    filter: &Filter,
    backends: &[String],
    agg: &Aggregates,
) -> Html {
    let heading = if show_all {
        "Usage — all users"
    } else {
        "Your usage"
    };
    let blurb = if show_all {
        "Per-user and per-backend request volume and token usage across every \
         access method. \u{201c}Requests\u{201d} counts upstream backend calls, so a \
         tool-using turn (which makes several round-trips) counts as more than one."
    } else {
        "Your request volume and token usage across the chat UI, the API, and \
         scheduled actions. \u{201c}Requests\u{201d} counts upstream backend calls, so a \
         tool-using turn counts as more than one."
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
                    "Usage metrics are disabled ("
                    code(class: "text-xs") { "[usage].enabled = false" }
                    "). Figures below reflect only data recorded before it was turned off."
                }
            }
        }
        .to_html()
    };

    let filter_bar = render_filter_bar(period, filter, backends, show_all);
    // Empty fragment for non-admins (no "All users" toggle).
    let scope_toggle = if admin {
        render_scope_toggle(show_all, period, filter)
    } else {
        html! {}.to_html()
    };
    let stats = render_stats(show_all, &agg.summary);

    // The all-users view gets a leading per-user table; everyone gets the
    // dimension splits.
    let mut tables: Vec<Html> = Vec::new();
    if show_all {
        tables.push(render_table("By user", "User", &agg.by_user));
    }
    tables.push(render_table("By backend", "Backend", &agg.by_backend));
    tables.push(render_table("By source", "Source", &agg.by_source));
    tables.push(render_table("By model", "Model", &agg.by_model));

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
    q
}

/// Admin-only segmented toggle between the caller's own usage and the
/// whole-roster view. Each side is a link preserving the current filters,
/// so flipping scope doesn't reset the period/source/backend.
fn render_scope_toggle(show_all: bool, period: Period, filter: &Filter) -> Html {
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
    html! {
        div(class: "join") {
            a(href: (mine_href), class: (mine_class), "data-on:click__prevent": (super::nav_get_directive(&mine_href))) { "Mine" }
            a(href: (all_href), class: (all_class), "data-on:click__prevent": (super::nav_get_directive(&all_href))) { "All users" }
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

fn render_filter_bar(period: Period, filter: &Filter, backends: &[String], show_all: bool) -> Html {
    let cur_source = filter.source.clone().unwrap_or_default();
    let cur_backend = filter.backend.clone().unwrap_or_default();
    // Native GET submit on change — no datastar dependency; the URL fully
    // describes the view, and the server re-renders.
    let on_change = "evt.target.form.requestSubmit()";

    let period_opts: Vec<Html> = Period::ALL
        .iter()
        .map(|p| opt(p.as_str(), p.label(), *p == period))
        .collect();
    let source_opts: Vec<Html> = SOURCE_OPTIONS
        .iter()
        .map(|(value, label)| opt(value, label, *value == cur_source))
        .collect();
    let mut backend_opts: Vec<Html> = vec![opt("", "All backends", cur_backend.is_empty())];
    for b in backends {
        backend_opts.push(opt(b, b, *b == cur_backend));
    }
    // Preserve the admin "all users" scope across filter changes (the
    // form's GET would otherwise drop it). Empty for the self view.
    let scope_value = if show_all { "all" } else { "" };

    html! {
        form(method: "get", action: "/usage", class: "flex flex-wrap items-end gap-3") {
            input(type: "hidden", name: "scope", value: (scope_value));
            label(class: "form-control") {
                span(class: "label-text text-xs text-base-content/60") { "Period" }
                select(name: "period", class: "select select-bordered select-sm", "data-on:change": (on_change)) {
                    for o in period_opts.iter() { (o.clone()) }
                }
            }
            label(class: "form-control") {
                span(class: "label-text text-xs text-base-content/60") { "Source" }
                select(name: "source", class: "select select-bordered select-sm", "data-on:change": (on_change)) {
                    for o in source_opts.iter() { (o.clone()) }
                }
            }
            label(class: "form-control") {
                span(class: "label-text text-xs text-base-content/60") { "Backend" }
                select(name: "backend", class: "select select-bordered select-sm", "data-on:change": (on_change)) {
                    for o in backend_opts.iter() { (o.clone()) }
                }
            }
            // Fallback for clients without JS: an explicit apply.
            noscript {
                button(type: "submit", class: "btn btn-sm btn-primary") { "Apply" }
            }
        }
    }
    .to_html()
}

fn render_stats(show_all: bool, s: &usage::Summary) -> Html {
    let requests = fmt_int(s.requests);
    let tokens = fmt_int(s.total_tokens);
    let errors = fmt_int(s.errors);
    let users = fmt_int(s.unique_users);
    html! {
        div(class: "stats stats-vertical sm:stats-horizontal shadow bg-base-100 border border-base-300 w-full") {
            div(class: "stat") {
                div(class: "stat-title") { "Requests" }
                div(class: "stat-value text-2xl tabular-nums") { (requests) }
                div(class: "stat-desc") { "upstream backend calls" }
            }
            div(class: "stat") {
                div(class: "stat-title") { "Tokens" }
                div(class: "stat-value text-2xl tabular-nums") { (tokens) }
                div(class: "stat-desc") { "prompt + completion" }
            }
            if show_all {
                div(class: "stat") {
                    div(class: "stat-title") { "Users" }
                    div(class: "stat-value text-2xl tabular-nums") { (users) }
                    div(class: "stat-desc") { "active in range" }
                }
            }
            div(class: "stat") {
                div(class: "stat-title") { "Errors" }
                div(class: "stat-value text-2xl tabular-nums") { (errors) }
                div(class: "stat-desc") { "status \u{2265} 400" }
            }
        }
    }
    .to_html()
}

fn render_table(title: &str, key_header: &str, rows: &[GroupCount]) -> Html {
    let title = title.to_string();
    let key_header = key_header.to_string();
    let body: Vec<Html> = rows.iter().map(render_row).collect();
    html! {
        div(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-2 p-4") {
                h2(class: "card-title text-base") { (title) }
                if rows.is_empty() {
                    p(class: "text-base-content/60 text-sm") { "No activity in this range." }
                } else {
                    div(class: "overflow-x-auto") {
                        table(class: "table table-sm") {
                            thead {
                                tr {
                                    th { (key_header) }
                                    th(class: "text-right") { "Requests" }
                                    th(class: "text-right") { "Tokens" }
                                    th(class: "text-right") { "Errors" }
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

fn render_row(r: &GroupCount) -> Html {
    let label = if r.label.is_empty() {
        "—".to_string()
    } else {
        r.label.clone()
    };
    let requests = fmt_int(r.requests);
    let tokens = fmt_int(r.total_tokens);
    let errors = fmt_int(r.errors);
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
            td(class: (err_class)) { (errors) }
        }
    }
    .to_html()
}

/// Group an integer with thin spaces every three digits for readability
/// (locale-agnostic — no comma/period ambiguity across DE/EN).
fn fmt_int(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i != 0 && (len - i).is_multiple_of(3) {
            out.push('\u{202f}'); // narrow no-break space, every 3 from the right
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::fmt_int;

    #[test]
    fn fmt_int_groups_thousands() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(42), "42");
        assert_eq!(fmt_int(1234), "1\u{202f}234");
        assert_eq!(fmt_int(1234567), "1\u{202f}234\u{202f}567");
        assert_eq!(fmt_int(-1000), "-1\u{202f}000");
    }

    #[test]
    fn source_options_match_enum_strings() {
        // The hardcoded filter values must match the enum's wire strings,
        // else the dropdown silently filters nothing.
        use super::SOURCE_OPTIONS;
        use crate::server::db::usage::UsageSource;
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
