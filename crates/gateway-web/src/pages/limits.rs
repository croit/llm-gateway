// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/limits` — the rate-limit / quota editor.
//!
//! One page to add, update, and delete [`db::limits`] rules at the global,
//! per-role, and per-user levels. Rules are data (not config), so this is the
//! only place they're managed; the enforcement itself lives in
//! `server::limits`. Admin-gated like the other `/admin/*` pages.

use std::collections::HashMap;
use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};

use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_admin_or_403};
use session_core::chrome::{
    Flash, FlashKind, NavSections, Theme, is_datastar_request, read_body_to_bytes, sse_patch,
    sse_response, sse_toast,
};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

use gateway_core::rama_server::state::RamaState;
use gateway_core::server::db::limits::{self, Dimension, LimitRule, SubjectType, Window};
use gateway_core::server::db::users;

/// GET /admin/limits — the add-form + a table of every configured rule.
pub async fn limits_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let rules = limits::list_all(&state.db).await.unwrap_or_default();
    let roster = users::list_all(&state.db).await.unwrap_or_default();
    let role_ids: Vec<String> = state.config.roles.iter().map(|r| r.id.clone()).collect();
    // Every advertised model across all pools/kinds — the scope dropdown.
    let models = state.upstreams.all_models();
    let currency = &state.config.usage.currency;

    let body = render_body(lang, currency, &rules, &role_ids, &roster, &models);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    let title = t(lang, "limits-heading");
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
        nav_or_html_page(&pctx, NavItem::Limits, &title, body, "/admin/limits", &chat)
    }
}

#[derive(serde::Deserialize)]
struct SaveForm {
    subject_type: String,
    #[serde(default)]
    subject_id: String,
    #[serde(default)]
    model: String,
    dimension: String,
    window: String,
    #[serde(default)]
    value: String,
}

/// POST /admin/limits — add or update a rule, then patch the table in place.
pub async fn limits_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
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

    let Some(subject_type) = SubjectType::parse(&form.subject_type) else {
        return toast(FlashKind::Error, t(lang, "admin-malformed-form"));
    };
    let Some(dimension) = Dimension::parse(&form.dimension) else {
        return toast(FlashKind::Error, t(lang, "admin-malformed-form"));
    };
    let Some(window) = Window::parse(&form.window) else {
        return toast(FlashKind::Error, t(lang, "admin-malformed-form"));
    };
    let value = match form.value.trim().parse::<f64>() {
        Ok(v) if v.is_finite() && v >= 0.0 => v,
        _ => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "limits-invalid-value",
                    &i18n::args([("value", form.value.clone().into())]),
                ),
            );
        }
    };

    // Resolve the subject id: global ignores it; role validates against the
    // configured roles; user accepts an id or an email and resolves to the id.
    let (subject_id, subject_label) = match subject_type {
        SubjectType::Global => (String::new(), t(lang, "limits-subject-global")),
        SubjectType::Role => {
            let id = form.subject_id.trim();
            if id.is_empty() {
                return toast(FlashKind::Error, t(lang, "limits-missing-subject-id"));
            }
            if !state.config.roles.iter().any(|r| r.id == id) {
                return toast(
                    FlashKind::Error,
                    t_args(
                        lang,
                        "limits-unknown-role",
                        &i18n::args([("role", id.to_string().into())]),
                    ),
                );
            }
            (id.to_string(), id.to_string())
        }
        SubjectType::User => {
            let needle = form.subject_id.trim();
            if needle.is_empty() {
                return toast(FlashKind::Error, t(lang, "limits-missing-subject-id"));
            }
            let roster = users::list_all(&state.db).await.unwrap_or_default();
            match roster
                .iter()
                .find(|u| u.id == needle || u.email.eq_ignore_ascii_case(needle))
            {
                Some(u) => (u.id.clone(), u.email.clone()),
                None => {
                    return toast(
                        FlashKind::Error,
                        t_args(
                            lang,
                            "limits-unknown-user",
                            &i18n::args([("user", needle.to_string().into())]),
                        ),
                    );
                }
            }
        }
    };

    let model = {
        let m = form.model.trim();
        (!m.is_empty()).then(|| m.to_string())
    };

    if let Err(err) = limits::upsert(
        &state.db,
        subject_type,
        &subject_id,
        model.as_deref(),
        dimension,
        window,
        value,
    )
    .await
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

    patch_table_response(
        &state,
        lang,
        FlashKind::Success,
        t_args(
            lang,
            "limits-saved",
            &i18n::args([("subject", subject_label.into())]),
        ),
    )
    .await
}

#[derive(serde::Deserialize)]
struct DeleteForm {
    id: String,
}

/// POST /admin/limits/delete — remove a rule by id, then patch the table.
pub async fn limits_delete(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: DeleteForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(_) => return toast(FlashKind::Error, t(lang, "admin-malformed-form")),
    };
    if let Err(err) = limits::delete(&state.db, &form.id).await {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-delete-error",
                &i18n::args([("err", err.to_string().into())]),
            ),
        );
    }
    patch_table_response(&state, lang, FlashKind::Success, t(lang, "limits-deleted")).await
}

/// Re-render the rules table and return it as an in-place patch plus a toast,
/// so an add / update / delete reflects immediately without a full reload.
async fn patch_table_response(
    state: &RamaState,
    lang: Lang,
    kind: FlashKind,
    message: String,
) -> Response {
    let rules = limits::list_all(&state.db).await.unwrap_or_default();
    let roster = users::list_all(&state.db).await.unwrap_or_default();
    let currency = &state.config.usage.currency;
    let table = render_table(lang, currency, &rules, &roster).to_string();
    sse_response(&[
        sse_toast(&Flash { kind, message }),
        sse_patch(Some("#limits-table"), Some("inner"), &table),
    ])
}

fn toast(kind: FlashKind, message: impl Into<String>) -> Response {
    sse_response(&[sse_toast(&Flash {
        kind,
        message: message.into(),
    })])
}

// ---------------------------------------------------------------- rendering

#[allow(clippy::too_many_arguments)]
fn render_body(
    lang: Lang,
    currency: &str,
    rules: &[LimitRule],
    role_ids: &[String],
    roster: &[users::User],
    models: &[String],
) -> Html {
    html! {
        section(class: "max-w-5xl mx-auto p-4 sm:p-6 flex flex-col gap-4") {
            header(class: "flex flex-col gap-1") {
                h1(class: "text-2xl font-bold") { (t(lang, "limits-heading")) }
                p(class: "text-base-content/70 text-sm") { (t(lang, "limits-intro")) }
            }
            (render_add_form(lang, currency, role_ids, roster, models))
            div(id: "limits-table") {
                (render_table(lang, currency, rules, roster))
            }
        }
    }
    .to_html()
}

fn opt(value: &str, label: &str) -> Html {
    html! { option(value: (value.to_string())) { (label.to_string()) } }.to_html()
}

fn render_add_form(
    lang: Lang,
    currency: &str,
    role_ids: &[String],
    roster: &[users::User],
    models: &[String],
) -> Html {
    let action = "/admin/limits";
    let submit = format!("@post('{action}', {{contentType: 'form'}})");
    let cost_label = t_args(
        lang,
        "limits-dim-cost",
        &i18n::args([("cur", currency.to_string().into())]),
    );
    // Suggestions for the subject id: every configured role id + every known
    // user email, offered via a datalist (role ids and emails don't collide in
    // practice, and the handler validates whichever the admin picks).
    let mut suggestions: Vec<Html> = role_ids.iter().map(|r| opt_bare(r)).collect();
    suggestions.extend(roster.iter().map(|u| opt_bare(&u.email)));

    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                h2(class: "card-title text-base") { (t(lang, "limits-add-heading")) }
                form(
                    method: "post",
                    action: (action),
                    "data-on:submit__prevent": (submit),
                    class: "grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3 m-0"
                ) {
                    label(class: "flex flex-col gap-1") {
                        span(class: "label-text text-xs opacity-70") { (t(lang, "limits-field-subject")) }
                        select(name: "subject_type", class: "select select-bordered select-sm") {
                            (opt("global", &t(lang, "limits-subject-global")))
                            (opt("role", &t(lang, "limits-subject-role")))
                            (opt("user", &t(lang, "limits-subject-user")))
                        }
                    }
                    label(class: "flex flex-col gap-1") {
                        span(class: "label-text text-xs opacity-70") { (t(lang, "limits-field-subject-id")) }
                        input(
                            type: "text", name: "subject_id", list: "limits-subjects",
                            placeholder: (t(lang, "limits-field-subject-id-ph")),
                            class: "input input-bordered input-sm"
                        );
                        datalist(id: "limits-subjects") {
                            for s in suggestions.iter() { (s.clone()) }
                        }
                    }
                    label(class: "flex flex-col gap-1") {
                        span(class: "label-text text-xs opacity-70") { (t(lang, "limits-field-model")) }
                        select(name: "model", class: "select select-bordered select-sm") {
                            // Empty value = the all-models aggregate (the common case).
                            (opt("", &t(lang, "limits-all-models")))
                            for m in models.iter() {
                                (opt(m, m))
                            }
                        }
                    }
                    label(class: "flex flex-col gap-1") {
                        span(class: "label-text text-xs opacity-70") { (t(lang, "limits-field-dimension")) }
                        select(name: "dimension", class: "select select-bordered select-sm") {
                            (opt("requests", &t(lang, "limits-dim-requests")))
                            (opt("tokens", &t(lang, "limits-dim-tokens")))
                            (opt("cost", &cost_label))
                        }
                    }
                    label(class: "flex flex-col gap-1") {
                        span(class: "label-text text-xs opacity-70") { (t(lang, "limits-field-window")) }
                        select(name: "window", class: "select select-bordered select-sm") {
                            (opt("hour", &t(lang, "limits-win-hour")))
                            (opt("day", &t(lang, "limits-win-day")))
                            (opt("week", &t(lang, "limits-win-week")))
                            (opt("month", &t(lang, "limits-win-month")))
                        }
                    }
                    label(class: "flex flex-col gap-1") {
                        span(class: "label-text text-xs opacity-70") { (t(lang, "limits-field-value")) }
                        input(
                            type: "number", name: "value", min: "0", step: "any", required: "required",
                            class: "input input-bordered input-sm"
                        );
                    }
                    div(class: "flex items-end") {
                        button(type: "submit", class: "btn btn-primary btn-sm") {
                            (icons::check(14))
                            span { (t(lang, "limits-add-submit")) }
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

fn opt_bare(value: &str) -> Html {
    html! { option(value: (value.to_string())) {} }.to_html()
}

fn render_table(lang: Lang, currency: &str, rules: &[LimitRule], roster: &[users::User]) -> Html {
    // id → email, so a per-user rule shows the friendly address.
    let emails: HashMap<&str, &str> = roster
        .iter()
        .map(|u| (u.id.as_str(), u.email.as_str()))
        .collect();
    let rows: Vec<Html> = rules
        .iter()
        .map(|r| render_row(lang, currency, r, &emails))
        .collect();
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-2 p-4") {
                if rules.is_empty() {
                    p(class: "text-base-content/60 text-sm") { (t(lang, "limits-none")) }
                } else {
                    div(class: "overflow-x-auto") {
                        table(class: "table table-sm") {
                            thead {
                                tr {
                                    th { (t(lang, "limits-col-subject")) }
                                    th { (t(lang, "limits-col-scope")) }
                                    th { (t(lang, "limits-col-limit")) }
                                    th { (t(lang, "limits-col-window")) }
                                    th(class: "text-right") { (t(lang, "limits-col-value")) }
                                    th(class: "text-right") { (t(lang, "limits-col-actions")) }
                                }
                            }
                            tbody {
                                for r in rows.iter() { (r.clone()) }
                            }
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

fn dim_label(lang: Lang, d: Dimension) -> String {
    match d {
        Dimension::Requests => t(lang, "limits-dim-requests"),
        Dimension::Tokens => t(lang, "limits-dim-tokens"),
        Dimension::Cost => t(lang, "limits-dim-cost-short"),
    }
}

fn win_label(lang: Lang, w: Window) -> String {
    match w {
        Window::Hour => t(lang, "limits-win-hour"),
        Window::Day => t(lang, "limits-win-day"),
        Window::Week => t(lang, "limits-win-week"),
        Window::Month => t(lang, "limits-win-month"),
    }
}

fn subject_label(lang: Lang, r: &LimitRule, emails: &HashMap<&str, &str>) -> String {
    match r.subject_type {
        SubjectType::Global => t(lang, "limits-subject-global"),
        SubjectType::Role => format!("{}: {}", t(lang, "limits-subject-role"), r.subject_id),
        SubjectType::User => {
            let who = emails
                .get(r.subject_id.as_str())
                .copied()
                .unwrap_or(r.subject_id.as_str());
            format!("{}: {}", t(lang, "limits-subject-user"), who)
        }
    }
}

/// Group an integer's digits with a thin no-break space every three places
/// (locale-agnostic — dodges the comma/period ambiguity across DE/EN). Matches
/// the `/usage` page's `fmt_int`. `1000000` → `1 000 000`.
fn group_int(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i != 0 && (len - i).is_multiple_of(3) {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    out
}

/// Format a rule's value for the table: cost with two decimals + currency,
/// requests/tokens as grouped whole numbers (`5 000 000`, not `5000000`).
fn fmt_value(r: &LimitRule, currency: &str) -> String {
    match r.dimension {
        Dimension::Cost => {
            let whole = r.value.trunc() as i64;
            let cents = ((r.value - r.value.trunc()) * 100.0).round().abs() as i64;
            format!("{}.{:02}\u{202f}{}", group_int(whole), cents, currency)
        }
        _ => group_int(r.value as i64),
    }
}

fn render_row(lang: Lang, currency: &str, r: &LimitRule, emails: &HashMap<&str, &str>) -> Html {
    let del = "/admin/limits/delete";
    let del_submit = format!("@post('{del}', {{contentType: 'form'}})");
    let scope = r
        .model
        .clone()
        .unwrap_or_else(|| t(lang, "limits-all-models"));
    html! {
        tr {
            td { (subject_label(lang, r, emails)) }
            td(class: "font-mono break-all") { (scope) }
            td { (dim_label(lang, r.dimension)) }
            td { (win_label(lang, r.window)) }
            td(class: "text-right tabular-nums") { (fmt_value(r, currency)) }
            td(class: "text-right") {
                form(method: "post", action: (del), "data-on:submit__prevent": (del_submit), class: "m-0 inline") {
                    input(type: "hidden", name: "id", value: (r.id.clone()));
                    button(type: "submit", class: "btn btn-ghost btn-xs text-error", title: (t(lang, "limits-delete"))) {
                        (icons::trash(14))
                    }
                }
            }
        }
    }
    .to_html()
}
