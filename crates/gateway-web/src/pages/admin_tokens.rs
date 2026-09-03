// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/tokens` — every API token in the deployment with its owner.
//!
//! The same view a user gets of their own tokens on `/tokens`, widened to the
//! whole roster and with the owner spelled out: name, owner, created / last
//! used / expires, state, this month's usage, the model allowlist, and any
//! per-token quota.
//!
//! **The token itself is not here, and cannot be.** Only a SHA-256 of the
//! plaintext is ever stored (`tokens.hash`), so there is nothing to reveal —
//! this page is a register of which credentials exist and what they cost, not
//! a way to recover one. An admin who needs a working token for someone mints
//! a new one, or impersonates and rotates.
//!
//! Read-only by design. Editing a token's allowlist or quota from here would
//! need a second set of write paths with a different ownership rule than the
//! self-service ones on `/tokens`; an admin who must change one can set the
//! quota on `/admin/limits` (subject `token`) or impersonate the owner. What
//! this page owes its reader is the whole picture in one place.

use std::collections::HashMap;
use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};

use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_admin_or_403};
use session_core::chrome::{NavSections, Theme, is_datastar_request};
use session_core::i18n::{self, Lang, t, t_args};

use gateway_core::server::db::limits::{LimitRule, SubjectType};
use gateway_core::server::db::{limits, token_models, tokens, usage};
use gateway_runtime::rama_server::state::RamaState;

/// GET /admin/tokens — the deployment-wide token register.
pub async fn admin_tokens_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, admin) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let all = match tokens::list_all_with_owner(&state.db).await {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(error = %err, "listing all tokens");
            return super::internal_error_html(&admin.email, "could not list tokens");
        }
    };

    // Month-to-date usage for every token at once, in the admin's timezone.
    let tz = super::viewer_tz(&session, &admin);
    let now = jiff::Timestamp::now();
    let bounds = usage::period_bounds(usage::Period::ThisMonth, &tz, now);
    let filter = usage::Filter {
        source: None,
        backend: None,
        // No user scope: this is the whole deployment.
        user_id: None,
        token_id: None,
    };
    let agg = usage::aggregate(
        &state.db,
        bounds,
        &filter,
        state.config().usage.retention_days,
        now,
        false,
    )
    .await
    .unwrap_or_default();
    let by_token: HashMap<&str, &usage::GroupCount> =
        agg.by_token.iter().map(|g| (g.key.as_str(), g)).collect();

    let allowlists = token_models::all(&state.db).await.unwrap_or_default();
    let usage_on = state.usage.is_enabled();
    let rules = limits::list_all(&state.db).await.unwrap_or_default();
    // Indexed once: scanning every rule per token row is O(tokens × rules).
    let mut rules_by_token: HashMap<&str, Vec<&LimitRule>> = HashMap::new();
    for r in rules
        .iter()
        .filter(|r| r.subject_type == SubjectType::Token)
    {
        rules_by_token
            .entry(r.subject_id.as_str())
            .or_default()
            .push(r);
    }
    let currency = state.config().usage.currency.clone();

    let rows: Vec<TokenRow> = all
        .iter()
        .map(|t| {
            let u = by_token.get(t.id.as_str());
            TokenRow {
                name: t.name.clone(),
                id: t.id.clone(),
                owner: t.user_email.clone(),
                created: t.created_at.strftime("%Y-%m-%d").to_string(),
                last_used: t
                    .last_used_at
                    .map(|lu| lu.strftime("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| session_core::i18n::t(lang, "tokens-last-used-never")),
                expires: t.expires_at.strftime("%Y-%m-%d").to_string(),
                revoked: t.revoked_at.is_some(),
                // An expired token authenticates no better than a revoked
                // one, and an admin scanning for live credentials needs to
                // see the difference at a glance.
                expired: t.revoked_at.is_none() && t.expires_at < now,
                // `None` is "not recording", which is not the same as zero —
                // the same distinction /tokens draws, drawn the same way.
                usage: usage_on.then(|| u.map(|g| super::TokenUsage::from(*g)).unwrap_or_default()),
                models: allowlists.get(&t.id).cloned(),
                limits: rules_by_token
                    .get(t.id.as_str())
                    .map(|v| v.iter().map(|r| (*r).clone()).collect())
                    .unwrap_or_default(),
            }
        })
        .collect();

    let body = render_body(lang, &rows, &currency);
    let chat = fetch_sidebar_chat(&state, &admin.id, None).await;
    let title = t(lang, "admin-tokens-page-title");
    {
        let pctx = super::PageCtx {
            theme,
            lang,
            nav,
            datastar,
            user_email: admin.email.clone(),
            is_admin: is_admin(&state, &admin),
            skills_enabled: state.user_skills_enabled(),
            impersonating: session.impersonator_id.is_some(),
        };
        nav_or_html_page(
            &pctx,
            NavItem::AdminTokens,
            &title,
            body,
            "/admin/tokens",
            &chat,
        )
    }
}

struct TokenRow {
    name: String,
    id: String,
    owner: String,
    created: String,
    last_used: String,
    expires: String,
    revoked: bool,
    expired: bool,
    /// `None` = usage recording is off, so the numbers are unknown rather
    /// than zero.
    usage: Option<super::TokenUsage>,
    /// `None` = unrestricted, the default.
    models: Option<Vec<String>>,
    limits: Vec<LimitRule>,
}

fn render_body(lang: Lang, rows: &[TokenRow], currency: &str) -> Html {
    let heading = t(lang, "admin-tokens-heading");
    let blurb = t(lang, "admin-tokens-blurb");
    let body: Vec<Html> = rows.iter().map(|r| render_row(lang, r, currency)).collect();
    let count = t_args(
        lang,
        "admin-tokens-count",
        &i18n::args([("count", (rows.len() as i64).into())]),
    );
    html! {
        section(class: "max-w-6xl mx-auto p-4 sm:p-6 pt-14 sm:pt-6 flex flex-col gap-4") {
            header(class: "flex flex-col gap-2") {
                h1(class: "text-2xl font-bold") { (heading) }
                p(class: "text-base-content/70 text-sm") { (blurb) }
            }
            article(class: "card border border-base-300 bg-base-100") {
                div(class: "card-body gap-2 p-4") {
                    if rows.is_empty() {
                        p(class: "text-base-content/60 text-sm") { (t(lang, "admin-tokens-none")) }
                    } else {
                        div(class: "overflow-x-auto") {
                            table(class: "table table-sm") {
                                thead {
                                    tr {
                                        th { (t(lang, "admin-tokens-col-name")) }
                                        th { (t(lang, "admin-tokens-col-owner")) }
                                        th { (t(lang, "admin-tokens-col-state")) }
                                        th { (t(lang, "admin-tokens-col-dates")) }
                                        th(class: "text-right") { (t(lang, "usage-col-requests")) }
                                        th(class: "text-right") { (t(lang, "usage-col-tokens")) }
                                        th(class: "text-right") { (t(lang, "usage-col-cost")) }
                                        th { (t(lang, "admin-tokens-col-scope")) }
                                    }
                                }
                                tbody {
                                    for r in body.iter() { (r.clone()) }
                                }
                            }
                        }
                        p(class: "text-xs text-base-content/60") { (count) }
                    }
                }
            }
        }
    }
    .to_html()
}

fn render_row(lang: Lang, r: &TokenRow, currency: &str) -> Html {
    let dates = t_args(
        lang,
        // The same labelled triple the owner sees on /tokens, rather than a
        // second translated string that says less.
        "tokens-row-meta",
        &i18n::args([
            ("created", r.created.clone().into()),
            ("last_used", r.last_used.clone().into()),
            ("expires", r.expires.clone().into()),
        ]),
    );
    // Usage recording off means the numbers below are not zero, they are
    // unknown — say so rather than implying the token is idle.
    let (requests, tokens, cost) = match &r.usage {
        Some(u) => (
            super::fmt_int(u.requests),
            super::fmt_int(u.tokens),
            super::fmt_cost(u.cost, currency),
        ),
        None => (
            super::DASH.to_string(),
            super::DASH.to_string(),
            super::DASH.to_string(),
        ),
    };
    let scope = render_scope(lang, r, currency);
    html! {
        tr {
            td {
                div(class: "font-medium") { (r.name.clone()) }
                div(class: "text-xs text-base-content/50 font-mono break-all") { (r.id.clone()) }
            }
            td(class: "break-all") { (r.owner.clone()) }
            td { (render_state(lang, r)) }
            td(class: "text-xs text-base-content/70") { (dates) }
            td(class: "text-right tabular-nums") { (requests) }
            td(class: "text-right tabular-nums") { (tokens) }
            td(class: "text-right tabular-nums") { (cost) }
            td { (scope) }
        }
    }
    .to_html()
}

fn render_state(lang: Lang, r: &TokenRow) -> Html {
    if r.revoked {
        return html! { span(class: "badge badge-error") { (t(lang, "tokens-badge-revoked")) } }
            .to_html();
    }
    if r.expired {
        return html! { span(class: "badge badge-warning") { (t(lang, "admin-tokens-badge-expired")) } }
            .to_html();
    }
    html! { span(class: "badge badge-secondary") { (t(lang, "tokens-badge-active")) } }.to_html()
}

/// The token's scope column: its model allowlist and its own quota rules —
/// the two things that make one token different from another.
fn render_scope(lang: Lang, r: &TokenRow, currency: &str) -> Html {
    let models = match &r.models {
        None => t(lang, "limits-all-models"),
        Some(list) => list.join(", "),
    };
    let model_class = if r.models.is_none() {
        "text-xs text-base-content/50"
    } else {
        "text-xs font-mono break-all"
    };
    let rules: Vec<String> = r
        .limits
        .iter()
        .map(|rule| super::describe_rule(lang, rule, currency))
        .collect();
    html! {
        div(class: "flex flex-col gap-1") {
            div(class: (model_class)) { (models) }
            if !rules.is_empty() {
                div(class: "text-xs text-base-content/70") { (rules.join(" · ")) }
            }
        }
    }
    .to_html()
}
