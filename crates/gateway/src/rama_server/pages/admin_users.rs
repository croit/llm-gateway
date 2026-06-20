// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/users` — the registered-user roster plus admin impersonation.
//!
//! Three handlers:
//!   - `GET  /admin/users`           — list everyone who has ever signed
//!     in, with their raw OIDC groups and *resolved* RBAC role IDs, plus
//!     an Impersonate button per row and the recent impersonation trail.
//!   - `POST /admin/users/impersonate` — start acting as another user.
//!   - `POST /impersonate/stop`      — return to your own account.
//!
//! The roster + start are gated on the `admin` role via
//! [`super::require_admin_or_403`]. Stop is deliberately *not* admin-gated:
//! the active identity during impersonation is the target (who may be a
//! non-admin), and they must always be able to get back out.
//!
//! Why the target id rides in a POST body rather than the URL path: rama's
//! router lowercases path segments, which would mangle case-sensitive OIDC
//! subjects. The `/admin/models` page dodges the same trap the same way.
//!
//! Full impersonation is unrestricted by design — while impersonating, the
//! admin can take any action as the target, including minting tokens. The
//! persistent banner (rendered by the shared page chrome) and the
//! append-only `impersonation_audit` table are the accountability controls.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response, StatusCode, header};
use serde::Deserialize;

use super::{
    NavItem, fetch_sidebar_chat, forbidden_html, is_admin, nav_or_html_page, require_admin_or_403,
    require_session_or_redirect,
};
use session_core::chrome::{
    NavSections, Theme, is_datastar_request, read_body_to_bytes, see_other,
};
use session_core::icons;

use crate::rama_server::session::COOKIE_NAME;
use crate::rama_server::state::RamaState;
use crate::server::db::{audit, users};

/// GET /admin/users — the roster + recent impersonation trail.
pub async fn users_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, admin) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let all = match users::list_all(&state.db).await {
        Ok(u) => u,
        Err(err) => {
            tracing::warn!(error = %err, "listing users");
            return super::internal_error_html(&admin.email, "could not list users");
        }
    };
    let rows: Vec<UserRow> = all
        .iter()
        .map(|u| UserRow {
            id: u.id.clone(),
            email: u.email.clone(),
            name: u.name.clone().unwrap_or_default(),
            oidc_roles: join_or(&u.roles, "none"),
            rbac_roles: join_or(&state.rbac.role_ids_for(&u.roles), "none granted"),
            created: u.created_at.strftime("%Y-%m-%d").to_string(),
            is_self: u.id == admin.id,
        })
        .collect();
    let events = audit::recent(&state.db, 20).await.unwrap_or_default();

    let allow_impersonation = state.config.gateway.allow_impersonation;
    let body = render_body(&rows, &events, allow_impersonation);
    let chat = fetch_sidebar_chat(&state, &admin.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        nav,
        NavItem::Users,
        "Users — LLM Gateway",
        &admin.email,
        is_admin(&state, &admin),
        session.impersonator_id.is_some(),
        body,
        "/admin/users",
        &chat,
    )
}

#[derive(Deserialize)]
struct ImpersonateForm {
    user_id: String,
}

/// POST /admin/users/impersonate — begin acting as `user_id`. Mints an
/// impersonation session (target as `user_id`, admin as `impersonator_id`),
/// swaps the cookie, audits the start, and lands the admin on `/` as a
/// full navigation so the shell re-renders with the banner.
pub async fn users_impersonate(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (session, admin) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    // Kill switch: when impersonation is disabled gateway-wide, the button is
    // already hidden, but reject the POST too so a hand-crafted request can't
    // start one.
    if !state.config.gateway.allow_impersonation {
        return forbidden_html(&admin.email, "Impersonation is disabled on this gateway.");
    }
    // Refuse to nest: an admin already impersonating must stop first.
    // (require_admin_or_403 already blocks the common nesting path — the
    // active identity is the target, usually a non-admin — but an admin
    // impersonating another admin would otherwise slip through.)
    if session.impersonator_id.is_some() {
        return forbidden_html(
            &admin.email,
            "Already impersonating — return to your account before starting another.",
        );
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return forbidden_html(&admin.email, &msg),
    };
    let form: ImpersonateForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => return forbidden_html(&admin.email, &format!("malformed form: {err}")),
    };
    if form.user_id == admin.id {
        return forbidden_html(&admin.email, "You can't impersonate yourself.");
    }
    let target = match users::find_by_id(&state.db, &form.user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => return forbidden_html(&admin.email, "No such user."),
        Err(err) => {
            tracing::warn!(error = %err, "impersonate: target lookup");
            return super::internal_error_html(&admin.email, "could not look up user");
        }
    };

    let new_session = match state
        .sessions
        .create_impersonation(&target.id, &admin.id)
        .await
    {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "impersonate: minting session");
            return super::internal_error_html(&admin.email, "could not start impersonation");
        }
    };
    if let Err(err) = audit::record(
        &state.db,
        &admin.id,
        &admin.email,
        &target.id,
        &target.email,
        audit::Action::Start,
    )
    .await
    {
        tracing::warn!(error = %err, "impersonate: audit start");
    }
    tracing::info!(
        actor = %admin.id, actor_email = %admin.email,
        target = %target.id, target_email = %target.email,
        "impersonation started"
    );
    redirect_with_session_cookie(&state, &new_session.id, "/")
}

/// POST /impersonate/stop — end an impersonation and restore the admin.
/// Not admin-gated: the live identity is the target. A no-op redirect for
/// an ordinary (non-impersonation) session.
pub async fn impersonate_stop(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (session, target) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let Some(admin_id) = session.impersonator_id.clone() else {
        // Not impersonating — nothing to stop.
        return see_other("/");
    };

    // Mint a fresh ordinary session for the admin and drop the
    // impersonation row so its cookie can't be replayed.
    let admin_session = match state.sessions.create(&admin_id).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "stop impersonation: minting admin session");
            return super::internal_error_html(&target.email, "could not restore your account");
        }
    };
    let _ = state.sessions.delete(&session.id).await;

    let admin_email = users::find_by_id(&state.db, &admin_id)
        .await
        .ok()
        .flatten()
        .map(|u| u.email)
        .unwrap_or_else(|| admin_id.clone());
    if let Err(err) = audit::record(
        &state.db,
        &admin_id,
        &admin_email,
        &target.id,
        &target.email,
        audit::Action::Stop,
    )
    .await
    {
        tracing::warn!(error = %err, "impersonate: audit stop");
    }
    tracing::info!(
        actor = %admin_id, target = %target.id, target_email = %target.email,
        "impersonation stopped"
    );
    redirect_with_session_cookie(&state, &admin_session.id, "/")
}

/// 303 to `location`, setting the signed session cookie for `session_id`.
/// Mirrors the cookie attributes the OIDC callback uses.
fn redirect_with_session_cookie(state: &RamaState, session_id: &str, location: &str) -> Response {
    let cookie = format!(
        "{name}={signed}; Path=/; HttpOnly; SameSite=Lax",
        name = COOKIE_NAME,
        signed = state.sessions.sign(session_id),
    );
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, location)
        .header(header::SET_COOKIE, cookie)
        .body("".into())
        .unwrap()
}

fn join_or(items: &[String], empty: &str) -> String {
    if items.is_empty() {
        empty.to_string()
    } else {
        items.join(", ")
    }
}

struct UserRow {
    id: String,
    email: String,
    name: String,
    oidc_roles: String,
    rbac_roles: String,
    created: String,
    /// The signed-in admin's own row — show a "you" marker, no Impersonate.
    is_self: bool,
}

fn render_body(rows: &[UserRow], events: &[audit::ImpersonationEvent], allow: bool) -> Html {
    let user_rows: Vec<Html> = rows.iter().map(|r| render_user_row(r, allow)).collect();
    html! {
        section(class: "max-w-5xl mx-auto p-4 sm:p-6 flex flex-col gap-4") {
            header(class: "flex flex-col gap-1") {
                h1(class: "text-2xl font-bold") { "Users" }
                if allow {
                    p(class: "text-base-content/70 text-sm") {
                        "Everyone who has signed in to this gateway, with their identity-provider \
                         groups and the gateway roles those map to. "
                        strong { "Impersonate" }
                        " starts a session that behaves exactly as that user — useful for \
                         reproducing what they see. Every impersonation is logged below."
                    }
                } else {
                    p(class: "text-base-content/70 text-sm") {
                        "Everyone who has signed in to this gateway, with their identity-provider \
                         groups and the gateway roles those map to. Impersonation is "
                        strong { "disabled" }
                        " on this gateway (`allow_impersonation = false`)."
                    }
                }
            }
            div(class: "overflow-x-auto card border border-base-300 bg-base-100") {
                table(class: "table table-sm") {
                    thead {
                        tr {
                            th { "User" }
                            th { "OIDC groups" }
                            th { "Gateway roles" }
                            th { "Joined" }
                            th(class: "text-right") { "Action" }
                        }
                    }
                    tbody {
                        for r in user_rows.iter() {
                            (r.clone())
                        }
                    }
                }
            }
            (render_audit(events))
        }
    }
    .to_html()
}

fn render_user_row(r: &UserRow, allow: bool) -> Html {
    let email = r.email.clone();
    let name = r.name.clone();
    let oidc = r.oidc_roles.clone();
    let rbac = r.rbac_roles.clone();
    let created = r.created.clone();
    let id = r.id.clone();
    let is_self = r.is_self;
    html! {
        tr {
            td {
                div(class: "font-medium") { (email) }
                if !name.is_empty() {
                    div(class: "text-xs text-base-content/60") { (name) }
                }
                div(class: "text-xs text-base-content/40 font-mono break-all") { (id.clone()) }
            }
            td(class: "text-sm") { (oidc) }
            td(class: "text-sm") { (rbac) }
            td(class: "text-sm whitespace-nowrap") { (created) }
            td(class: "text-right") {
                if is_self {
                    span(class: "badge badge-ghost") { "you" }
                } else if allow {
                    form(method: "post", action: "/admin/users/impersonate", class: "m-0 inline") {
                        input(type: "hidden", name: "user_id", value: (id));
                        button(type: "submit", class: "btn btn-outline btn-sm") {
                            (icons::users(14))
                            span { "Impersonate" }
                        }
                    }
                } else {
                    span(class: "text-base-content/40") { "—" }
                }
            }
        }
    }
    .to_html()
}

fn render_audit(events: &[audit::ImpersonationEvent]) -> Html {
    let rows: Vec<Html> = events.iter().map(render_audit_row).collect();
    html! {
        section(class: "card border border-base-300 bg-base-100 mt-2") {
            div(class: "card-body gap-2") {
                h2(class: "card-title text-base") { "Recent impersonation activity" }
                if rows.is_empty() {
                    p(class: "text-base-content/60 text-sm") {
                        "No impersonations recorded yet."
                    }
                } else {
                    div(class: "overflow-x-auto") {
                        table(class: "table table-sm") {
                            thead {
                                tr {
                                    th { "When" }
                                    th { "Action" }
                                    th { "Admin" }
                                    th { "Target" }
                                }
                            }
                            tbody {
                                for r in rows.iter() {
                                    (r.clone())
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

fn render_audit_row(e: &audit::ImpersonationEvent) -> Html {
    let when = e.created_at.strftime("%Y-%m-%d %H:%M").to_string();
    let action = e.action.clone();
    let actor = e.actor_email.clone();
    let target = e.target_email.clone();
    let badge = if action == "start" {
        "badge badge-warning"
    } else {
        "badge badge-ghost"
    };
    html! {
        tr {
            td(class: "text-sm whitespace-nowrap") { (when) }
            td { span(class: (badge)) { (action) } }
            td(class: "text-sm break-all") { (actor) }
            td(class: "text-sm break-all") { (target) }
        }
    }
    .to_html()
}
