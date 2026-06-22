// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The per-user `/integrations` connector store.
//!
//! Every signed-in user can connect their own accounts (Gmail, Google
//! Calendar/Drive, GitHub, GitLab, Atlassian, …) to the assistant by
//! authorizing the gateway against each provider over OAuth 2.1. Once
//! connected, that connector's tools become available to the user's chats and
//! API tokens, each with a per-tool permission (always / ask / off).
//!
//! The catalog of connectable servers is admin-managed (`/admin/connectors`);
//! this page only surfaces the *enabled* ones, plus the caller's own
//! connection + per-tool state.
//!
//! OAuth flow (MCP Authorization spec): `connect` discovers the provider's
//! endpoints, optionally registers a client (DCR), and redirects the browser
//! to the provider; `callback` exchanges the code for tokens, encrypts them,
//! and persists the connection. See `server::auth::mcp_oauth`.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::{Path, Query, State};
use rama::http::{Request, Response, StatusCode, header};
use serde::Deserialize;

use super::{
    NavItem, fetch_sidebar_chat, forbidden_html, internal_error_html, is_admin, nav_or_html_page,
    read_form, require_session_or_redirect,
};
use crate::rama_server::state::RamaState;
use crate::server::auth::mcp_oauth::{self, Overrides};
use crate::server::db::mcp_catalog::{self, AuthKind, Connector};
use crate::server::db::user_mcp::{self, NewConnection, PendingOauth, ToolMode};
use crate::server::tools::mcp::manager::ToolInfo;
use session_core::chrome::{NavSections, Theme, is_datastar_request};

// ---------------------------------------------------------------------------
// GET /integrations

pub async fn integrations_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let connectors = mcp_catalog::list_enabled(&state.db)
        .await
        .unwrap_or_default();
    let role_ids = state.rbac.role_ids_for(&user.roles);

    let mut cards: Vec<Html> = Vec::new();
    for connector in &connectors {
        // RBAC gate: hide connectors the caller's roles don't permit.
        if let Some(required) = &connector.required_role
            && !role_ids.iter().any(|r| r == required)
        {
            continue;
        }
        let connected = user_mcp::get_connection(&state.db, &user.id, &connector.key)
            .await
            .ok()
            .flatten();
        // Surface the *real* reason tools can't load (connection/transport/auth
        // error) instead of a generic message — both in the UI and the log.
        let (tools, tool_error) = if connected.is_some() {
            match state.mcp.connector_tool_infos(&user.id, connector).await {
                Ok(t) => (Some(t), None),
                Err(e) => {
                    tracing::warn!(user = %user.id, connector = %connector.key, error = %e,
                        "loading connector tools for store failed");
                    (None, Some(e))
                }
            }
        } else {
            (None, None)
        };
        cards.push(render_connector_card(
            connector,
            connected.as_ref(),
            tools.as_deref(),
            tool_error.as_deref(),
        ));
    }

    let body = render_body(cards);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        nav,
        NavItem::Integrations,
        "Integrations — LLM Gateway",
        &user.email,
        is_admin(&state, &user),
        session.impersonator_id.is_some(),
        body,
        "/integrations",
        &chat,
    )
}

// ---------------------------------------------------------------------------
// POST /integrations/{key}/connect  → redirect to the provider

pub async fn integrations_connect(
    State(state): State<Arc<RamaState>>,
    Path(key): Path<String>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let connector = match mcp_catalog::get(&state.db, &key).await {
        Ok(Some(c)) if c.enabled => c,
        _ => return forbidden_html(&user.email, "unknown or disabled connector"),
    };
    if let Some(required) = &connector.required_role
        && !state
            .rbac
            .role_ids_for(&user.roles)
            .iter()
            .any(|r| r == required)
    {
        return forbidden_html(&user.email, "you don't have access to this connector");
    }
    if connector.auth != AuthKind::OAuth2 {
        return internal_error_html(&user.email, "this connector does not use OAuth");
    }

    let public = state.config.gateway.public_url.trim_end_matches('/');
    let redirect_uri = format!("{public}/integrations/callback");
    let http = state.mcp.http();
    let ov = Overrides {
        authorize_url: connector.authorize_url.clone(),
        token_url: connector.token_url.clone(),
        registration_url: connector.registration_url.clone(),
    };
    let endpoints = match mcp_oauth::discover(http, &connector.url, &ov).await {
        Ok(e) => e,
        Err(err) => {
            return internal_error_html(&user.email, &format!("OAuth discovery failed: {err}"));
        }
    };

    // Resolve the client identity: a static configured client, or one
    // registered on the fly (DCR). The DCR client is stashed in the pending
    // row so the callback (and later refreshes) can reuse it.
    let (client_id, dcr_client_id, dcr_secret) = if let Some(cid) = connector.client_id.clone() {
        (cid, None, None)
    } else if connector.use_dcr {
        let Some(reg) = endpoints.registration_url.as_deref() else {
            return internal_error_html(
                &user.email,
                "this connector needs setup: no client id is configured and the provider \
                 offers no dynamic registration. Ask an admin to add an OAuth client.",
            );
        };
        match mcp_oauth::register_client(
            http,
            reg,
            &redirect_uri,
            "croit LLM Gateway",
            &connector.scopes,
        )
        .await
        {
            Ok((id, secret)) => {
                let sealed = match secret.as_deref() {
                    Some(s) => match state.mcp.crypto().seal_str(s) {
                        Ok(x) => Some(x),
                        Err(err) => {
                            return internal_error_html(
                                &user.email,
                                &format!("sealing client secret: {err}"),
                            );
                        }
                    },
                    None => None,
                };
                (id.clone(), Some(id), sealed)
            }
            Err(err) => {
                return internal_error_html(
                    &user.email,
                    &format!("dynamic client registration failed: {err}"),
                );
            }
        }
    } else {
        return internal_error_html(
            &user.email,
            "this connector needs setup: an admin must configure an OAuth client id.",
        );
    };

    let pkce = mcp_oauth::pkce();
    let oauth_state = mcp_oauth::random_state();
    let resource = connector.url.clone();
    let authorize_url = match mcp_oauth::build_authorize_url(
        &endpoints.authorize_url,
        &client_id,
        &redirect_uri,
        &connector.scopes,
        &oauth_state,
        &pkce.challenge,
        &resource,
    ) {
        Ok(u) => u,
        Err(err) => {
            return internal_error_html(&user.email, &format!("building authorize URL: {err}"));
        }
    };

    let pending = PendingOauth {
        state: oauth_state,
        user_id: user.id.clone(),
        connector_key: key.clone(),
        pkce_verifier: pkce.verifier,
        redirect_uri,
        token_url: endpoints.token_url,
        resource: Some(resource),
        dcr_client_id,
        dcr_client_secret_ct: dcr_secret.as_ref().map(|s| s.ciphertext.clone()),
        dcr_client_secret_nonce: dcr_secret.as_ref().map(|s| s.nonce.clone()),
        return_to: None,
    };
    if let Err(err) = user_mcp::create_pending(&state.db, &pending).await {
        return internal_error_html(&user.email, &format!("persisting authorization: {err}"));
    }
    redirect(&authorize_url)
}

// ---------------------------------------------------------------------------
// GET /integrations/callback

#[derive(Deserialize)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

pub async fn integrations_callback(
    State(state): State<Arc<RamaState>>,
    Query(params): Query<CallbackParams>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Some(err) = params.error {
        let desc = params.error_description.unwrap_or_default();
        return internal_error_html(
            &user.email,
            &format!("provider returned an error: {err} {desc}"),
        );
    }
    let (Some(code), Some(st)) = (params.code, params.state) else {
        return internal_error_html(&user.email, "callback missing code or state");
    };
    let pending = match user_mcp::take_pending(&state.db, &st).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return internal_error_html(
                &user.email,
                "this authorization has expired or was already used — start again from Integrations",
            );
        }
        Err(err) => {
            return internal_error_html(&user.email, &format!("loading authorization: {err}"));
        }
    };
    if pending.user_id != user.id {
        return forbidden_html(
            &user.email,
            "authorization state did not match your session",
        );
    }
    let connector = match mcp_catalog::get(&state.db, &pending.connector_key).await {
        Ok(Some(c)) => c,
        _ => return internal_error_html(&user.email, "the connector no longer exists"),
    };

    // Client credentials: the DCR client minted at connect time, else the
    // catalog's static client.
    let (client_id, client_secret) = if let Some(dcr) = pending.dcr_client_id.clone() {
        let secret = match (
            &pending.dcr_client_secret_ct,
            &pending.dcr_client_secret_nonce,
        ) {
            (Some(ct), Some(nonce)) => match state.mcp.crypto().open_str(nonce, ct) {
                Ok(s) => Some(s),
                Err(err) => {
                    return internal_error_html(
                        &user.email,
                        &format!("decrypting client secret: {err}"),
                    );
                }
            },
            _ => None,
        };
        (dcr, secret)
    } else {
        let Some(cid) = connector.client_id.clone() else {
            return internal_error_html(&user.email, "connector is missing its OAuth client id");
        };
        let secret = match state.mcp.decrypt_connector_secret(&connector) {
            Ok(s) => s,
            Err(err) => {
                return internal_error_html(
                    &user.email,
                    &format!("decrypting client secret: {err}"),
                );
            }
        };
        (cid, secret)
    };

    let resource = pending
        .resource
        .clone()
        .unwrap_or_else(|| connector.url.clone());
    let tokens = match mcp_oauth::exchange_code(
        state.mcp.http(),
        &pending.token_url,
        &code,
        &pending.pkce_verifier,
        &pending.redirect_uri,
        &client_id,
        client_secret.as_deref(),
        &resource,
    )
    .await
    {
        Ok(t) => t,
        Err(err) => {
            return internal_error_html(&user.email, &err.to_string());
        }
    };

    // Seal everything before it touches the DB.
    let access = match state.mcp.crypto().seal_str(&tokens.access_token) {
        Ok(s) => s,
        Err(err) => {
            return internal_error_html(&user.email, &format!("sealing access token: {err}"));
        }
    };
    let refresh = match tokens.refresh_token.as_deref() {
        Some(rt) => match state.mcp.crypto().seal_str(rt) {
            Ok(s) => Some(s),
            Err(err) => {
                return internal_error_html(&user.email, &format!("sealing refresh token: {err}"));
            }
        },
        None => None,
    };
    let scopes = if tokens.scopes.is_empty() {
        connector.scopes.clone()
    } else {
        tokens.scopes.clone()
    };

    let new = NewConnection {
        user_id: user.id.clone(),
        connector_key: pending.connector_key.clone(),
        access_token_ct: access.ciphertext,
        access_token_nonce: access.nonce,
        refresh_token_ct: refresh.as_ref().map(|s| s.ciphertext.clone()),
        refresh_token_nonce: refresh.as_ref().map(|s| s.nonce.clone()),
        token_expires_at: tokens.expires_at,
        scopes,
        dcr_client_id: pending.dcr_client_id.clone(),
        dcr_client_secret_ct: pending.dcr_client_secret_ct.clone(),
        dcr_client_secret_nonce: pending.dcr_client_secret_nonce.clone(),
        token_url: Some(pending.token_url.clone()),
    };
    if let Err(err) = user_mcp::upsert_connection(&state.db, new).await {
        return internal_error_html(&user.email, &format!("saving connection: {err}"));
    }
    state.mcp.invalidate(&user.id, &pending.connector_key).await;
    redirect("/integrations")
}

// ---------------------------------------------------------------------------
// POST /integrations/{key}/token  (static-bearer connectors: user-supplied token)

#[derive(Deserialize)]
struct TokenForm {
    token: String,
}

pub async fn integrations_connect_token(
    State(state): State<Arc<RamaState>>,
    Path(key): Path<String>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let connector = match mcp_catalog::get(&state.db, &key).await {
        Ok(Some(c)) if c.enabled => c,
        _ => return forbidden_html(&user.email, "unknown or disabled connector"),
    };
    if let Some(required) = &connector.required_role
        && !state
            .rbac
            .role_ids_for(&user.roles)
            .iter()
            .any(|r| r == required)
    {
        return forbidden_html(&user.email, "you don't have access to this connector");
    }
    if connector.auth != AuthKind::StaticBearer {
        return internal_error_html(&user.email, "this connector is not token-based");
    }
    let (_, body) = req.into_parts();
    let form: TokenForm = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let token = form.token.trim();
    if token.is_empty() {
        return internal_error_html(&user.email, "a token is required");
    }
    let sealed = match state.mcp.crypto().seal_str(token) {
        Ok(s) => s,
        Err(err) => return internal_error_html(&user.email, &format!("sealing token: {err}")),
    };
    let new = NewConnection {
        user_id: user.id.clone(),
        connector_key: key.clone(),
        access_token_ct: sealed.ciphertext,
        access_token_nonce: sealed.nonce,
        // A user-supplied token has no refresh/expiry — it's used verbatim until
        // the user replaces or disconnects it.
        refresh_token_ct: None,
        refresh_token_nonce: None,
        token_expires_at: None,
        scopes: Vec::new(),
        dcr_client_id: None,
        dcr_client_secret_ct: None,
        dcr_client_secret_nonce: None,
        token_url: None,
    };
    if let Err(err) = user_mcp::upsert_connection(&state.db, new).await {
        return internal_error_html(&user.email, &format!("saving connection: {err}"));
    }
    state.mcp.invalidate(&user.id, &key).await;
    redirect("/integrations")
}

// ---------------------------------------------------------------------------
// POST /integrations/{key}/retry — drop the cached connection and re-attempt
// on the next load (for transient "couldn't load tools" failures + token
// connectors). OAuth connectors use /connect to fully re-authorize instead.

pub async fn integrations_retry(
    State(state): State<Arc<RamaState>>,
    Path(key): Path<String>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    state.mcp.invalidate(&user.id, &key).await;
    redirect("/integrations")
}

// ---------------------------------------------------------------------------
// POST /integrations/{key}/disconnect

pub async fn integrations_disconnect(
    State(state): State<Arc<RamaState>>,
    Path(key): Path<String>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if let Err(err) = user_mcp::delete_connection(&state.db, &user.id, &key).await {
        return internal_error_html(&user.email, &format!("disconnecting: {err}"));
    }
    state.mcp.invalidate(&user.id, &key).await;
    redirect("/integrations")
}

// ---------------------------------------------------------------------------
// POST /integrations/{key}/tools/mode

#[derive(Deserialize)]
struct ToolModeForm {
    tool: String,
    mode: String,
}

pub async fn integrations_tool_mode(
    State(state): State<Arc<RamaState>>,
    Path(key): Path<String>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let form: ToolModeForm = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let Some(mode) = ToolMode::parse(&form.mode) else {
        return internal_error_html(&user.email, "invalid permission mode");
    };
    if let Err(err) = user_mcp::set_tool_mode(&state.db, &user.id, &key, &form.tool, mode).await {
        return internal_error_html(&user.email, &format!("saving tool permission: {err}"));
    }
    // Connection cache holds no mode state, but bounce through invalidate so a
    // freshly-`off`'d tool drops from the next turn's overlay immediately.
    state.mcp.invalidate(&user.id, &key).await;
    redirect("/integrations")
}

// ---------------------------------------------------------------------------
// POST /integrations/{key}/tools/all — set every tool of a connector to one mode

#[derive(Deserialize)]
struct ToolsAllForm {
    mode: String,
}

pub async fn integrations_tools_all(
    State(state): State<Arc<RamaState>>,
    Path(key): Path<String>,
    req: Request,
) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let form: ToolsAllForm = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let Some(mode) = ToolMode::parse(&form.mode) else {
        return internal_error_html(&user.email, "invalid permission mode");
    };
    let connector = match mcp_catalog::get(&state.db, &key).await {
        Ok(Some(c)) => c,
        _ => return forbidden_html(&user.email, "unknown connector"),
    };
    // Enumerate the connector's tools (live listing) and set each to `mode`.
    match state.mcp.connector_tool_infos(&user.id, &connector).await {
        Ok(tools) => {
            for t in &tools {
                if let Err(err) =
                    user_mcp::set_tool_mode(&state.db, &user.id, &key, &t.remote_name, mode).await
                {
                    return internal_error_html(&user.email, &format!("saving permissions: {err}"));
                }
            }
        }
        Err(err) => {
            return internal_error_html(&user.email, &format!("listing tools: {err}"));
        }
    }
    state.mcp.invalidate(&user.id, &key).await;
    redirect("/integrations")
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

fn render_body(cards: Vec<Html>) -> Html {
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
            h1(class: "text-2xl font-bold mb-2") { "Integrations" }
            p(class: "text-base-content/60 text-sm mb-6") {
                "Connect your own accounts so the assistant can act on your behalf — "
                "reading your email, calendar, files, repositories, and more. Each "
                "connection uses your own permissions and can be disconnected anytime."
            }
            if cards.is_empty() {
                div(class: "card border border-base-300") {
                    div(class: "card-body") {
                        p(class: "text-base-content/60 text-sm m-0") {
                            "No connectors are available yet. An administrator can enable "
                            "them under Admin → Connectors."
                        }
                    }
                }
            }
            div(class: "flex flex-col gap-4") {
                for card in cards.iter() {
                    (card.clone())
                }
            }
        }
    }
    .to_html()
}

fn render_connector_card(
    connector: &Connector,
    connection: Option<&user_mcp::Connection>,
    tools: Option<&[ToolInfo]>,
    tool_error: Option<&str>,
) -> Html {
    let connected = connection.is_some();
    let errored = connection.map(|c| c.status == "error").unwrap_or(false);
    let icon_text = connector.icon.clone().unwrap_or_default();
    let logo = session_core::icons::connector_logo(&connector.key, 26).unwrap_or_else(|| {
        html! { span(class: "text-2xl leading-none") { (icon_text) } }.to_html()
    });
    let name = connector.name.clone();
    let desc = connector.description.clone().unwrap_or_default();
    let key = connector.key.clone();
    let needs_setup = connector.needs_setup();
    let token_auth = connector.auth == AuthKind::StaticBearer;

    html! {
        section(class: "card border border-base-300") {
            div(class: "card-body gap-3") {
                div(class: "flex items-start gap-3") {
                    span(class: "shrink-0 mt-0.5") { (logo.clone()) }
                    div(class: "min-w-0 flex-1") {
                        div(class: "flex items-center gap-2 flex-wrap") {
                            h2(class: "card-title text-base m-0") { (name) }
                            if connected && !errored {
                                span(class: "badge badge-success badge-sm") { "Connected" }
                            }
                            if errored {
                                span(class: "badge badge-error badge-sm") { "Needs reconnect" }
                            }
                            if !connected && needs_setup {
                                span(class: "badge badge-ghost badge-sm") { "Needs admin setup" }
                            }
                        }
                        p(class: "text-base-content/60 text-sm m-0 mt-1") { (desc) }
                    }
                    div(class: "shrink-0") {
                        (render_connect_controls(&key, connected, needs_setup, token_auth))
                    }
                }
                if token_auth && !connected {
                    (render_token_form(&key))
                }
                if connected {
                    (render_tools(&key, tools, tool_error))
                }
            }
        }
    }
    .to_html()
}

fn render_connect_controls(
    key: &str,
    connected: bool,
    needs_setup: bool,
    token_auth: bool,
) -> Html {
    let connect_action = format!("/integrations/{key}/connect");
    let disconnect_action = format!("/integrations/{key}/disconnect");
    let reconnect_action = if token_auth {
        format!("/integrations/{key}/retry")
    } else {
        connect_action.clone()
    };
    html! {
        if connected {
            div(class: "flex items-center gap-2") {
                form(method: "post", action: (reconnect_action), class: "m-0") {
                    button(type: "submit", class: "btn btn-sm btn-ghost",
                           title: "Re-establish the connection (re-auth / retry)") { "Reconnect" }
                }
                form(method: "post", action: (disconnect_action), class: "m-0",
                     onsubmit: "return confirm('Disconnect this integration? Your stored access token will be deleted.')") {
                    button(type: "submit", class: "btn btn-sm btn-ghost text-error") { "Disconnect" }
                }
            }
        } else if token_auth {
            // The token-entry form is rendered full-width below the header.
            span {}
        } else if needs_setup {
            button(type: "button", class: "btn btn-sm", disabled: "disabled") { "Connect" }
        } else {
            form(method: "post", action: (connect_action), class: "m-0") {
                button(type: "submit", class: "btn btn-sm btn-primary") { "Connect" }
            }
        }
    }
    .to_html()
}

/// Token-entry form for a `static_bearer` connector: the user pastes their own
/// API token (e.g. an ERP `crp_…` key or a GitLab PAT). Stored encrypted; no
/// OAuth round-trip.
fn render_token_form(key: &str) -> Html {
    let action = format!("/integrations/{key}/token");
    html! {
        form(method: "post", action: (action),
             class: "border-t border-base-300 pt-3 flex items-end gap-2 flex-wrap") {
            label(class: "form-control flex-1 min-w-48") {
                span(class: "label-text text-xs") { "Your API token" }
                input(type: "password", name: "token", required: "required",
                      placeholder: "paste your token", autocomplete: "off",
                      class: "input input-bordered input-sm w-full");
            }
            button(type: "submit", class: "btn btn-sm btn-primary") { "Connect" }
        }
    }
    .to_html()
}

fn render_tools(key: &str, tools: Option<&[ToolInfo]>, tool_error: Option<&str>) -> Html {
    let Some(tools) = tools else {
        // Show the real connection/transport/auth error, not a generic line —
        // so the failure is self-diagnosable (URL wrong, server unreachable,
        // token rejected, …).
        let detail = tool_error.unwrap_or("connection unavailable").to_string();
        return html! {
            div(class: "border-t border-base-300 pt-3") {
                p(class: "text-error text-xs m-0") {
                    "Couldn't load this connector's tools: " (detail)
                }
                p(class: "text-base-content/50 text-xs m-0 mt-1") {
                    "Check the MCP server URL / your token, then use Reconnect above."
                }
            }
        }
        .to_html();
    };
    if tools.is_empty() {
        return html! {
            p(class: "text-base-content/50 text-xs m-0 border-t border-base-300 pt-3") {
                "This connector exposes no tools."
            }
        }
        .to_html();
    }
    let rows: Vec<Html> = tools.iter().map(|t| render_tool_row(key, t)).collect();
    let header = format!("Tool permissions ({})", tools.len());
    let all_action = format!("/integrations/{key}/tools/all");
    let set_all = |mode: &str, label: &str| -> Html {
        let mode = mode.to_string();
        let label = label.to_string();
        let all_action = all_action.clone();
        html! {
            form(method: "post", action: (all_action), class: "m-0") {
                button(type: "submit", name: "mode", value: (mode),
                       class: "btn btn-xs btn-ghost") { (label) }
            }
        }
        .to_html()
    };
    html! {
        div(class: "border-t border-base-300 pt-3") {
            // Header: count + "Set all" (always visible, even when collapsed).
            div(class: "flex items-center justify-between gap-3 flex-wrap mb-1") {
                span(class: "text-xs font-medium text-base-content/70") {
                    (header)
                }
                div(class: "flex items-center gap-1") {
                    span(class: "text-xs text-base-content/50 mr-1") { "Set all:" }
                    (set_all("always", "Always"))
                    (set_all("ask", "Ask"))
                    (set_all("off", "Off"))
                }
            }
            // Collapsible list — long, so collapsed by default.
            details {
                summary(class: "cursor-pointer text-xs text-base-content/60 select-none py-1") {
                    "Show / hide individual tools"
                }
                div(class: "flex flex-col divide-y divide-base-200 mt-1") {
                    for row in rows.iter() {
                        (row.clone())
                    }
                }
            }
        }
    }
    .to_html()
}

fn render_tool_row(key: &str, tool: &ToolInfo) -> Html {
    let action = format!("/integrations/{key}/tools/mode");
    let name = tool.remote_name.clone();
    let desc = tool.description.clone();
    let kind = if tool.read_only { "read" } else { "write" };
    html! {
        div(class: "flex items-center gap-3 py-2") {
            div(class: "min-w-0 flex-1") {
                div(class: "flex items-center gap-2") {
                    code(class: "text-xs font-mono") { (name) }
                    span(class: "badge badge-ghost badge-xs") { (kind) }
                }
                if !desc.is_empty() {
                    p(class: "text-xs text-base-content/50 m-0 mt-0.5 line-clamp-1") { (desc) }
                }
            }
            (render_mode_picker(&action, &name, tool.mode))
        }
    }
    .to_html()
}

/// Three submit buttons (Always / Ask / Off) — a plain form, no JS. The active
/// mode is highlighted.
fn render_mode_picker(action: &str, tool: &str, current: ToolMode) -> Html {
    let btn = |mode: ToolMode, label: &str| -> Html {
        let active = mode == current;
        let class = if active {
            "btn btn-xs btn-primary"
        } else {
            "btn btn-xs btn-ghost"
        };
        let label = label.to_string();
        let tool = tool.to_string();
        let mode_val = mode.as_str().to_string();
        html! {
            button(type: "submit", name: "mode", value: (mode_val), class: (class)) { (label) }
            // hidden tool field is shared by all three buttons via the form below
            input(type: "hidden", name: "tool", value: (tool));
        }
        .to_html()
    };
    // One form per button group; each button submits its own `mode`. The
    // `tool` hidden input is repeated per button but only the activated
    // button's form fields are submitted, so the value is unambiguous.
    html! {
        div(class: "join shrink-0") {
            form(method: "post", action: (action), class: "m-0 join-item") { (btn(ToolMode::Always, "Always")) }
            form(method: "post", action: (action), class: "m-0 join-item") { (btn(ToolMode::Ask, "Ask")) }
            form(method: "post", action: (action), class: "m-0 join-item") { (btn(ToolMode::Off, "Off")) }
        }
    }
    .to_html()
}
