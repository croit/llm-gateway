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
    NavItem, fetch_sidebar_chat, internal_error_html, nav_or_html_page, read_form,
    require_admin_or_403,
};
use crate::rama_server::state::RamaState;
use crate::server::db::mcp_catalog::{self, AuthKind, Connector, ConnectorInput};
use session_core::chrome::{NavSections, Theme, is_datastar_request};

// ---------------------------------------------------------------------------
// GET /admin/connectors

pub async fn connectors_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
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
    let body = render_body(&connectors, &redirect_uri);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        nav,
        NavItem::Connectors,
        "Connectors — LLM Gateway",
        &user.email,
        true,
        session.impersonator_id.is_some(),
        body,
        "/admin/connectors",
        &chat,
    )
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
    use_dcr: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    authorize_url: Option<String>,
    token_url: Option<String>,
    registration_url: Option<String>,
    scopes: Option<String>,
    required_role: Option<String>,
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
        return internal_error_html(&user.email, "key, name and URL are required");
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
                    "couldn't read a client_id from the pasted JSON — expected the Google \
                     OAuth client file ({\"web\":{\"client_id\":…,\"client_secret\":…}}).",
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
        Some(secret) => match state.mcp_crypto.seal_str(&secret) {
            Ok(s) => Some(s),
            Err(err) => return internal_error_html(&user.email, &format!("sealing secret: {err}")),
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

    let input = ConnectorInput {
        key: key.clone(),
        name: form.name.trim().to_string(),
        description: clean(form.description),
        icon: clean(form.icon),
        category: clean(form.category),
        url: form.url.trim().to_string(),
        auth: AuthKind::parse(form.auth.as_deref().unwrap_or("oauth2")),
        use_dcr: form.use_dcr.is_some(),
        client_id,
        client_secret_ct: sealed.as_ref().map(|s| s.ciphertext.clone()),
        client_secret_nonce: sealed.as_ref().map(|s| s.nonce.clone()),
        authorize_url,
        token_url,
        registration_url: clean(form.registration_url),
        scopes,
        required_role: clean(form.required_role),
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
        return internal_error_html(&user.email, &format!("saving connector: {err}"));
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
        return internal_error_html(
            &user.email,
            "this connector needs an OAuth client id before it can be enabled \
             (it can't use dynamic registration). Edit it and add the client id/secret.",
        );
    }
    if let Err(err) = mcp_catalog::set_enabled(&state.db, &key, enabled).await {
        return internal_error_html(&user.email, &format!("toggling connector: {err}"));
    }
    redirect("/admin/connectors")
}

pub async fn connectors_delete(
    State(state): State<Arc<RamaState>>,
    Path(key): Path<String>,
    req: Request,
) -> Response {
    let (_session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Err(err) = mcp_catalog::delete(&state.db, &key).await {
        return internal_error_html(&user.email, &format!("deleting connector: {err}"));
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
    let (_session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Err(err) = mcp_catalog::seed_defaults(&state.db).await {
        return internal_error_html(&user.email, &format!("restoring defaults: {err}"));
    }
    redirect("/admin/connectors")
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

fn render_body(connectors: &[Connector], redirect_uri: &str) -> Html {
    let rows: Vec<Html> = connectors
        .iter()
        .map(|c| render_connector_row(c, redirect_uri))
        .collect();
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
            div(class: "flex items-center justify-between gap-3 mb-2 flex-wrap") {
                h1(class: "text-2xl font-bold m-0") { "Connectors" }
                form(method: "post", action: "/admin/connectors/restore-defaults", class: "m-0") {
                    button(type: "submit", class: "btn btn-sm btn-ghost") { "Restore defaults" }
                }
            }
            p(class: "text-base-content/60 text-sm mb-6") {
                "Curate the MCP servers users can connect under Integrations. Enable a "
                "connector to make it visible. Connectors that can't use dynamic client "
                "registration (e.g. Google) need a deployment OAuth client id/secret before "
                "they can be enabled."
            }
            (render_add_form(redirect_uri))
            if connectors.is_empty() {
                div(class: "card border border-base-300") {
                    div(class: "card-body") {
                        p(class: "text-base-content/60 text-sm m-0") { "No connectors yet." }
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

fn render_connector_row(c: &Connector, redirect_uri: &str) -> Html {
    let enabled = c.enabled;
    let key = c.key.clone();
    let name = c.name.clone();
    let icon_text = c.icon.clone().unwrap_or_default();
    let logo = session_core::icons::connector_logo(&c.key, 22)
        .unwrap_or_else(|| html! { span(class: "text-xl leading-none") { (icon_text) } }.to_html());
    let url = c.url.clone();
    let toggle_action = format!("/admin/connectors/{key}/toggle");
    let delete_action = format!("/admin/connectors/{key}/delete");
    let has_secret = c.client_secret_ct.is_some();
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
                                span(class: "badge badge-success badge-sm") { "Enabled" }
                            } else {
                                span(class: "badge badge-ghost badge-sm") { "Disabled" }
                            }
                            if c.seeded {
                                span(class: "badge badge-outline badge-sm") { "Default" }
                            }
                            if c.use_dcr {
                                span(class: "badge badge-outline badge-sm") { "DCR" }
                            }
                            if c.needs_setup() {
                                span(class: "badge badge-warning badge-sm") { "Needs client id" }
                            }
                        }
                        p(class: "text-xs text-base-content/50 m-0 mt-0.5 break-all") { (url) }
                    }
                    div(class: "flex items-center gap-2 shrink-0") {
                        form(method: "post", action: (toggle_action), class: "m-0") {
                            if enabled {
                                button(type: "submit", class: "btn btn-xs btn-ghost") { "Disable" }
                            } else if c.needs_setup() {
                                // Required OAuth client id missing → can't be
                                // enabled yet. Grey it out instead of erroring
                                // on click; the Edit form + help box explain it.
                                button(type: "button", disabled: "disabled",
                                       class: "btn btn-xs btn-primary btn-disabled",
                                       title: "Add the OAuth client id below first (Edit → OAuth client id)") {
                                    "Enable"
                                }
                            } else {
                                button(type: "submit", name: "enabled", value: "1", class: "btn btn-xs btn-primary") { "Enable" }
                            }
                        }
                        form(method: "post", action: (delete_action), class: "m-0",
                             onsubmit: "return confirm('Delete this connector? It is removed for all users, along with their stored connections and tokens. This cannot be undone.')") {
                            button(type: "submit", class: "btn btn-xs btn-ghost text-error") { "Delete" }
                        }
                    }
                }
                details {
                    summary(class: "cursor-pointer text-sm text-base-content/70") { "Edit" }
                    (render_edit_form(c, has_secret, redirect_uri))
                }
            }
        }
    }
    .to_html()
}

fn render_add_form(redirect_uri: &str) -> Html {
    html! {
        details(class: "card border border-base-300 mb-2") {
            summary(class: "cursor-pointer card-body py-3 font-medium text-sm") { "Add a connector" }
            div(class: "card-body pt-0") {
                (render_form_fields(None, false, redirect_uri))
            }
        }
    }
    .to_html()
}

fn render_edit_form(c: &Connector, has_secret: bool, redirect_uri: &str) -> Html {
    html! {
        div(class: "mt-2") {
            (render_form_fields(Some(c), has_secret, redirect_uri))
        }
    }
    .to_html()
}

/// Provider-specific help for obtaining an OAuth client (shown for connectors
/// that need a manually-created client). Always shows the redirect URI to
/// register; adds a direct link for Google / GitHub.
fn render_oauth_help(existing: Option<&Connector>, redirect_uri: &str) -> Html {
    // Token-based connectors need no OAuth client — users paste their own token.
    if existing
        .map(|c| c.auth == AuthKind::StaticBearer)
        .unwrap_or(false)
    {
        return html! {
            div(class: "rounded-md border border-info/30 bg-info/5 p-3 text-xs leading-relaxed") {
                p(class: "m-0") {
                    "Token connector: set the MCP server URL above; each user pastes their own "
                    "API token under Integrations (sent as "
                    code { "Authorization: Bearer <token>" }
                    "). No OAuth client needed."
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
                p(class: "m-0 font-medium") { "Dynamic Client Registration — no OAuth client needed" }
                p(class: "m-0 mt-1") {
                    "Just set the MCP server URL above. The server registers this gateway "
                    "automatically (RFC 7591); each user then clicks Connect and authorizes with "
                    "their own account — one sign-in covers every service the server exposes."
                }
                if is_google_ws {
                    p(class: "m-0 mt-2") {
                        "Point this at your "
                        strong { "self-hosted Google Workspace MCP server" }
                        " (e.g. "
                        a(class: "link", target: "_blank", rel: "noopener noreferrer",
                          href: "https://github.com/taylorwilsdon/google_workspace_mcp") {
                            "taylorwilsdon/google_workspace_mcp"
                        }
                        ") running in streamable-HTTP mode — URL ends in "
                        code { "/mcp/" }
                        ". That server holds the Google OAuth client and uses the "
                        strong { "GA Google APIs" }
                        " (no developer preview). Allow this gateway's redirect URI on the server "
                        "via "
                        code { "WORKSPACE_MCP_ALLOWED_CLIENT_REDIRECT_URIS" }
                        ":"
                    }
                    code(class: "block mt-1 p-1.5 rounded bg-base-300/60 break-all select-all") {
                        (redirect_uri)
                    }
                    p(class: "m-0 mt-2 text-base-content/60") {
                        "Google's hosted MCP endpoints (gmailmcp/calendarmcp/drivemcp.googleapis.com) "
                        "are intentionally not used — they require enrolling the org in the Workspace "
                        "Developer Preview Program. See docs/connectors.md for the deploy recipe."
                    }
                }
            }
        }
        .to_html();
    }
    let is_google = category == "Google" || key.starts_with("google") || key == "gmail";
    let is_github = key == "github";
    let redirect_uri = redirect_uri.to_string();
    html! {
        div(class: "rounded-md border border-info/30 bg-info/5 p-3 text-xs leading-relaxed") {
            p(class: "m-0 font-medium") { "Setting up the OAuth client" }
            p(class: "m-0 mt-1") {
                "Register this exact redirect URI with your OAuth client, then paste its "
                "client id (and secret) below:"
            }
            code(class: "block mt-1 mb-2 p-1.5 rounded bg-base-300/60 break-all select-all") {
                (redirect_uri)
            }
            if is_google {
                p(class: "m-0") {
                    "Google: create an "
                    a(class: "link", target: "_blank", rel: "noopener noreferrer",
                      href: "https://console.cloud.google.com/apis/credentials") {
                        "OAuth 2.0 Client ID (Web application)"
                    }
                    " in Google Cloud Console, add the redirect URI above, and enable the "
                    "Gmail / Google Calendar / Google Drive APIs for the project."
                }
            } else if is_github {
                p(class: "m-0") {
                    "GitHub: create an "
                    a(class: "link", target: "_blank", rel: "noopener noreferrer",
                      href: "https://github.com/settings/developers") {
                        "OAuth App"
                    }
                    " (Settings → Developer settings → OAuth Apps), set the Authorization "
                    "callback URL to the redirect URI above, and copy the Client ID + a "
                    "generated client secret."
                }
            } else {
                p(class: "m-0") {
                    "Create an OAuth client at your provider with this redirect URI and the "
                    "authorize / token URLs set below."
                }
            }
            p(class: "m-0 mt-2 text-base-content/60") {
                "Why a one-time admin step? In OAuth the client id identifies "
                strong { "this gateway" } " as an app (shared by all users) — only the "
                "per-user access token differs. Claude Desktop skips it because Anthropic "
                "ships pre-registered apps tied to its fixed redirect URL; a self-hosted "
                "gateway uses its own redirect URI (above), and Google/GitHub don't support "
                "automatic registration (DCR) the way Atlassian does — so you register once, "
                "then every user just clicks Connect. "
                strong { "No OAuth app at all?" }
                " Switch Authentication to “User-supplied token” and each user pastes their "
                "own token (e.g. a GitHub Personal Access Token) — credentials then come "
                "straight from the user, no admin client."
            }
        }
    }
    .to_html()
}

/// The shared create/edit form. `existing` pre-fills the fields (and pins the
/// key read-only); `None` renders a blank create form.
fn render_form_fields(existing: Option<&Connector>, has_secret: bool, redirect_uri: &str) -> Html {
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
    let required_role = existing
        .and_then(|c| c.required_role.clone())
        .unwrap_or_default();
    let use_dcr = existing.map(|c| c.use_dcr).unwrap_or(true);
    let auth_static = existing
        .map(|c| c.auth == AuthKind::StaticBearer)
        .unwrap_or(false);
    let is_edit = existing.is_some();
    let secret_placeholder = if has_secret {
        "•••••••• (leave blank to keep)"
    } else {
        "client secret (optional)"
    };

    let text_field = |label: &str, fname: &str, val: &str, ph: &str| -> Html {
        let label = label.to_string();
        let fname = fname.to_string();
        let val = val.to_string();
        let ph = ph.to_string();
        html! {
            label(class: "form-control w-full") {
                span(class: "label-text text-xs") { (label) }
                input(
                    type: "text", name: (fname), value: (val), placeholder: (ph),
                    class: "input input-bordered input-sm w-full"
                );
            }
        }
        .to_html()
    };

    html! {
        form(method: "post", action: "/admin/connectors", class: "flex flex-col gap-2") {
            div(class: "grid grid-cols-1 sm:grid-cols-2 gap-2") {
                if is_edit {
                    label(class: "form-control w-full") {
                        span(class: "label-text text-xs") { "Key" }
                        input(type: "text", name: "key", value: (key.clone()), readonly: "readonly",
                              class: "input input-bordered input-sm w-full opacity-60");
                    }
                } else {
                    (text_field("Key (stable id)", "key", "", "e.g. gmail"))
                }
                (text_field("Name", "name", &name, "Display name"))
                (text_field("Icon (emoji)", "icon", &icon, "📧"))
                (text_field("Category", "category", &category, "Google"))
            }
            (text_field("Description", "description", &description, "What this connector does"))
            (text_field("MCP server URL", "url", &url, "https://…/mcp"))
            label(class: "form-control w-full") {
                span(class: "label-text text-xs") { "Authentication" }
                select(name: "auth", class: "select select-bordered select-sm w-full") {
                    if auth_static {
                        option(value: "oauth2") { "OAuth 2.1 (each user authorizes via the provider)" }
                        option(value: "static_bearer", selected: "selected") { "User-supplied token (each user pastes their own API token)" }
                    } else {
                        option(value: "oauth2", selected: "selected") { "OAuth 2.1 (each user authorizes via the provider)" }
                        option(value: "static_bearer") { "User-supplied token (each user pastes their own API token)" }
                    }
                }
            }
            (render_oauth_help(existing, redirect_uri))
            // OAuth-client fields only make sense for OAuth connectors. A
            // user-supplied-token (static_bearer) connector has no app-level
            // client — each user pastes their own token at connect time — so
            // hide the whole block (incl. the Google client-JSON paste).
            if !auth_static {
                label(class: "form-control w-full") {
                    span(class: "label-text text-xs") {
                        "Paste OAuth client JSON (optional — e.g. Google’s “Download JSON”)"
                    }
                    textarea(name: "client_json", rows: "3", autocomplete: "off",
                             placeholder: "{\"web\":{\"client_id\":\"…\",\"client_secret\":\"…\",\"auth_uri\":\"…\",\"token_uri\":\"…\"}}",
                             class: "textarea textarea-bordered textarea-sm w-full font-mono text-xs") {}
                    span(class: "label-text-alt text-base-content/50") {
                        "Fills client id / secret (and authorize + token URLs) from the file. Or use the individual fields below."
                    }
                }
                div(class: "grid grid-cols-1 sm:grid-cols-2 gap-2") {
                    label(class: "form-control w-full") {
                        span(class: "label-text text-xs") { "OAuth client id" }
                        input(type: "text", name: "client_id", value: (client_id),
                              placeholder: "…apps.googleusercontent.com / GitHub OAuth App id",
                              class: "input input-bordered input-sm w-full");
                        span(class: "label-text-alt text-base-content/50") {
                            "The public id that identifies "
                            strong { "this gateway" }
                            " as an app to the provider — created once by an admin on the provider’s "
                            "OAuth credentials page (Google Cloud → Credentials, GitHub → OAuth Apps). "
                            "Not a per-user secret. Leave blank if DCR is enabled."
                        }
                    }
                    label(class: "form-control w-full") {
                        span(class: "label-text text-xs") { "OAuth client secret" }
                        input(type: "password", name: "client_secret", placeholder: (secret_placeholder),
                              class: "input input-bordered input-sm w-full");
                        span(class: "label-text-alt text-base-content/50") {
                            "Issued alongside the client id on the same page. Stored encrypted; "
                            "leave blank to keep the existing one."
                        }
                    }
                }
                label(class: "label cursor-pointer justify-start gap-2 py-0") {
                    if use_dcr {
                        input(type: "checkbox", name: "use_dcr", value: "1", checked: "checked", class: "checkbox checkbox-sm");
                    } else {
                        input(type: "checkbox", name: "use_dcr", value: "1", class: "checkbox checkbox-sm");
                    }
                    span(class: "label-text text-xs") { "Try dynamic client registration (RFC 7591)" }
                }
                (text_field("Scopes (space-separated)", "scopes", &scopes, "scope.a scope.b"))
                details {
                    summary(class: "cursor-pointer text-xs text-base-content/60") { "Advanced: discovery overrides" }
                    div(class: "grid grid-cols-1 gap-2 mt-2") {
                        (text_field("Authorize URL", "authorize_url", &authorize_url, "optional override"))
                        (text_field("Token URL", "token_url", &token_url, "optional override"))
                        (text_field("Registration URL", "registration_url", &registration_url, "optional override"))
                    }
                }
            }
            // RBAC gate applies to any connector (who may *connect* it).
            (text_field("Required role (RBAC gate)", "required_role", &required_role, "optional"))
            div {
                button(type: "submit", class: "btn btn-sm btn-primary") {
                    if is_edit { "Save changes" } else { "Add connector" }
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
