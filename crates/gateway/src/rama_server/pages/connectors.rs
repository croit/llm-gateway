// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Admin page `/admin/connectors` — manage the MCP connector catalog the
//! per-user store (`/integrations`) draws from.
//!
//! The catalog is seeded at boot with a built-in default set (all disabled);
//! here an admin enables them, edits endpoints/scopes, supplies a
//! deployment-specific OAuth client where a connector can't use dynamic client
//! registration (e.g. the official Google servers), adds custom connectors, or
//! restores the defaults. Client secrets are encrypted at rest
//! (`server::crypto`); the DB never sees plaintext.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::{Path, State};
use rama::http::{Request, Response, StatusCode, header};
use serde::Deserialize;

use super::{
    NavItem, fetch_sidebar_chat, internal_error_html, nav_or_html_page, parse_csv, read_form,
    require_admin_or_403,
};
use crate::rama_server::state::RamaState;
use crate::server::db::mcp_audit::{self, McpToolEvent};
use crate::server::db::mcp_catalog::{self, AuthKind, Connector, ConnectorInput, Scope};
use session_core::chrome::{NavSections, Theme, is_datastar_request};
use session_core::i18n::{self, Lang, t, t_args};

// ---------------------------------------------------------------------------
// GET /admin/connectors

pub async fn connectors_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let connectors = mcp_catalog::list_all(&state.db).await.unwrap_or_default();
    let redirect_uri = format!(
        "{}/integrations/callback",
        state.config.gateway.public_url.trim_end_matches('/')
    );
    let body = render_body(lang, &connectors, &redirect_uri);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    let title = t(lang, "connectors-page-title");
    {
        let pctx = super::PageCtx {
            theme,
            lang,
            nav,
            datastar,
            user_email: user.email.clone(),
            is_admin: true,
            skills_enabled: state.user_skills_enabled(),
            impersonating: session.impersonator_id.is_some(),
        };
        nav_or_html_page(
            &pctx,
            NavItem::Connectors,
            &title,
            body,
            "/admin/connectors",
            &chat,
        )
    }
}

// ---------------------------------------------------------------------------
// POST /admin/connectors  (create or update)

#[derive(Deserialize)]
struct SaveForm {
    key: String,
    name: String,
    description: Option<String>,
    icon: Option<String>,
    category: Option<String>,
    url: String,
    auth: Option<String>,
    scope: Option<String>,
    audit: Option<String>,
    use_dcr: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    authorize_url: Option<String>,
    token_url: Option<String>,
    registration_url: Option<String>,
    scopes: Option<String>,
    /// Comma-separated gateway groups allowed to see + connect this connector
    /// (empty = everyone). Matched like a pool's `allowed_groups`.
    allowed_groups: Option<String>,
    /// Optional: the OAuth client JSON downloaded from Google Cloud Console
    /// (`{"web":{…}}` / `{"installed":{…}}`). When present, its client_id /
    /// client_secret / auth_uri / token_uri pre-fill the fields below.
    client_json: Option<String>,
}

fn clean(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Fields lifted out of a Google (or generic OAuth) client-credentials JSON.
#[derive(Default)]
struct ParsedClientJson {
    client_id: Option<String>,
    client_secret: Option<String>,
    authorize_url: Option<String>,
    token_url: Option<String>,
}

/// Parse a downloaded OAuth client JSON. Accepts the Google shapes
/// `{"web":{…}}` and `{"installed":{…}}`, and a bare `{…}` object. Unknown /
/// malformed input yields all-`None` (the individual fields then apply).
fn parse_client_json(raw: &str) -> ParsedClientJson {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return ParsedClientJson::default();
    };
    let obj = v.get("web").or_else(|| v.get("installed")).unwrap_or(&v);
    let s = |k: &str| obj.get(k).and_then(|x| x.as_str()).map(str::to_owned);
    ParsedClientJson {
        client_id: s("client_id"),
        client_secret: s("client_secret"),
        authorize_url: s("auth_uri"),
        token_url: s("token_uri"),
    }
}

pub async fn connectors_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (_session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let form: SaveForm = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };

    let key = form.key.trim().to_string();
    if key.is_empty() || form.name.trim().is_empty() || form.url.trim().is_empty() {
        return internal_error_html(&user.email, &t(lang, "connectors-error-missing-fields"));
    }
    // A pasted OAuth client JSON wins over the individual fields (which stay as
    // a manual fallback). Reject obviously-malformed JSON so the admin gets a
    // clear signal rather than a silently-ignored paste.
    let parsed = match clean(form.client_json) {
        Some(json) => {
            let p = parse_client_json(&json);
            if p.client_id.is_none() {
                return internal_error_html(
                    &user.email,
                    &t(lang, "connectors-error-bad-client-json"),
                );
            }
            p
        }
        None => ParsedClientJson::default(),
    };
    let client_id = parsed.client_id.or_else(|| clean(form.client_id));
    let authorize_url = parsed.authorize_url.or_else(|| clean(form.authorize_url));
    let token_url = parsed.token_url.or_else(|| clean(form.token_url));
    // Encrypt the client secret only when one was supplied (JSON or field);
    // otherwise leave the stored value untouched (edit) or unset (create).
    let secret_plain = parsed.client_secret.or_else(|| clean(form.client_secret));
    let sealed = match secret_plain {
        Some(secret) => match state.crypto.seal_str(&secret) {
            Ok(s) => Some(s),
            Err(err) => {
                return internal_error_html(
                    &user.email,
                    &t_args(
                        lang,
                        "connectors-error-sealing-secret",
                        &i18n::args([("error", err.to_string().into())]),
                    ),
                );
            }
        },
        None => None,
    };
    let scopes = form
        .scopes
        .unwrap_or_default()
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    let auth = AuthKind::parse(form.auth.as_deref().unwrap_or("oauth2"));
    let scope = Scope::parse(form.scope.as_deref().unwrap_or("per_user"));
    // A global connector is one shared identity for the whole gateway, so it
    // can't use per-user OAuth. Only `none` (token baked into the server, e.g.
    // Discord) or `static_bearer` (one shared token on the row) make sense.
    if scope == Scope::Global && auth == AuthKind::OAuth2 {
        return internal_error_html(
            &user.email,
            "A global connector is a single shared identity for the whole gateway, so it \
             can't use per-user OAuth. Choose \"No auth\" (token baked into the server, \
             e.g. Discord) or \"Bearer token\" (one shared token) instead.",
        );
    }

    let input = ConnectorInput {
        key: key.clone(),
        name: form.name.trim().to_string(),
        description: clean(form.description),
        icon: clean(form.icon),
        category: clean(form.category),
        url: form.url.trim().to_string(),
        auth,
        scope,
        audit: form.audit.is_some(),
        use_dcr: form.use_dcr.is_some(),
        client_id,
        client_secret_ct: sealed.as_ref().map(|s| s.ciphertext.clone()),
        client_secret_nonce: sealed.as_ref().map(|s| s.nonce.clone()),
        authorize_url,
        token_url,
        registration_url: clean(form.registration_url),
        scopes,
        allowed_groups: parse_csv(&form.allowed_groups.unwrap_or_default()),
    };

    // Upsert: update if it exists, else create.
    let exists = matches!(mcp_catalog::get(&state.db, &key).await, Ok(Some(_)));
    let res = if exists {
        mcp_catalog::update(&state.db, &key, input)
            .await
            .map(|_| ())
    } else {
        mcp_catalog::create(&state.db, input).await
    };
    if let Err(err) = res {
        return internal_error_html(
            &user.email,
            &t_args(
                lang,
                "connectors-error-saving",
                &i18n::args([("error", err.to_string().into())]),
            ),
        );
    }
    redirect("/admin/connectors")
}

// ---------------------------------------------------------------------------
// POST /admin/connectors/{key}/toggle  |  /delete

#[derive(Deserialize)]
struct ToggleForm {
    enabled: Option<String>,
}

pub async fn connectors_toggle(
    State(state): State<Arc<RamaState>>,
    Path(key): Path<String>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (_session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let form: ToggleForm = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let enabled = form.enabled.is_some();
    // Guard: don't let an admin enable an OAuth connector that still needs a
    // client id (no DCR, no client_id) — it would only fail at connect time.
    if enabled
        && let Ok(Some(c)) = mcp_catalog::get(&state.db, &key).await
        && c.needs_setup()
    {
        return internal_error_html(&user.email, &t(lang, "connectors-error-needs-client-id"));
    }
    if let Err(err) = mcp_catalog::set_enabled(&state.db, &key, enabled).await {
        return internal_error_html(
            &user.email,
            &t_args(
                lang,
                "connectors-error-toggling",
                &i18n::args([("error", err.to_string().into())]),
            ),
        );
    }
    redirect("/admin/connectors")
}

pub async fn connectors_delete(
    State(state): State<Arc<RamaState>>,
    Path(key): Path<String>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (_session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Err(err) = mcp_catalog::delete(&state.db, &key).await {
        return internal_error_html(
            &user.email,
            &t_args(
                lang,
                "connectors-error-deleting",
                &i18n::args([("error", err.to_string().into())]),
            ),
        );
    }
    // Drop every user's connection (incl. encrypted tokens) + prefs for it, so
    // deleting a connector doesn't leave orphaned secrets behind.
    if let Err(err) = crate::server::db::user_mcp::delete_all_for_connector(&state.db, &key).await {
        tracing::warn!(error = %err, connector = %key, "cleaning up user connections after connector delete");
    }
    redirect("/admin/connectors")
}

// ---------------------------------------------------------------------------
// POST /admin/connectors/restore-defaults

pub async fn connectors_restore(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (_session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Err(err) = mcp_catalog::seed_defaults(&state.db).await {
        return internal_error_html(
            &user.email,
            &t_args(
                lang,
                "connectors-error-restoring",
                &i18n::args([("error", err.to_string().into())]),
            ),
        );
    }
    redirect("/admin/connectors")
}

// ---------------------------------------------------------------------------
// GET /admin/connectors/{key}/audit  — dedicated tool-call log for one connector

pub async fn connectors_audit(
    State(state): State<Arc<RamaState>>,
    Path(key): Path<String>,
    req: Request,
) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let connector = mcp_catalog::get(&state.db, &key).await.ok().flatten();
    let name = connector
        .as_ref()
        .map(|c| c.name.clone())
        .unwrap_or_else(|| key.clone());
    let events = mcp_audit::recent_for_connector(&state.db, &key, 200)
        .await
        .unwrap_or_default();
    let body = render_audit_page(&name, &key, &events);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    let title = format!("{name} — audit log");
    {
        let pctx = super::PageCtx {
            theme,
            lang,
            nav,
            datastar,
            user_email: user.email.clone(),
            is_admin: true,
            skills_enabled: state.user_skills_enabled(),
            impersonating: session.impersonator_id.is_some(),
        };
        nav_or_html_page(
            &pctx,
            NavItem::Connectors,
            &title,
            body,
            "/admin/connectors",
            &chat,
        )
    }
}

// ---------------------------------------------------------------------------
// Rendering

fn redirect(location: &str) -> Response {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, location)
        .body("".into())
        .unwrap()
}

fn render_body(lang: Lang, connectors: &[Connector], redirect_uri: &str) -> Html {
    let rows: Vec<Html> = connectors
        .iter()
        .map(|c| render_connector_row(lang, c, redirect_uri))
        .collect();
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
            div(class: "flex items-center justify-between gap-3 mb-2 flex-wrap") {
                h1(class: "text-2xl font-bold m-0") { (t(lang, "connectors-heading")) }
                form(method: "post", action: "/admin/connectors/restore-defaults", class: "m-0") {
                    button(type: "submit", class: "btn btn-sm btn-ghost") { (t(lang, "connectors-restore-defaults-button")) }
                }
            }
            p(class: "text-base-content/60 text-sm mb-6") {
                (t(lang, "connectors-catalog-intro"))
            }
            (render_add_form(lang, redirect_uri))
            if connectors.is_empty() {
                div(class: "card border border-base-300") {
                    div(class: "card-body") {
                        p(class: "text-base-content/60 text-sm m-0") { (t(lang, "connectors-empty-state")) }
                    }
                }
            }
            div(class: "flex flex-col gap-3 mt-4") {
                for row in rows.iter() {
                    (row.clone())
                }
            }
        }
    }
    .to_html()
}

/// The dedicated per-connector tool-call log page (opened from the connector's
/// "Audit log" button). Newest first; last 200.
fn render_audit_page(name: &str, key: &str, events: &[McpToolEvent]) -> Html {
    let rows: Vec<Html> = events
        .iter()
        .map(|e| {
            let when = e.created_at.to_string();
            let who = if e.user_email.is_empty() {
                e.user_id.clone()
            } else {
                e.user_email.clone()
            };
            let ok = e.outcome == "ok";
            let detail = e
                .error
                .clone()
                .or_else(|| e.arguments.clone())
                .unwrap_or_default();
            html! {
                tr {
                    td(class: "text-xs whitespace-nowrap text-base-content/60") { (when) }
                    td(class: "text-xs") { (who) }
                    td(class: "text-xs") { code { (e.tool_id.clone()) } }
                    td(class: "text-xs") {
                        if ok {
                            span(class: "badge badge-success badge-xs") { "ok" }
                        } else {
                            span(class: "badge badge-error badge-xs") { "error" }
                        }
                    }
                    td(class: "text-xs text-base-content/50 max-w-md truncate") { (detail) }
                }
            }
            .to_html()
        })
        .collect();
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
            a(href: "/admin/connectors", class: "text-sm text-base-content/60 hover:underline") { "← Connectors" }
            div(class: "flex items-center gap-2 flex-wrap mt-2 mb-1") {
                h1(class: "text-2xl font-bold m-0") { (name.to_string()) }
                span(class: "badge badge-warning badge-sm") { "Audited" }
            }
            p(class: "text-base-content/60 text-sm mb-6") {
                "Tool-call audit for " code { (key.to_string()) } ". Newest first; last 200."
            }
            if events.is_empty() {
                p(class: "text-base-content/50 text-sm m-0") {
                    "No tool calls recorded for this connector yet."
                }
            } else {
                div(class: "overflow-x-auto") {
                    table(class: "table table-sm") {
                        thead {
                            tr {
                                th { "When" } th { "User" } th { "Tool" }
                                th { "Outcome" } th { "Detail" }
                            }
                        }
                        tbody {
                            for row in rows.iter() { (row.clone()) }
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

fn render_connector_row(lang: Lang, c: &Connector, redirect_uri: &str) -> Html {
    let enabled = c.enabled;
    let key = c.key.clone();
    let name = c.name.clone();
    let icon_text = c.icon.clone().unwrap_or_default();
    let logo = session_core::icons::connector_logo(&c.key, 22)
        .unwrap_or_else(|| html! { span(class: "text-xl leading-none") { (icon_text) } }.to_html());
    let url = c.url.clone();
    let toggle_action = format!("/admin/connectors/{key}/toggle");
    let delete_action = format!("/admin/connectors/{key}/delete");
    let audit_action = format!("/admin/connectors/{key}/audit");
    let has_secret = c.client_secret_ct.is_some();
    let delete_confirm = t(lang, "connectors-delete-confirm");
    html! {
        section(class: "card border border-base-300") {
            div(class: "card-body gap-2") {
                div(class: "flex items-center gap-3 flex-wrap") {
                    span(class: "shrink-0") { (logo.clone()) }
                    div(class: "min-w-0 flex-1") {
                        div(class: "flex items-center gap-2 flex-wrap") {
                            h2(class: "card-title text-base m-0") { (name) }
                            code(class: "text-xs text-base-content/50") { (key.clone()) }
                            if enabled {
                                span(class: "badge badge-success badge-sm") { (t(lang, "connectors-badge-enabled")) }
                            } else {
                                span(class: "badge badge-ghost badge-sm") { (t(lang, "connectors-badge-disabled")) }
                            }
                            if c.is_global() {
                                span(class: "badge badge-info badge-sm") { "Global" }
                            }
                            if c.audit {
                                span(class: "badge badge-warning badge-sm") { "Audited" }
                            }
                            if c.seeded {
                                span(class: "badge badge-outline badge-sm") { (t(lang, "connectors-badge-default")) }
                            }
                            if c.use_dcr {
                                span(class: "badge badge-outline badge-sm") { (t(lang, "connectors-badge-dcr")) }
                            }
                            if c.needs_setup() {
                                span(class: "badge badge-warning badge-sm") { (t(lang, "connectors-badge-needs-client-id")) }
                            }
                        }
                        p(class: "text-xs text-base-content/50 m-0 mt-0.5 break-all") { (url) }
                    }
                    div(class: "flex items-center gap-2 shrink-0") {
                        form(method: "post", action: (toggle_action), class: "m-0") {
                            if enabled {
                                button(type: "submit", class: "btn btn-xs btn-ghost") { (t(lang, "connectors-disable-button")) }
                            } else if c.needs_setup() {
                                // Required OAuth client id missing → can't be
                                // enabled yet. Grey it out instead of erroring
                                // on click; the Edit form + help box explain it.
                                button(type: "button", disabled: "disabled",
                                       class: "btn btn-xs btn-primary btn-disabled",
                                       title: (t(lang, "connectors-enable-disabled-title"))) {
                                    (t(lang, "connectors-enable-button"))
                                }
                            } else {
                                button(type: "submit", name: "enabled", value: "1", class: "btn btn-xs btn-primary") { (t(lang, "connectors-enable-button")) }
                            }
                        }
                        // Only when this connector is audited — opens its
                        // dedicated tool-call log page.
                        if c.audit {
                            a(href: (audit_action), class: "btn btn-xs btn-ghost") { "Audit log" }
                        }
                        form(method: "post", action: (delete_action), class: "m-0",
                             "data-confirm": (delete_confirm)) {
                            button(type: "submit", class: "btn btn-xs btn-ghost text-error") { (t(lang, "connectors-delete-button")) }
                        }
                    }
                }
                details {
                    summary(class: "cursor-pointer text-sm text-base-content/70") { (t(lang, "connectors-edit-summary")) }
                    (render_edit_form(lang, c, has_secret, redirect_uri))
                }
            }
        }
    }
    .to_html()
}

fn render_add_form(lang: Lang, redirect_uri: &str) -> Html {
    html! {
        details(class: "card border border-base-300 mb-2") {
            summary(class: "cursor-pointer card-body py-3 font-medium text-sm") { (t(lang, "connectors-add-summary")) }
            div(class: "card-body pt-0") {
                (render_form_fields(lang, None, false, redirect_uri))
            }
        }
    }
    .to_html()
}

fn render_edit_form(lang: Lang, c: &Connector, has_secret: bool, redirect_uri: &str) -> Html {
    html! {
        div(class: "mt-2") {
            (render_form_fields(lang, Some(c), has_secret, redirect_uri))
        }
    }
    .to_html()
}

/// Provider-specific help for obtaining an OAuth client (shown for connectors
/// that need a manually-created client). Always shows the redirect URI to
/// register; adds a direct link for Google / GitHub.
fn render_oauth_help(lang: Lang, existing: Option<&Connector>, redirect_uri: &str) -> Html {
    // Token-based connectors need no OAuth client — users paste their own token.
    if existing
        .map(|c| c.auth == AuthKind::StaticBearer)
        .unwrap_or(false)
    {
        return html! {
            div(class: "rounded-md border border-info/30 bg-info/5 p-3 text-xs leading-relaxed") {
                p(class: "m-0") {
                    (t(lang, "connectors-oauth-help-token-1")) " "
                    code { "Authorization: Bearer <token>" }
                    (t(lang, "connectors-oauth-help-token-2"))
                }
            }
        }
        .to_html();
    }
    // Open connectors need no credentials at all — just the server URL.
    if existing.map(|c| c.auth == AuthKind::None).unwrap_or(false) {
        let is_global = existing.map(|c| c.is_global()).unwrap_or(false);
        return html! {
            div(class: "rounded-md border border-info/30 bg-info/5 p-3 text-xs leading-relaxed") {
                if is_global {
                    p(class: "m-0") {
                        "Global connector: set the MCP server URL above (the gateway reaches it "
                        "with no auth). The credential — e.g. a Discord bot token — is baked into "
                        "the MCP server itself, so keep its endpoint private/loopback-only, never "
                        "publicly exposed. Its tools are available to everyone the role below "
                        "allows, with no per-user sign-in; each user can still toggle them "
                        "always/ask/off on their Tools page."
                    }
                } else {
                    p(class: "m-0") {
                        "Public connector: set the MCP server URL above. No OAuth client and no "
                        "per-user token — every user still connects individually so they can opt "
                        "its tools in or out."
                    }
                }
            }
        }
        .to_html();
    }
    let key = existing.map(|c| c.key.clone()).unwrap_or_default();
    let category = existing
        .and_then(|c| c.category.clone())
        .unwrap_or_default();
    // DCR connectors have no OAuth client to create here — the MCP server is its
    // own authorization server and registers the gateway dynamically.
    if existing.map(|c| c.use_dcr).unwrap_or(false) {
        let is_google_ws = key == "google_workspace";
        let redirect_uri = redirect_uri.to_string();
        return html! {
            div(class: "rounded-md border border-info/30 bg-info/5 p-3 text-xs leading-relaxed") {
                p(class: "m-0 font-medium") { (t(lang, "connectors-oauth-help-dcr-heading")) }
                p(class: "m-0 mt-1") {
                    (t(lang, "connectors-oauth-help-dcr-body"))
                }
                if is_google_ws {
                    p(class: "m-0 mt-2") {
                        (t(lang, "connectors-oauth-help-gws-1")) " "
                        strong { (t(lang, "connectors-oauth-help-gws-self-hosted")) } " "
                        (t(lang, "connectors-oauth-help-gws-2")) " "
                        a(class: "link", target: "_blank", rel: "noopener noreferrer",
                          href: "https://github.com/taylorwilsdon/google_workspace_mcp") {
                            "taylorwilsdon/google_workspace_mcp"
                        }
                        " "
                        (t(lang, "connectors-oauth-help-gws-3")) " "
                        code { "/mcp/" }
                        (t(lang, "connectors-oauth-help-gws-4")) " "
                        strong { (t(lang, "connectors-oauth-help-gws-ga-apis")) } " "
                        (t(lang, "connectors-oauth-help-gws-5")) " "
                        code { "WORKSPACE_MCP_ALLOWED_CLIENT_REDIRECT_URIS" }
                        ":"
                    }
                    code(class: "block mt-1 p-1.5 rounded bg-base-300/60 break-all select-all") {
                        (redirect_uri)
                    }
                    p(class: "m-0 mt-2 text-base-content/60") {
                        (t(lang, "connectors-oauth-help-gws-footer"))
                    }
                }
            }
        }
        .to_html();
    }
    let is_google = category == "Google" || key.starts_with("google") || key == "gmail";
    let is_github = key == "github";
    let is_slack = key == "slack";
    let redirect_uri = redirect_uri.to_string();
    html! {
        div(class: "rounded-md border border-info/30 bg-info/5 p-3 text-xs leading-relaxed") {
            p(class: "m-0 font-medium") { (t(lang, "connectors-oauth-help-generic-heading")) }
            p(class: "m-0 mt-1") {
                (t(lang, "connectors-oauth-help-generic-intro"))
            }
            code(class: "block mt-1 mb-2 p-1.5 rounded bg-base-300/60 break-all select-all") {
                (redirect_uri)
            }
            if is_google {
                p(class: "m-0") {
                    (t(lang, "connectors-oauth-help-google-1")) " "
                    a(class: "link", target: "_blank", rel: "noopener noreferrer",
                      href: "https://console.cloud.google.com/apis/credentials") {
                        (t(lang, "connectors-oauth-help-google-link"))
                    }
                    " "
                    (t(lang, "connectors-oauth-help-google-2"))
                }
            } else if is_github {
                p(class: "m-0") {
                    (t(lang, "connectors-oauth-help-github-1")) " "
                    a(class: "link", target: "_blank", rel: "noopener noreferrer",
                      href: "https://github.com/settings/developers") {
                        (t(lang, "connectors-oauth-help-github-link"))
                    }
                    " "
                    (t(lang, "connectors-oauth-help-github-2"))
                }
            } else if is_slack {
                p(class: "m-0") {
                    "Slack: create an app at "
                    a(class: "link", target: "_blank", rel: "noopener noreferrer",
                      href: "https://api.slack.com/apps") {
                        "api.slack.com/apps"
                    }
                    ", add the redirect URI above under OAuth & Permissions, request the "
                    "scopes configured below, and copy the Client ID + Client Secret from "
                    "Basic Information. Slack requires a statically registered, "
                    "directory-published or workspace-internal app — dynamic client "
                    "registration isn't supported."
                }
            } else {
                p(class: "m-0") {
                    (t(lang, "connectors-oauth-help-fallback"))
                }
            }
            p(class: "m-0 mt-2 text-base-content/60") {
                (t(lang, "connectors-oauth-why-1")) " "
                strong { (t(lang, "connectors-term-this-gateway")) } " "
                (t(lang, "connectors-oauth-why-2")) " "
                strong { (t(lang, "connectors-oauth-why-no-app")) } " "
                (t(lang, "connectors-oauth-why-3"))
            }
        }
    }
    .to_html()
}

/// The shared create/edit form. `existing` pre-fills the fields (and pins the
/// key read-only); `None` renders a blank create form.
fn render_form_fields(
    lang: Lang,
    existing: Option<&Connector>,
    has_secret: bool,
    redirect_uri: &str,
) -> Html {
    let v = |f: fn(&Connector) -> String| existing.map(f).unwrap_or_default();
    let key = existing.map(|c| c.key.clone()).unwrap_or_default();
    let name = v(|c| c.name.clone());
    let description = existing
        .and_then(|c| c.description.clone())
        .unwrap_or_default();
    let icon = existing.and_then(|c| c.icon.clone()).unwrap_or_default();
    let category = existing
        .and_then(|c| c.category.clone())
        .unwrap_or_default();
    let url = v(|c| c.url.clone());
    let client_id = existing
        .and_then(|c| c.client_id.clone())
        .unwrap_or_default();
    let authorize_url = existing
        .and_then(|c| c.authorize_url.clone())
        .unwrap_or_default();
    let token_url = existing
        .and_then(|c| c.token_url.clone())
        .unwrap_or_default();
    let registration_url = existing
        .and_then(|c| c.registration_url.clone())
        .unwrap_or_default();
    let scopes = existing.map(|c| c.scopes.join(" ")).unwrap_or_default();
    let allowed_groups = existing
        .map(|c| c.allowed_groups.join(", "))
        .unwrap_or_default();
    let use_dcr = existing.map(|c| c.use_dcr).unwrap_or(true);
    let auth_kind = existing.map(|c| c.auth).unwrap_or(AuthKind::OAuth2);
    let auth_static = auth_kind == AuthKind::StaticBearer;
    let auth_none = auth_kind == AuthKind::None;
    let scope_kind = existing.map(|c| c.scope).unwrap_or(Scope::PerUser);
    let is_global = scope_kind == Scope::Global;
    let audit = existing.map(|c| c.audit).unwrap_or(false);
    let is_edit = existing.is_some();
    let secret_placeholder = if has_secret {
        t(lang, "connectors-secret-placeholder-existing")
    } else {
        t(lang, "connectors-secret-placeholder-new")
    };

    let text_field = |label: &str, fname: &str, val: &str, ph: &str| -> Html {
        let label = label.to_string();
        let fname = fname.to_string();
        let val = val.to_string();
        let ph = ph.to_string();
        html! {
            label(class: "flex flex-col gap-1 w-full") {
                span(class: "label-text text-xs") { (label) }
                input(
                    type: "text", name: (fname), value: (val), placeholder: (ph),
                    class: "input input-bordered input-sm w-full"
                );
            }
        }
        .to_html()
    };

    let key_label = t(lang, "connectors-field-key-label");
    let key_placeholder = t(lang, "connectors-field-key-placeholder");
    let name_label = t(lang, "connectors-field-name-label");
    let name_placeholder = t(lang, "connectors-field-name-placeholder");
    let icon_label = t(lang, "connectors-field-icon-label");
    let category_label = t(lang, "connectors-field-category-label");
    let category_placeholder = t(lang, "connectors-field-category-placeholder");
    let description_label = t(lang, "connectors-field-description-label");
    let description_placeholder = t(lang, "connectors-field-description-placeholder");
    let url_label = t(lang, "connectors-field-url-label");
    let scopes_label = t(lang, "connectors-field-scopes-label");
    let optional_override = t(lang, "connectors-placeholder-optional-override");
    let authorize_url_label = t(lang, "connectors-field-authorize-url-label");
    let token_url_label = t(lang, "connectors-field-token-url-label");
    let registration_url_label = t(lang, "connectors-field-registration-url-label");
    let allowed_groups_label = t(lang, "connectors-field-allowed-groups-label");
    let allowed_groups_placeholder = t(lang, "connectors-placeholder-optional");
    let client_id_placeholder = t(lang, "connectors-field-client-id-placeholder");

    html! {
        form(method: "post", action: "/admin/connectors", class: "flex flex-col gap-2") {
            div(class: "grid grid-cols-1 sm:grid-cols-2 gap-2") {
                if is_edit {
                    label(class: "flex flex-col gap-1 w-full") {
                        span(class: "label-text text-xs") { (t(lang, "connectors-field-key-readonly-label")) }
                        input(type: "text", name: "key", value: (key.clone()), readonly: "readonly",
                              class: "input input-bordered input-sm w-full opacity-60");
                    }
                } else {
                    (text_field(&key_label, "key", "", &key_placeholder))
                }
                (text_field(&name_label, "name", &name, &name_placeholder))
                (text_field(&icon_label, "icon", &icon, "📧"))
                (text_field(&category_label, "category", &category, &category_placeholder))
            }
            (text_field(&description_label, "description", &description, &description_placeholder))
            (text_field(&url_label, "url", &url, "https://…/mcp"))
            label(class: "flex flex-col gap-1 w-full") {
                span(class: "label-text text-xs") { "Scope" }
                select(name: "scope", class: "select select-bordered select-sm w-full") {
                    if is_global {
                        option(value: "per_user") { "Per-user (each user connects their own account)" }
                        option(value: "global", selected: "selected") { "Global (one shared identity for everyone)" }
                    } else {
                        option(value: "per_user", selected: "selected") { "Per-user (each user connects their own account)" }
                        option(value: "global") { "Global (one shared identity for everyone)" }
                    }
                }
                span(class: "label-text-alt text-base-content/50") {
                    "Global connectors are shared by everyone (RBAC-gated) with no sign-in — a single bot/token for the whole gateway (e.g. Discord). They must use \"No auth\" or \"Bearer token\", not OAuth."
                }
            }
            label(class: "flex flex-col gap-1 w-full") {
                span(class: "label-text text-xs") { (t(lang, "connectors-field-auth-label")) }
                select(name: "auth", class: "select select-bordered select-sm w-full") {
                    if auth_static {
                        option(value: "oauth2") { (t(lang, "connectors-auth-option-oauth")) }
                        option(value: "static_bearer", selected: "selected") { (t(lang, "connectors-auth-option-token")) }
                        option(value: "none") { (t(lang, "connectors-auth-option-none")) }
                    } else if auth_none {
                        option(value: "oauth2") { (t(lang, "connectors-auth-option-oauth")) }
                        option(value: "static_bearer") { (t(lang, "connectors-auth-option-token")) }
                        option(value: "none", selected: "selected") { (t(lang, "connectors-auth-option-none")) }
                    } else {
                        option(value: "oauth2", selected: "selected") { (t(lang, "connectors-auth-option-oauth")) }
                        option(value: "static_bearer") { (t(lang, "connectors-auth-option-token")) }
                        option(value: "none") { (t(lang, "connectors-auth-option-none")) }
                    }
                }
            }
            (render_oauth_help(lang, existing, redirect_uri))
            // A global static_bearer connector sends one shared token for
            // everyone, so the admin enters it here (stored encrypted on the
            // connector row). Per-user static_bearer connectors instead have
            // each user paste their own token on /integrations, so the field is
            // only shown for the global case.
            if is_global && auth_static {
                label(class: "flex flex-col gap-1 w-full") {
                    span(class: "label-text text-xs") { "Bearer token (shared)" }
                    input(type: "password", name: "client_secret", placeholder: (secret_placeholder),
                          class: "input input-bordered input-sm w-full");
                    span(class: "label-text-alt text-base-content/50") {
                        "Sent as " code { "Authorization: Bearer <token>" } " to the MCP server for every user. Stored encrypted."
                    }
                }
            }
            // OAuth-client fields only make sense for OAuth connectors. A
            // user-supplied-token (static_bearer) or public (none) connector
            // has no app-level client, so hide the whole block (incl. the
            // Google client-JSON paste).
            if !auth_static && !auth_none {
                label(class: "flex flex-col gap-1 w-full") {
                    span(class: "label-text text-xs") {
                        (t(lang, "connectors-field-client-json-label"))
                    }
                    textarea(name: "client_json", rows: "3", autocomplete: "off",
                             placeholder: "{\"web\":{\"client_id\":\"…\",\"client_secret\":\"…\",\"auth_uri\":\"…\",\"token_uri\":\"…\"}}",
                             class: "textarea textarea-bordered textarea-sm w-full font-mono text-xs") {}
                    span(class: "label-text-alt text-base-content/50") {
                        (t(lang, "connectors-field-client-json-help"))
                    }
                }
                div(class: "grid grid-cols-1 sm:grid-cols-2 gap-2") {
                    label(class: "flex flex-col gap-1 w-full") {
                        span(class: "label-text text-xs") { (t(lang, "connectors-field-client-id-label")) }
                        input(type: "text", name: "client_id", value: (client_id),
                              placeholder: (client_id_placeholder),
                              class: "input input-bordered input-sm w-full");
                        span(class: "label-text-alt text-base-content/50") {
                            (t(lang, "connectors-field-client-id-help-1")) " "
                            strong { (t(lang, "connectors-term-this-gateway")) } " "
                            (t(lang, "connectors-field-client-id-help-2"))
                        }
                    }
                    label(class: "flex flex-col gap-1 w-full") {
                        span(class: "label-text text-xs") { (t(lang, "connectors-field-client-secret-label")) }
                        input(type: "password", name: "client_secret", placeholder: (secret_placeholder),
                              class: "input input-bordered input-sm w-full");
                        span(class: "label-text-alt text-base-content/50") {
                            (t(lang, "connectors-field-client-secret-help"))
                        }
                    }
                }
                label(class: "label cursor-pointer justify-start gap-2 py-0") {
                    if use_dcr {
                        input(type: "checkbox", name: "use_dcr", value: "1", checked: "checked", class: "checkbox checkbox-sm");
                    } else {
                        input(type: "checkbox", name: "use_dcr", value: "1", class: "checkbox checkbox-sm");
                    }
                    span(class: "label-text text-xs") { (t(lang, "connectors-field-use-dcr-label")) }
                }
                (text_field(&scopes_label, "scopes", &scopes, "scope.a scope.b"))
                details {
                    summary(class: "cursor-pointer text-xs text-base-content/60") { (t(lang, "connectors-advanced-summary")) }
                    div(class: "grid grid-cols-1 gap-2 mt-2") {
                        (text_field(&authorize_url_label, "authorize_url", &authorize_url, &optional_override))
                        (text_field(&token_url_label, "token_url", &token_url, &optional_override))
                        (text_field(&registration_url_label, "registration_url", &registration_url, &optional_override))
                    }
                }
            }
            // RBAC gate applies to any connector (who may *connect* it).
            (text_field(&allowed_groups_label, "allowed_groups", &allowed_groups, &allowed_groups_placeholder))
            // Audit toggle — applies to any connector. Records every tool call
            // (who/what/when/outcome) to the trail shown at the foot of this page.
            label(class: "label cursor-pointer justify-start gap-2 py-0") {
                if audit {
                    input(type: "checkbox", name: "audit", value: "1", checked: "checked", class: "checkbox checkbox-sm");
                } else {
                    input(type: "checkbox", name: "audit", value: "1", class: "checkbox checkbox-sm");
                }
                span(class: "label-text text-xs") { "Audit tool calls (log who ran each tool, and the outcome)" }
            }
            div {
                button(type: "submit", class: "btn btn-sm btn-primary") {
                    if is_edit { (t(lang, "connectors-save-changes-button")) } else { (t(lang, "connectors-add-connector-button")) }
                }
            }
        }
    }
    .to_html()
}

#[cfg(test)]
mod tests {
    use super::parse_client_json;

    #[test]
    fn parses_google_web_client_json() {
        let raw = r#"{"web":{"client_id":"abc.apps.googleusercontent.com","client_secret":"GOCSPX-xyz","auth_uri":"https://accounts.google.com/o/oauth2/auth","token_uri":"https://oauth2.googleapis.com/token","redirect_uris":["http://localhost:8080/integrations/callback"]}}"#;
        let p = parse_client_json(raw);
        assert_eq!(
            p.client_id.as_deref(),
            Some("abc.apps.googleusercontent.com")
        );
        assert_eq!(p.client_secret.as_deref(), Some("GOCSPX-xyz"));
        assert_eq!(
            p.authorize_url.as_deref(),
            Some("https://accounts.google.com/o/oauth2/auth")
        );
        assert_eq!(
            p.token_url.as_deref(),
            Some("https://oauth2.googleapis.com/token")
        );
    }

    #[test]
    fn parses_installed_and_bare_shapes() {
        let installed = r#"{"installed":{"client_id":"cid","client_secret":"sec"}}"#;
        assert_eq!(
            parse_client_json(installed).client_id.as_deref(),
            Some("cid")
        );
        let bare = r#"{"client_id":"cid2"}"#;
        assert_eq!(parse_client_json(bare).client_id.as_deref(), Some("cid2"));
    }

    #[test]
    fn malformed_json_yields_none() {
        assert!(parse_client_json("not json").client_id.is_none());
        assert!(parse_client_json("{}").client_id.is_none());
    }
}
