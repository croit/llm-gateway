// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The per-user `/tools` page — every signed-in user can turn the
//! individual AI tools the assistant may call on or off for their own
//! account.
//!
//! This is a personal layer on top of RBAC: the list only ever shows
//! tools the user's roles already grant (via `rbac::allowed_tools`),
//! and a toggle can only *subtract* one — never add a tool a role
//! didn't grant. Default is enabled; we persist only explicit choices
//! (see `db::user_tool_prefs`). Tools are grouped + de-noised by
//! `server::tools::catalog`.

use std::collections::HashSet;
use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};
use serde::Deserialize;

use super::tool_toggles::{self, ToggleCtx};
use super::{
    NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, read_form,
    require_session_or_redirect, toast,
};
use session_core::chrome::{
    Flash, FlashKind, Theme, is_datastar_request, sse_patch, sse_response, sse_toast,
};

use crate::rama_server::state::RamaState;
use crate::server::db::{user_tool_prefs, users};
use crate::server::tools::catalog::ToolEntry;

/// Where a `/tools` toggle posts + how its row id is namespaced. The
/// prefix is `tool-row` so the DOM ids stay exactly as before the toggle
/// list was extracted into the shared component.
fn toggle_ctx() -> ToggleCtx {
    ToggleCtx {
        post_path: "/tools/toggle".to_string(),
        row_id_prefix: "tool-row".to_string(),
    }
}

/// Tool id whose presence in a user's grants unlocks the
/// browser-location sharing card on this page.
const LOCATION_TOOL_ID: &str = "get_user_location";

// ---------------------------------------------------------------------------
// GET /tools

/// GET /tools — render the caller's tool list with a toggle per entry.
pub async fn tools_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());

    let (session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let entries = entries_for_user(&state, &user.roles);
    let disabled = user_tool_prefs::disabled_for_user(&state.db, &user.id)
        .await
        .unwrap_or_default();
    // Only surface the "share precise location" card when the caller's
    // roles actually grant the location tool — otherwise sharing a
    // position would feed nothing. Its current state seeds the label.
    let geo_label = if entries.iter().any(|e| e.key == LOCATION_TOOL_ID) {
        let stored = users::find_location(&state.db, &user.id)
            .await
            .ok()
            .flatten();
        Some(match stored {
            Some(loc) => match loc.accuracy {
                Some(a) => format!("Shared — accuracy ±{a:.0} m."),
                None => "Shared.".to_string(),
            },
            None => "Not shared.".to_string(),
        })
    } else {
        None
    };
    let body = render_tools_body(&entries, &disabled, geo_label.as_deref());
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        NavItem::Tools,
        "Tools — LLM Gateway",
        &user.email,
        is_admin(&state, &user),
        session.impersonator_id.is_some(),
        body,
        "/tools",
        &chat,
    )
}

/// The tools the user's roles grant, grouped + de-noised for display.
/// Thin wrapper over the shared component's resolver.
fn entries_for_user(state: &RamaState, roles: &[String]) -> Vec<ToolEntry> {
    tool_toggles::entries_for_roles(state, roles)
}

// ---------------------------------------------------------------------------
// POST /tools/toggle

#[derive(Deserialize)]
struct ToggleForm {
    tool_key: String,
    /// Present (any value) when the toggle is checked; absent when the
    /// browser leaves an unchecked checkbox out of the form body.
    enabled: Option<String>,
}

/// POST /tools/toggle — persist one tool's on/off state for the caller
/// and patch its row back in place. The desired state rides in the
/// form (checkbox presence), so double-clicks converge rather than
/// race a read-modify-write.
pub async fn tools_toggle(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let form: ToggleForm = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };

    // Only let the user toggle a key their roles actually expose — the
    // page never offers others, so a request for one is bogus.
    let entries = entries_for_user(&state, &user.roles);
    let Some(entry) = entries.into_iter().find(|e| e.key == form.tool_key) else {
        return toast(FlashKind::Error, "unknown tool");
    };

    let enabled = form.enabled.is_some();
    if let Err(err) = user_tool_prefs::set(&state.db, &user.id, &entry.key, enabled).await {
        tracing::warn!(error = %err, tool_key = %entry.key, "tool pref save");
        return toast(FlashKind::Error, "could not save preference");
    }

    let ctx = toggle_ctx();
    let selector = format!("#{}", ctx.row_id(&entry.key));
    let row_html = tool_toggles::render_toggle_row(&entry, enabled, &ctx).to_string();
    let verb = if enabled { "enabled" } else { "disabled" };
    sse_response(&[
        sse_patch(Some(&selector), Some("outer"), &row_html),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: format!("{} {}.", entry.title, verb),
        }),
    ])
}

// ---------------------------------------------------------------------------
// Rendering

fn render_tools_body(
    entries: &[ToolEntry],
    disabled: &HashSet<String>,
    geo_label: Option<&str>,
) -> Html {
    // The grouped category cards come from the shared `tool_toggles`
    // component — the same list the per-token panel on /tokens renders.
    let sections = tool_toggles::render_toggle_sections(entries, disabled, &toggle_ctx());
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
        h1(class: "text-2xl font-bold mb-2") { "Tools" }
        p(class: "text-base-content/60 text-sm mb-6") {
            "Turn the tools the assistant may use on or off. Changes apply "
            "to your account only and take effect on your next message."
        }
        if geo_label.is_some() {
            (render_location_card(geo_label.unwrap_or("")))
        }
        if entries.is_empty() {
            div(class: "card border border-base-300") {
                div(class: "card-body") {
                    p(class: "text-base-content/60 text-sm m-0") {
                        "Your roles don't grant any tools."
                    }
                }
            }
        }
        (sections)
        }
    }
    .to_html()
}

/// The browser-location sharing card shown above the tool list when the
/// caller has the `get_user_location` tool. The buttons call into
/// `window.geo` (see `ui/ts/geo.ts`), which requests the position via
/// `navigator.geolocation` (a user gesture is required — hence a button,
/// not an automatic post) and `POST`s / `DELETE`s `/api/v0/me/location`.
/// `status` is the server-rendered initial label; `geo.ts` updates the
/// `[data-geo-status]` span live after each action.
fn render_location_card(status: &str) -> Html {
    let status = status.to_string();
    html! {
        section(class: "card border border-base-300 mb-6") {
            div(class: "card-body") {
                h2(class: "card-title text-base") { "Location" }
                p(class: "text-base-content/60 text-sm m-0") {
                    "Share your device's precise location so the assistant can answer questions "
                    "like \"what's the weather here?\". It's used only for your tool calls and "
                    "you can stop sharing anytime. Without it, the assistant falls back to an "
                    "approximate location derived from your IP address."
                }
                div(class: "flex items-center gap-3 mt-3 flex-wrap") {
                    button(
                        type: "button",
                        class: "btn btn-sm btn-primary",
                        "data-on:click": "window.geo.share(el)"
                    ) { "Share precise location" }
                    button(
                        type: "button",
                        class: "btn btn-sm btn-ghost",
                        "data-on:click": "window.geo.forget(el)"
                    ) { "Stop sharing" }
                    span(class: "text-xs text-base-content/60", "data-geo-status": "") {
                        (status)
                    }
                }
            }
        }
    }
    .to_html()
}
