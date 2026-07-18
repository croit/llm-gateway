// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/groups` — the gateway-groups editor.
//!
//! Gateway groups are the DB-backed RBAC unit (see `db::gateway_groups`). This
//! page is where an operator maps OIDC claim values onto clean group names and
//! decides what each group grants: admin / default flags, built-in tools, and
//! skills. Which pools, RAG collections, and MCP connectors a group may reach
//! is set on those resources' own pages (they reference groups by name).
//!
//! Everything here is data, edited live — an edit reloads the RBAC resolver in
//! place (`AppState::reload_rbac`), so grants take effect on the next request
//! without a restart. Admin-gated like the other `/admin/*` pages.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};

use super::{
    NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, parse_csv, require_admin_or_403,
};
use session_core::chrome::{
    FlashKind, NavSections, Theme, is_datastar_request, read_body_to_bytes, see_other,
};
use session_core::i18n::{Lang, t};
use session_core::icons;

use crate::rama_server::state::RamaState;
use crate::server::db::{gateway_groups, skill_grants};

/// A group flattened for rendering: its definition plus its current grants.
struct GroupView {
    name: String,
    description: String,
    is_admin: bool,
    is_default: bool,
    oidc_values: Vec<String>,
    tools: Vec<String>,
    skills: Vec<String>,
}

/// GET /admin/groups — the create form + one edit card per existing group.
pub async fn groups_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let views = load_group_views(&state).await;
    let observed = gateway_groups::observed_oidc_values(&state.db)
        .await
        .unwrap_or_default();
    let mut tool_ids: Vec<String> = state.tools.ids().map(|s| s.to_string()).collect();
    tool_ids.sort();
    let skill_names: Vec<String> = state
        .skills
        .as_ref()
        .map(|s| s.current().names().map(|n| n.to_string()).collect())
        .unwrap_or_default();

    let body = render_body(lang, &views, &observed, &tool_ids, &skill_names);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    let title = t(lang, "groups-heading");
    nav_or_html_page(
        datastar,
        theme,
        lang,
        nav,
        NavItem::Groups,
        &title,
        &user.email,
        is_admin(&state, &user),
        state.user_skills_enabled(),
        session.impersonator_id.is_some(),
        body,
        "/admin/groups",
        &chat,
    )
}

async fn load_group_views(state: &RamaState) -> Vec<GroupView> {
    let groups = gateway_groups::list_groups(&state.db)
        .await
        .unwrap_or_default();
    let mut out = Vec::with_capacity(groups.len());
    for g in groups {
        let oidc_values = gateway_groups::mapped_values_for_group(&state.db, &g.name)
            .await
            .unwrap_or_default();
        let tools = gateway_groups::tools_for_group(&state.db, &g.name)
            .await
            .unwrap_or_default();
        let skills = skill_grants::skills_for_role(&state.db, &g.name)
            .await
            .unwrap_or_default();
        out.push(GroupView {
            name: g.name,
            description: g.description,
            is_admin: g.is_admin,
            is_default: g.is_default,
            oidc_values,
            tools,
            skills,
        });
    }
    out
}

#[derive(serde::Deserialize)]
struct SaveForm {
    /// The group name being created or edited. Immutable key: editing keeps it,
    /// creating supplies a fresh one.
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    is_admin: String,
    #[serde(default)]
    is_default: String,
    #[serde(default)]
    oidc_values: String,
    #[serde(default)]
    tools: String,
    #[serde(default)]
    skills: String,
}

/// POST /admin/groups/save — upsert a group and replace its OIDC mappings, tool
/// grants, and skill grants, then reload the resolver.
pub async fn groups_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return super::toast(FlashKind::Error, msg),
    };
    let form: SaveForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(_) => return super::toast(FlashKind::Error, t(lang, "admin-malformed-form")),
    };
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return super::toast(FlashKind::Error, t(lang, "groups-error-name-required"));
    }

    let is_admin_flag = super::checkbox_on(&form.is_admin);
    let is_default_flag = super::checkbox_on(&form.is_default);
    if let Err(err) = gateway_groups::upsert_group(
        &state.db,
        &name,
        form.description.trim(),
        is_admin_flag,
        is_default_flag,
    )
    .await
    {
        return super::toast(FlashKind::Error, err.to_string());
    }
    if let Err(err) =
        gateway_groups::set_mappings_for_group(&state.db, &name, &parse_csv(&form.oidc_values))
            .await
    {
        return super::toast(FlashKind::Error, err.to_string());
    }
    if let Err(err) =
        gateway_groups::set_tools_for_group(&state.db, &name, &parse_csv(&form.tools)).await
    {
        return super::toast(FlashKind::Error, err.to_string());
    }
    if let Err(err) =
        skill_grants::set_skills_for_role(&state.db, &name, &parse_csv(&form.skills)).await
    {
        return super::toast(FlashKind::Error, err.to_string());
    }

    state.reload_rbac().await;
    see_other("/admin/groups")
}

#[derive(serde::Deserialize)]
struct DeleteForm {
    name: String,
}

/// POST /admin/groups/delete — remove a group (cascades its mappings + grants),
/// then reload the resolver.
pub async fn groups_delete(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return super::toast(FlashKind::Error, msg),
    };
    let form: DeleteForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(_) => return super::toast(FlashKind::Error, t(lang, "admin-malformed-form")),
    };
    // Deleting a group also clears its skill grants (kept in `skill_role_grants`,
    // which has no FK to `gateway_groups`).
    let _ = skill_grants::set_skills_for_role(&state.db, form.name.trim(), &[]).await;
    if let Err(err) = gateway_groups::delete_group(&state.db, form.name.trim()).await {
        return super::toast(FlashKind::Error, err.to_string());
    }
    state.reload_rbac().await;
    see_other("/admin/groups")
}

// ---------------------------------------------------------------- rendering

fn render_body(
    lang: Lang,
    views: &[GroupView],
    observed: &[String],
    tool_ids: &[String],
    skill_names: &[String],
) -> Html {
    let cards: Vec<Html> = views.iter().map(|g| group_card(lang, g)).collect();
    html! {
        section(class: "max-w-4xl mx-auto p-4 sm:p-6 flex flex-col gap-6") {
            header(class: "flex flex-col gap-1") {
                h1(class: "text-2xl font-bold") { (t(lang, "groups-heading")) }
                p(class: "text-base-content/70 text-sm") { (t(lang, "groups-intro")) }
            }
            (datalists(observed, tool_ids, skill_names))
            (group_form(lang, None))
            div(class: "flex flex-col gap-4") {
                h2(class: "text-lg font-semibold") { (t(lang, "groups-existing-heading")) }
                if views.is_empty() {
                    p(class: "text-base-content/60 text-sm") { (t(lang, "groups-empty")) }
                } else {
                    for c in cards.iter() { (c.clone()) }
                }
            }
        }
    }
    .to_html()
}

/// The three shared `<datalist>`s (observed OIDC values, registered tools,
/// loaded skills) that every group form's inputs reference for autocomplete.
fn datalists(observed: &[String], tool_ids: &[String], skill_names: &[String]) -> Html {
    html! {
        datalist(id: "oidc-values") {
            for v in observed.iter() { option(value: (v.clone())) {} }
        }
        datalist(id: "tool-ids") {
            for v in tool_ids.iter() { option(value: (v.clone())) {} }
        }
        datalist(id: "skill-names") {
            for v in skill_names.iter() { option(value: (v.clone())) {} }
        }
    }
    .to_html()
}

fn group_card(lang: Lang, g: &GroupView) -> Html {
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3 p-4") {
                (group_form(lang, Some(g)))
                form(method: "post", action: "/admin/groups/delete", class: "flex justify-end") {
                    input(type: "hidden", name: "name", value: (g.name.clone()));
                    button(
                        type: "submit",
                        class: "btn btn-sm btn-ghost text-error",
                        onclick: "return confirm('Delete this group?')"
                    ) {
                        (icons::trash(16))
                        (t(lang, "groups-delete"))
                    }
                }
            }
        }
    }
    .to_html()
}

/// The create/edit form. `existing = None` renders an empty "new group" form;
/// `Some(g)` pre-fills it and locks the name (the key). A standalone helper so
/// the `html!` macro never captures caller locals into per-attribute closures.
fn group_form(lang: Lang, existing: Option<&GroupView>) -> Html {
    let name = existing.map(|g| g.name.clone()).unwrap_or_default();
    let description = existing.map(|g| g.description.clone()).unwrap_or_default();
    let is_admin = existing.map(|g| g.is_admin).unwrap_or(false);
    let is_default = existing.map(|g| g.is_default).unwrap_or(false);
    let oidc = existing
        .map(|g| g.oidc_values.join(", "))
        .unwrap_or_default();
    let tools = existing.map(|g| g.tools.join(", ")).unwrap_or_default();
    let skills = existing.map(|g| g.skills.join(", ")).unwrap_or_default();
    let is_edit = existing.is_some();
    let heading = if is_edit {
        name.clone()
    } else {
        t(lang, "groups-new-heading")
    };
    html! {
        form(method: "post", action: "/admin/groups/save", class: "flex flex-col gap-3") {
            h3(class: "font-semibold") { (heading) }
            label(class: "form-control") {
                span(class: "label-text") { (t(lang, "groups-field-name")) }
                (name_input(&name, is_edit))
            }
            label(class: "form-control") {
                span(class: "label-text") { (t(lang, "groups-field-description")) }
                input(type: "text", name: "description", value: (description), class: "input input-bordered input-sm");
            }
            div(class: "flex gap-4 flex-wrap") {
                (super::bool_checkbox("is_admin", "1", &t(lang, "groups-field-admin"), is_admin, false))
                (super::bool_checkbox("is_default", "1", &t(lang, "groups-field-default"), is_default, false))
            }
            label(class: "form-control") {
                span(class: "label-text") { (t(lang, "groups-field-oidc")) }
                span(class: "label-text-alt text-base-content/60") { (t(lang, "groups-field-oidc-help")) }
                input(type: "text", name: "oidc_values", value: (oidc), list: "oidc-values", class: "input input-bordered input-sm", placeholder: "grp-dev, CN=devs,OU=…");
            }
            label(class: "form-control") {
                span(class: "label-text") { (t(lang, "groups-field-tools")) }
                input(type: "text", name: "tools", value: (tools), list: "tool-ids", class: "input input-bordered input-sm", placeholder: "*");
            }
            label(class: "form-control") {
                span(class: "label-text") { (t(lang, "groups-field-skills")) }
                input(type: "text", name: "skills", value: (skills), list: "skill-names", class: "input input-bordered input-sm", placeholder: "*");
            }
            div(class: "flex justify-end") {
                button(type: "submit", class: "btn btn-sm btn-primary") { (t(lang, "groups-save")) }
            }
        }
    }
    .to_html()
}

/// Name input: editable + required when creating; readonly (the immutable key)
/// when editing. Standalone so the presence-only `readonly` attribute is
/// emitted correctly (see `pk_name_input` rationale in `pages/mod.rs`).
fn name_input(value: &str, readonly: bool) -> Html {
    if readonly {
        html! {
            input(type: "text", name: "name", value: (value.to_string()), readonly: "readonly", class: "input input-bordered input-sm font-mono bg-base-200");
        }
        .to_html()
    } else {
        html! {
            input(type: "text", name: "name", value: (value.to_string()), required: "required", class: "input input-bordered input-sm font-mono", placeholder: "developers");
        }
        .to_html()
    }
}
