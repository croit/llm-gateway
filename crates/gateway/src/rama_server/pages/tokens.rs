// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Tokens page — list + create + revoke + delete handlers, plus the
//! minted-banner / row / list renderers. The CRUD endpoints all
//! return `text/event-stream` so the page updates in place (row
//! insert / outer-mode swap / remove + a toast) without a full
//! reload.
//!
//! Shared chrome (layout, SSE framing, toast types, session gate)
//! lives in the parent `pages` module and is imported via `super`.

use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};
use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::{Path, State};
use rama::http::{Request, Response};
use serde::Deserialize;
use uuid::Uuid;

use super::{
    NavItem, fetch_sidebar_chat, internal_error_html, is_admin, nav_or_html_page,
    require_session_or_redirect,
};
use session_core::chrome::{
    Flash, FlashKind, Theme, is_datastar_request, read_body_to_bytes, sse_patch, sse_response,
    sse_script, sse_toast,
};
use session_core::icons;

use crate::rama_server::state::RamaState;
use crate::server::auth::token;
use crate::server::db::tokens;
use crate::server::db::users::User;

// ---------------------------------------------------------------------------
// Tokens

#[derive(Deserialize)]
struct CreateTokenForm {
    name: String,
    ttl_days: Option<i64>,
}

/// GET /tokens — the token-management page. Renders the list of the
/// caller's tokens plus an inline form to mint a new one.
pub async fn tokens_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());

    let (session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let list = match tokens::list_for_user(&state.db, &user.id).await {
        Ok(l) => l,
        Err(err) => {
            tracing::warn!(error = %err, "listing tokens");
            return internal_error_html(&user.email, "could not list tokens");
        }
    };
    let account = AccountSummary::new(&user, &state.rbac.role_ids_for(&user.roles));
    let body = render_tokens_body(&list, None, &account);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        NavItem::Tokens,
        "API tokens — LLM Gateway",
        &user.email,
        is_admin(&state, &user),
        session.impersonator_id.is_some(),
        body,
        "/tokens",
        &chat,
    )
}

/// POST /tokens — form-encoded create. On success renders a one-time
/// page showing the plaintext (with a copy-friendly <pre> block) and
/// a "Done" link back to /tokens. The plaintext lives in the response
/// body once, never in a URL or a cookie.
/// Shorthand: an SSE response that fires a single toast. Used by the
/// failure / no-op branches of each datastar-driven action so the
/// caller still sees feedback without a full page reload.
fn sse_toast_response(kind: FlashKind, message: impl Into<String>) -> Response {
    sse_response(&[sse_toast(&Flash {
        kind,
        message: message.into(),
    })])
}

pub async fn tokens_create(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (_session, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return sse_toast_response(FlashKind::Error, msg),
    };
    let form: CreateTokenForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return sse_toast_response(FlashKind::Error, format!("malformed form: {err}"));
        }
    };
    let name = form.name.trim();
    if name.is_empty() || name.len() > 128 {
        return sse_toast_response(FlashKind::Error, "Token name must be 1..=128 characters.");
    }
    let ttl_days = form
        .ttl_days
        .unwrap_or(state.config.gateway.token_ttl_days)
        .clamp(1, 365 * 5);

    let now = Timestamp::now();
    let expires_at = now + SignedDuration::from_hours(24 * ttl_days);
    let (plaintext, hash) = token::mint();
    let row = tokens::Token {
        id: Uuid::new_v4().to_string(),
        user_id: user.id.clone(),
        name: name.to_string(),
        hash,
        created_at: now,
        last_used_at: None,
        expires_at,
        revoked_at: None,
    };
    if let Err(err) = tokens::insert(&state.db, &row).await {
        tracing::warn!(error = %err, "storing token");
        return sse_toast_response(FlashKind::Error, "Storing token failed.");
    }

    // Surgical patches:
    //   1. Append the new row to `#token-list` (CSS auto-hides the
    //      empty-state paragraph once the list has children).
    //   2. Replace `#token-minted-banner` with the filled banner.
    //   3. Reset the create form so the next mint starts clean.
    //   4. Append a success toast.
    let row_data = TokenRowData::from(&row);
    let row_html = render_token_row(&row_data).to_string();
    let banner_html = render_minted_banner(&MintedBanner {
        name: row.name.clone(),
        plaintext,
    })
    .to_string();
    sse_response(&[
        sse_patch(Some("#token-list"), Some("append"), &row_html),
        sse_patch(Some("#token-minted-banner"), Some("outer"), &banner_html),
        sse_script("document.getElementById('token-create-form').reset()"),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: "Token created.".into(),
        }),
    ])
}

/// Helper: the active variant of a row, rendered fresh from the DB so
/// we never drift between what the page initially showed and what
/// `tokens_revoke` patches in.
async fn render_row_after_state_change(
    state: &RamaState,
    user_id: &str,
    token_id: &str,
) -> Option<String> {
    let list = tokens::list_for_user(&state.db, user_id).await.ok()?;
    let token = list.iter().find(|t| t.id == token_id)?;
    Some(render_token_row(&TokenRowData::from(token)).to_string())
}

/// POST /tokens/{id}/revoke — form action from the row's Revoke
/// button. datastar's `@post` intercepts the submit and consumes
/// the SSE response, which swaps the row in place + surfaces a toast.
pub async fn tokens_revoke(
    State(state): State<Arc<RamaState>>,
    Path(token_id): Path<String>,
    req: Request,
) -> Response {
    let (_, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match tokens::revoke(&state.db, &user.id, &token_id).await {
        Ok(true) => {
            let Some(row_html) = render_row_after_state_change(&state, &user.id, &token_id).await
            else {
                return sse_toast_response(FlashKind::Error, "Revoked token not found.");
            };
            let selector = format!("#token-row-{token_id}");
            sse_response(&[
                sse_patch(Some(&selector), Some("outer"), &row_html),
                sse_toast(&Flash {
                    kind: FlashKind::Success,
                    message: "Token revoked.".into(),
                }),
            ])
        }
        Ok(false) => sse_toast_response(FlashKind::Info, "Token was already revoked."),
        Err(err) => {
            tracing::warn!(error = %err, %token_id, "revoke");
            sse_toast_response(FlashKind::Error, "Revoke failed.")
        }
    }
}

/// POST /tokens/{id}/delete — hard-delete a revoked row. SSE response
/// removes the `<li>` from the list (`mode remove`) + appends a toast.
pub async fn tokens_delete(
    State(state): State<Arc<RamaState>>,
    Path(token_id): Path<String>,
    req: Request,
) -> Response {
    let (_, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match tokens::delete_if_revoked(&state.db, &user.id, &token_id).await {
        Ok(true) => {
            let selector = format!("#token-row-{token_id}");
            sse_response(&[
                sse_patch(Some(&selector), Some("remove"), ""),
                sse_toast(&Flash {
                    kind: FlashKind::Success,
                    message: "Token removed.".into(),
                }),
            ])
        }
        Ok(false) => {
            sse_toast_response(FlashKind::Info, "Token is still active — revoke it first.")
        }
        Err(err) => {
            tracing::warn!(error = %err, %token_id, "delete");
            sse_toast_response(FlashKind::Error, "Remove failed.")
        }
    }
}

struct MintedBanner {
    name: String,
    plaintext: String,
}

/// The signed-in user's identity, distilled for the low-key "Account"
/// section at the bottom of /tokens. This is the info the old dashboard
/// landing page used to show front-and-centre; it's reference material
/// most users never need, so it lives here rather than on the landing
/// page (which is now the chat surface).
struct AccountSummary {
    email: String,
    user_id: String,
    oidc_roles: String,
    rbac_roles: String,
}

impl AccountSummary {
    fn new(user: &User, role_ids: &[String]) -> Self {
        let join_or = |items: &[String], empty: &str| {
            if items.is_empty() {
                empty.to_string()
            } else {
                items.join(", ")
            }
        };
        Self {
            email: user.email.clone(),
            user_id: user.id.clone(),
            oidc_roles: join_or(&user.roles, "none"),
            rbac_roles: join_or(role_ids, "none granted"),
        }
    }
}

/// Compact, deliberately understated identity card. Same data the old
/// dashboard surfaced (email, user id, OIDC roles, RBAC role IDs) but
/// tucked at the foot of the tokens page where it doesn't compete with
/// the primary task.
fn render_account_section(account: &AccountSummary) -> Html {
    let email = account.email.clone();
    let user_id = account.user_id.clone();
    let oidc_roles = account.oidc_roles.clone();
    let rbac_roles = account.rbac_roles.clone();
    html! {
        section(class: "card border border-base-300 mt-6") {
            div(class: "card-body") {
                h2(class: "card-title text-base") { "Account" }
                p(class: "text-base-content/60 text-sm") { "Signed in as " (email) }
                // `minmax(0, 1fr)` on the value column lets the long
                // UUID shrink to the card width instead of overflowing.
                dl(class: "grid grid-cols-[8rem_minmax(0,1fr)] gap-y-2 gap-x-4 text-sm mt-2") {
                    dt(class: "text-base-content/60") { "User ID" }
                    dd(class: "font-mono text-xs break-all min-w-0") { (user_id) }
                    dt(class: "text-base-content/60") { "OIDC roles" }
                    dd(class: "min-w-0 break-words") { (oidc_roles) }
                    dt(class: "text-base-content/60") { "RBAC role IDs" }
                    dd(class: "min-w-0 break-words") { (rbac_roles) }
                }
            }
        }
    }
    .to_html()
}

fn render_tokens_body(
    list: &[tokens::Token],
    minted: Option<&MintedBanner>,
    account: &AccountSummary,
) -> Html {
    let rows: Vec<TokenRowData> = list.iter().map(TokenRowData::from).collect();
    // The banner is either the rendered minted-card or an empty
    // placeholder that the create handler can patch in via SSE
    // (`mode outer` on `#token-minted-banner`).
    let banner = match minted {
        Some(b) => render_minted_banner(b),
        None => empty_banner_placeholder(),
    };
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
        h1(class: "text-2xl font-bold mb-2") { "API tokens" }
        p(class: "text-base-content/60 text-sm mb-6") {
            "Bearer tokens for the OpenAI-compatible API. The plaintext "
            "is shown only at creation time — store it somewhere safe."
        }

        (banner)

        // datastar @post: form submission is intercepted, the form is
        // serialised + POSTed, and the response (SSE) patches the page
        // in place. `action="/tokens"` stays as a no-JS fallback.
        form(
            id: "token-create-form",
            action: "/tokens",
            method: "post",
            class: "card border border-base-300 mb-6",
            "data-on:submit__prevent": "@post('/tokens', {contentType: 'form'})"
        ) {
            div(class: "card-body") {
                h2(class: "card-title") { "Create token" }
                p(class: "text-base-content/70") {
                    "Mint a new bearer token for the OpenAI-compatible API."
                }
                label(class: "form-control w-full") {
                    div(class: "label") {
                        span(class: "label-text") { "Name" }
                    }
                    input(
                        id: "name",
                        name: "name",
                        type: "text",
                        required: "required",
                        placeholder: "e.g. laptop, ci-runner",
                        class: "input input-bordered w-full"
                    );
                }
                label(class: "form-control w-32") {
                    div(class: "label") {
                        span(class: "label-text") { "TTL (days)" }
                    }
                    input(
                        id: "ttl_days",
                        name: "ttl_days",
                        type: "number",
                        min: "1",
                        max: "1825",
                        value: "90",
                        class: "input input-bordered w-full"
                    );
                }
                div(class: "card-actions justify-end mt-2") {
                    button(type: "submit", class: "btn btn-primary") { "Create token" }
                }
            }
        }

        section(class: "card border border-base-300") {
            div(class: "card-body") {
                h2(class: "card-title") { "Your tokens" }
                // Always emit the <ul>; the empty-state paragraph
                // below is hidden via CSS while the list has children
                // (`.token-list:not(:empty) ~ .token-list-empty {
                // display: none; }` — see main.css). Datastar SSE
                // patches surgically append / swap / remove rows in
                // place without a re-render.
                ul(
                    id: "token-list",
                    class: "token-list flex flex-col divide-y divide-base-300"
                ) {
                    for r in rows.iter() {
                        (render_token_row(r))
                    }
                }
                p(class: "token-list-empty text-base-content/60 text-sm") {
                    "No tokens yet. Create one above."
                }
            }
        }

        (render_account_section(account))
        }
    }
    .to_html()
}

/// Pre-formatted view of a token row. We pre-render the strings outside
/// the macro because plait's macro doesn't handle `?` chains / method
/// calls on borrowed data inside its inner closures particularly well.
struct TokenRowData {
    id: String,
    name: String,
    meta: String,
    revoked: bool,
    revoke_action: String,
    delete_action: String,
}

impl TokenRowData {
    /// DOM id for the row's `<li>` — same string the datastar SSE
    /// patches target for swap/remove ops.
    fn dom_id(&self) -> String {
        format!("token-row-{}", self.id)
    }

    /// Datastar `data-on:submit__prevent` value for the row's button
    /// form. We pass it as a string field rather than re-deriving in
    /// the template so the URL and the directive can't drift.
    fn revoke_directive(&self) -> String {
        format!("@post('{}', {{contentType: 'form'}})", self.revoke_action)
    }
    fn delete_directive(&self) -> String {
        format!("@post('{}', {{contentType: 'form'}})", self.delete_action)
    }
}

impl From<&tokens::Token> for TokenRowData {
    fn from(t: &tokens::Token) -> Self {
        let revoked = t.revoked_at.is_some();
        let created = t.created_at.strftime("%Y-%m-%d").to_string();
        let last_used = t
            .last_used_at
            .map(|lu| lu.strftime("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "never".to_string());
        let expires = t.expires_at.strftime("%Y-%m-%d").to_string();
        Self {
            id: t.id.clone(),
            name: t.name.clone(),
            meta: format!("created {created} · last used {last_used} · expires {expires}"),
            revoked,
            revoke_action: format!("/tokens/{}/revoke", t.id),
            delete_action: format!("/tokens/{}/delete", t.id),
        }
    }
}

/// Single row in the token list. Single source of truth for both the
/// initial page render and the datastar SSE patches that surgically
/// swap (revoke) or replace (active ↔ revoked) a row in place.
fn render_token_row(r: &TokenRowData) -> Html {
    let dom_id = r.dom_id();
    html! {
        li(id: (dom_id), class: "flex items-center gap-4 py-3") {
            div(class: "flex-1 min-w-0") {
                div(class: "text-sm font-medium text-base-content") {
                    (r.name.clone())
                }
                div(class: "text-xs text-base-content/60") { (r.meta.clone()) }
            }
            if r.revoked {
                // shadcn destructive badge: filled error background,
                // light error-content text. Matches the look of the
                // Revoke action that produced this state.
                span(class: "badge badge-error") { "revoked" }
                // Outline variant — cleanup of an already-revoked
                // row isn't destructive (the damage is done), but
                // ghost reads as "no action available" in shadcn's
                // visual language. Outline gives a visible border +
                // hover lift without committing to the destructive
                // colour.
                form(
                    action: (r.delete_action.clone()),
                    method: "post",
                    class: "m-0",
                    "data-on:submit__prevent": (r.delete_directive())
                ) {
                    button(
                        type: "submit",
                        class: "btn btn-outline btn-sm"
                    ) { "Remove" }
                }
            } else {
                // shadcn secondary badge: filled neutral surface,
                // base-content text. "Active" is the normal state —
                // the eye shouldn't be drawn to it.
                span(class: "badge badge-secondary") { "active" }
                // shadcn destructive button: filled error background,
                // light text, hover dims to /90. Loud on purpose —
                // revoking is one-way without an admin.
                form(
                    action: (r.revoke_action.clone()),
                    method: "post",
                    class: "m-0",
                    "data-on:submit__prevent": (r.revoke_directive())
                ) {
                    button(
                        type: "submit",
                        class: "btn btn-error btn-sm"
                    ) { "Revoke" }
                }
            }
        }
    }
    .to_html()
}

/// The minted-token banner shown right after a successful create.
/// Single source of truth for both the initial page render (no banner)
/// and the SSE patch that swaps the placeholder for a filled banner.
///
/// Visual model: shadcn-style callout. The card sits on the page like
/// any other card (no loud `border-success` outline — that read as a
/// modal-ish "alert", out of place against the muted list below it).
/// The success vibe comes from a small check-circle in `text-success`
/// next to the title — exactly how shadcn's Alert / Callout components
/// surface variant intent.
///
/// The token `<pre>` is intentionally `bg-base-100`: the parent card
/// is `bg-base-200`, so the pre reads as an *inset* surface — a
/// distinct shelf inside the card rather than a transparent slab. A
/// 1 px `border-base-300` reinforces the edge for high-contrast themes
/// where the bg delta would otherwise be too subtle.
///
/// The copy button is a `btn-ghost btn-sm btn-square` floated top-right
/// of the pre. `data-copy-target="#minted-token-value"` is read by the
/// `window.uiCopy` helper (ui/ts/clipboard.ts), which is wired via the
/// button's `data-on:click` — no need to reflect the plaintext into a
/// data-attribute (which would put the secret in the DOM twice).
fn render_minted_banner(banner: &MintedBanner) -> Html {
    let name = banner.name.clone();
    let plain = banner.plaintext.clone();
    html! {
        div(
            id: "token-minted-banner",
            class: "card mb-6"
        ) {
            div(class: "card-body") {
                div(class: "flex items-center gap-2") {
                    span(class: "text-success") { (icons::check(18)) }
                    h2(class: "card-title text-base m-0") { "Token created" }
                }
                p(class: "text-sm text-base-content/70 mt-1 mb-3") {
                    "Copy the value now — you won't be able to see it again."
                }
                // `relative` wrapper so the copy button can anchor
                // top-right of the pre via `absolute`. The pre's
                // `pr-12` reserves space for the button so long tokens
                // don't wrap under it.
                div(class: "relative") {
                    pre(
                        id: "minted-token-value",
                        class: "bg-base-100 border border-base-300 \
                                text-base-content rounded-md p-3 pr-12 m-0 \
                                font-mono text-xs select-all break-all \
                                whitespace-pre-wrap w-full min-w-0"
                    ) {
                        (plain)
                    }
                    button(
                        type: "button",
                        "data-copy-target": "#minted-token-value",
                        "data-on:click": "window.uiCopy(el)",
                        "aria-label": "Copy token",
                        title: "Copy token",
                        class: "btn btn-ghost btn-sm btn-square \
                                absolute top-1.5 right-1.5"
                    ) {
                        (icons::copy(16))
                    }
                }
                p(class: "text-xs text-base-content/60 mt-3 mb-0") {
                    "Name: " (name)
                }
            }
        }
    }
    .to_html()
}

/// Empty placeholder element that occupies the banner slot until a
/// create succeeds. Lets the SSE response patch the slot with
/// `mode outer` and the banner HTML.
fn empty_banner_placeholder() -> Html {
    html! {
        div(id: "token-minted-banner") {}
    }
    .to_html()
}
