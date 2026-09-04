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

use std::collections::HashSet;
use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};
use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::{Path, State};
use rama::http::{Request, Response};
use serde::Deserialize;
use uuid::Uuid;

use super::tool_toggles::{self, ToggleCtx};
use super::{
    NavItem, fetch_sidebar_chat, internal_error_html, is_admin, nav_or_html_page, read_form,
    require_session_or_redirect,
};
use session_core::chrome::{
    Flash, FlashKind, NavSections, Theme, is_datastar_request, read_body_to_bytes, sse_patch,
    sse_response, sse_script, sse_toast,
};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

use gateway_core::server::auth::token;
use gateway_core::server::db::limits::{self, Dimension, ManagedBy, SubjectType, Window};
use gateway_core::server::db::users::User;
use gateway_core::server::db::{token_models, token_tool_prefs, tokens, usage};
use gateway_runtime::rama_server::state::RamaState;
use gateway_runtime::server::tools::catalog::ToolEntry;

/// Where a per-token capability toggle posts + how its rows are
/// namespaced (one list per token, so DOM ids can't collide).
fn token_toggle_ctx(token_id: &str) -> ToggleCtx {
    ToggleCtx {
        post_path: format!("/tokens/{token_id}/tools/toggle"),
        row_id_prefix: format!("token-{token_id}-toolrow"),
    }
}

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
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());

    let (session, user) = require_session!(state, req);
    let list = match tokens::list_for_user(&state.db, &user.id).await {
        Ok(l) => l,
        Err(err) => {
            tracing::warn!(error = %err, "listing tokens");
            return internal_error_html(&user.email, "could not list tokens");
        }
    };
    let account = AccountSummary::new(&user, &state.rbac.role_ids_for(&user.roles), lang);
    // The capability catalog is the same for every token (it's the
    // user's role grants); each token carries its own disabled set.
    let entries = tool_toggles::entries_for_roles(&state, &user.roles);
    // Period boundaries in the viewer's timezone, like /usage does it.
    let tz = super::viewer_tz(&session, &user);
    let extras = token_extras(&state, &user, &tz).await;
    let mut rows: Vec<(TokenRowData, HashSet<String>)> = Vec::with_capacity(list.len());
    for tok in &list {
        let disabled = token_tool_prefs::disabled_for_token(&state.db, &tok.id)
            .await
            .unwrap_or_default();
        let row = row_with_policy(&state.db, TokenRowData::from_token(tok, lang)).await;
        rows.push((extras.apply(row), disabled));
    }
    let body = render_tokens_body(
        &rows,
        &entries,
        None,
        &account,
        state.push.is_some(),
        &extras,
        lang,
    );
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
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
        nav_or_html_page(
            &pctx,
            NavItem::Tokens,
            &t(lang, "tokens-page-title"),
            body,
            "/tokens",
            &chat,
        )
    }
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
    let lang = Lang::from_headers(req.headers());
    let (session, user) = require_session!(state, req);
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return sse_toast_response(FlashKind::Error, msg),
    };
    let form: CreateTokenForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return sse_toast_response(
                FlashKind::Error,
                t_args(
                    lang,
                    "tokens-malformed-form",
                    &i18n::args([("err", err.to_string().into())]),
                ),
            );
        }
    };
    let name = form.name.trim();
    if name.is_empty() || name.len() > 128 {
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-name-length"));
    }
    let ttl_days = form
        .ttl_days
        .unwrap_or(state.config().gateway.token_ttl_days)
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
        // Tool use is opt-in; a freshly minted token starts off and the
        // user flips it on via the per-token panel below.
        tools_enabled: false,
    };
    if let Err(err) = tokens::insert(&state.db, &row).await {
        tracing::warn!(error = %err, "storing token");
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-store-failed"));
    }

    // Surgical patches:
    //   1. Append the new row to `#token-list` (CSS auto-hides the
    //      empty-state paragraph once the list has children).
    //   2. Replace `#token-minted-banner` with the filled banner.
    //   3. Reset the create form so the next mint starts clean.
    //   4. Append a success toast.
    let entries = tool_toggles::entries_for_roles(&state, &user.roles);
    let row_data = TokenRowData::from_token(&row, lang);
    // A brand-new token has no disabled keys yet (and tool use is off, so
    // the panel renders collapsed regardless).
    // A brand-new token has no usage, allowlist or quota yet; it only needs
    // the page-wide model list and currency the picker renders with.
    let extras = token_extras_one(&state, &user, &row.id, &super::viewer_tz(&session, &user)).await;
    let row_html =
        render_token_row(&row_data, &entries, &HashSet::new(), &extras, lang).to_string();
    let banner_html = render_minted_banner(
        &MintedBanner {
            name: row.name.clone(),
            plaintext,
        },
        lang,
    )
    .to_string();
    sse_response(&[
        sse_patch(Some("#token-list"), Some("append"), &row_html),
        sse_patch(Some("#token-minted-banner"), Some("outer"), &banner_html),
        sse_script("document.getElementById('token-create-form').reset()"),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: t(lang, "tokens-created-toast"),
        }),
    ])
}

/// Helper: a token row rendered fresh from the DB (row line + its tool
/// panel) so we never drift between what the page initially showed and
/// what an SSE patch swaps in. `roles` resolves the capability catalog.
async fn render_row_after_state_change(
    state: &RamaState,
    user: &User,
    token_id: &str,
    tz: &str,
    lang: Lang,
) -> Option<String> {
    let list = tokens::list_for_user(&state.db, &user.id).await.ok()?;
    let token = list.iter().find(|t| t.id == token_id)?;
    let entries = tool_toggles::entries_for_roles(state, &user.roles);
    let disabled = token_tool_prefs::disabled_for_token(&state.db, token_id)
        .await
        .unwrap_or_default();
    let row = row_with_policy(&state.db, TokenRowData::from_token(token, lang)).await;
    // Re-read the extras so a patched row carries the same usage / allowlist /
    // limits the full page render would have shown — skipping this is how a
    // surgical patch silently reverts a panel to its defaults — but for this
    // token only, not the whole user's set.
    let extras = token_extras_one(state, user, token_id, tz).await;
    let row = extras.apply(row);
    Some(render_token_row(&row, &entries, &disabled, &extras, lang).to_string())
}

/// POST /tokens/{id}/revoke — form action from the row's Revoke
/// button. datastar's `@post` intercepts the submit and consumes
/// the SSE response, which swaps the row in place + surfaces a toast.
pub async fn tokens_revoke(
    State(state): State<Arc<RamaState>>,
    Path(token_id): Path<String>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (session, user) = require_session!(state, req);
    match tokens::revoke(&state.db, &user.id, &token_id).await {
        Ok(true) => {
            let Some(row_html) = render_row_after_state_change(
                &state,
                &user,
                &token_id,
                &super::viewer_tz(&session, &user),
                lang,
            )
            .await
            else {
                return sse_toast_response(FlashKind::Error, t(lang, "tokens-revoked-not-found"));
            };
            let selector = format!("#token-row-{token_id}");
            sse_response(&[
                sse_patch(Some(&selector), Some("outer"), &row_html),
                sse_toast(&Flash {
                    kind: FlashKind::Success,
                    message: t(lang, "tokens-revoked-toast"),
                }),
            ])
        }
        Ok(false) => sse_toast_response(FlashKind::Info, t(lang, "tokens-already-revoked")),
        Err(err) => {
            tracing::warn!(error = %err, %token_id, "revoke");
            sse_toast_response(FlashKind::Error, t(lang, "tokens-revoke-failed"))
        }
    }
}

/// POST /tokens/{id}/rotate — mint a fresh secret for an existing token
/// without changing its name or tool config. The old plaintext stops
/// working immediately; the configured TTL is preserved (the new
/// lifetime spans the same duration from now). The SSE response swaps the
/// row in place (so the metadata line updates) and patches the minted
/// banner with the new plaintext — exactly like a create, so the owner
/// gets the same copy-once flow.
pub async fn tokens_rotate(
    State(state): State<Arc<RamaState>>,
    Path(token_id): Path<String>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (session, user) = require_session!(state, req);
    // Find the live token so we can preserve its name + configured TTL.
    let list = match tokens::list_for_user(&state.db, &user.id).await {
        Ok(l) => l,
        Err(err) => {
            tracing::warn!(error = %err, "listing tokens");
            return sse_toast_response(FlashKind::Error, t(lang, "tokens-load-failed"));
        }
    };
    let Some(existing) = list
        .iter()
        .find(|t| t.id == token_id && t.revoked_at.is_none())
    else {
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-not-found-or-revoked"));
    };

    // Preserve the originally-configured lifetime: re-issue with the same
    // span from now, so a 90-day token stays a 90-day token.
    let ttl = existing.expires_at - existing.created_at;
    let now = Timestamp::now();
    let expires_at = now + ttl;
    let name = existing.name.clone();
    let (plaintext, hash) = token::mint();

    match tokens::rotate(&state.db, &user.id, &token_id, &hash, now, expires_at).await {
        Ok(true) => {
            let Some(row_html) = render_row_after_state_change(
                &state,
                &user,
                &token_id,
                &super::viewer_tz(&session, &user),
                lang,
            )
            .await
            else {
                return sse_toast_response(FlashKind::Error, t(lang, "tokens-rotated-not-found"));
            };
            let banner_html =
                render_minted_banner(&MintedBanner { name, plaintext }, lang).to_string();
            let selector = format!("#token-row-{token_id}");
            sse_response(&[
                sse_patch(Some(&selector), Some("outer"), &row_html),
                sse_patch(Some("#token-minted-banner"), Some("outer"), &banner_html),
                // Rotate can be triggered from a row far down the list, so
                // the freshly-patched banner (the only place the new secret
                // is shown) may be off-screen. Bring it into view so the
                // user actually sees the value to copy.
                sse_script(
                    "document.getElementById('token-minted-banner')\
                     .scrollIntoView({behavior:'smooth',block:'start'})",
                ),
                sse_toast(&Flash {
                    kind: FlashKind::Success,
                    message: t(lang, "tokens-rotated-toast"),
                }),
            ])
        }
        Ok(false) => sse_toast_response(FlashKind::Error, t(lang, "tokens-not-found-or-revoked")),
        Err(err) => {
            tracing::warn!(error = %err, %token_id, "rotate");
            sse_toast_response(FlashKind::Error, t(lang, "tokens-rotate-failed"))
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
    let lang = Lang::from_headers(req.headers());
    let (_session, user) = require_session!(state, req);
    match tokens::delete_if_revoked(&state.db, &user.id, &token_id).await {
        Ok(true) => {
            let selector = format!("#token-row-{token_id}");
            sse_response(&[
                sse_patch(Some(&selector), Some("remove"), ""),
                sse_toast(&Flash {
                    kind: FlashKind::Success,
                    message: t(lang, "tokens-removed-toast"),
                }),
            ])
        }
        Ok(false) => sse_toast_response(FlashKind::Info, t(lang, "tokens-still-active")),
        Err(err) => {
            tracing::warn!(error = %err, %token_id, "delete");
            sse_toast_response(FlashKind::Error, t(lang, "tokens-remove-failed"))
        }
    }
}

/// Form body for the per-token master "tool use" switch. `enabled` is
/// present (checkbox checked) or absent — same convergence trick as the
/// `/tools` page.
#[derive(Deserialize)]
struct MasterForm {
    enabled: Option<String>,
}

/// Form body for one per-token capability toggle.
#[derive(Deserialize)]
struct ToolToggleForm {
    tool_key: String,
    enabled: Option<String>,
}

/// True if `token_id` belongs to `user_id` (any state — we only gate
/// writes on ownership, not on revoked/expired).
async fn owns_token(state: &RamaState, user_id: &str, token_id: &str) -> bool {
    tokens::list_for_user(&state.db, user_id)
        .await
        .map(|list| list.iter().any(|t| t.id == token_id))
        .unwrap_or(false)
}

/// POST /tokens/{id}/tools/master — flip a token's master "tool use"
/// switch. Re-renders the whole row so the capability panel appears /
/// disappears with the switch.
pub async fn tokens_tools_master(
    State(state): State<Arc<RamaState>>,
    Path(token_id): Path<String>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (session, user) = require_session!(state, req);
    let (_, body) = req.into_parts();
    let form: MasterForm = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let enabled = form.enabled.is_some();
    match tokens::set_tools_enabled(&state.db, &user.id, &token_id, enabled).await {
        Ok(true) => {}
        Ok(false) => return sse_toast_response(FlashKind::Error, t(lang, "tokens-not-found")),
        Err(err) => {
            tracing::warn!(error = %err, %token_id, "set tools_enabled");
            return sse_toast_response(FlashKind::Error, t(lang, "tokens-update-failed"));
        }
    }
    let Some(row_html) = render_row_after_state_change(
        &state,
        &user,
        &token_id,
        &super::viewer_tz(&session, &user),
        lang,
    )
    .await
    else {
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-not-found"));
    };
    let selector = format!("#token-row-{token_id}");
    let message_key = if enabled {
        "tokens-tool-use-enabled-toast"
    } else {
        "tokens-tool-use-disabled-toast"
    };
    sse_response(&[
        sse_patch(Some(&selector), Some("outer"), &row_html),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: t(lang, message_key),
        }),
    ])
}

/// POST /tokens/{id}/models — replace this token's model allowlist with the
/// checked boxes.
///
/// The form posts the *selected* models, and a post that names every model
/// the owner can currently reach clears the restriction instead of storing
/// today's list. Otherwise opening the panel and saving without changing
/// anything would silently pin the token to the current catalogue, and the
/// next model the operator adds would be denied to a token nobody meant to
/// restrict.
pub async fn tokens_models(
    State(state): State<Arc<RamaState>>,
    Path(token_id): Path<String>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (session, user) = require_session!(state, req);
    let (_, body) = req.into_parts();
    let pairs: Vec<(String, String)> = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    if !owns_token(&state, &user.id, &token_id).await {
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-not-found"));
    }
    // Restricted-or-not is its own checkbox, never inferred from the ticks.
    //
    // Inferring it ("every box ticked means unrestricted") is wrong in both
    // directions, and silently: a token restricted to exactly the models the
    // deployment happens to serve today renders fully ticked, so opening the
    // panel and pressing Save would drop the restriction and hand the token
    // every model added later. Unticking everything would read as "allow
    // nothing" to the person doing it and store the unrestricted default.
    let restrict = super::checkbox_on(super::field(&pairs, "restrict"));
    let picked = super::fields_all(&pairs, "models");
    if restrict && picked.is_empty() {
        // No rows is how "unrestricted" is spelled, so an empty allowlist has
        // no representation — and would mean the opposite of what was asked.
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-models-none-picked"));
    }
    let to_store: Vec<String> = if restrict { picked } else { Vec::new() };
    if let Err(err) =
        token_models::set_for_token(&state.db, &token_id, &to_store, ManagedBy::Owner).await
    {
        tracing::warn!(error = %err, %token_id, "saving token model allowlist");
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-update-failed"));
    }
    let message = if to_store.is_empty() {
        t(lang, "tokens-models-cleared-toast")
    } else {
        t_args(
            lang,
            "tokens-models-saved-toast",
            &i18n::args([("count", (to_store.len() as i64).into())]),
        )
    };
    patch_row(
        &state,
        &user,
        &token_id,
        &super::viewer_tz(&session, &user),
        lang,
        message,
    )
    .await
}

/// POST /tokens/{id}/limits — add or update one quota rule on this token.
///
/// Self-service on purpose: the token is the caller's own, and a rule here can
/// only ever narrow what they may already spend (the owner's own budget is
/// checked first and independently). The model scope is deliberately not
/// offered — the per-token surface is about capping a credential, and a
/// per-model cap is admin territory on /admin/limits.
pub async fn tokens_limits_add(
    State(state): State<Arc<RamaState>>,
    Path(token_id): Path<String>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (session, user) = require_session!(state, req);
    let (_, body) = req.into_parts();
    let pairs: Vec<(String, String)> = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    if !owns_token(&state, &user.id, &token_id).await {
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-not-found"));
    }
    // The two selects are server-rendered, so a value that fails to parse is
    // a hand-crafted post rather than a user mistake — one generic refusal is
    // the right amount of ceremony.
    let (Some(dimension), Some(window)) = (
        Dimension::parse(super::field(&pairs, "dimension")),
        Window::parse(super::field(&pairs, "window")),
    ) else {
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-update-failed"));
    };
    let raw = super::field(&pairs, "value").trim().replace(',', ".");
    let value: f64 = match raw.parse::<f64>() {
        // `is_finite` matters as much as the sign: "inf" parses, survives
        // `value.max(0.0)`, and stores a quota that can never be reached and
        // renders as `inf`. The admin form guards this the same way.
        Ok(v) if v.is_finite() && v >= 0.0 => v,
        _ => {
            return sse_toast_response(
                FlashKind::Error,
                t_args(
                    lang,
                    "limits-invalid-value",
                    &i18n::args([("value", raw.into())]),
                ),
            );
        }
    };
    match limits::upsert_checked(
        &state.db,
        SubjectType::Token,
        &token_id,
        None,
        dimension,
        window,
        value,
        // Self-service: creates or updates the owner's own rule, and is
        // refused rather than overwriting an admin's cap on the same token.
        ManagedBy::Owner,
    )
    .await
    {
        Ok(limits::Upserted::Created | limits::Upserted::Updated) => {}
        // The refusal is a decision the write already made, reported as a
        // value — not reconstructed here from a storage error.
        Ok(limits::Upserted::RefusedAdminOwned) => {
            return sse_toast_response(FlashKind::Error, t(lang, "tokens-limits-admin-set"));
        }
        Err(err) => {
            tracing::warn!(error = %err, %token_id, "saving token limit");
            return sse_toast_response(FlashKind::Error, t(lang, "tokens-update-failed"));
        }
    }
    patch_row(
        &state,
        &user,
        &token_id,
        &super::viewer_tz(&session, &user),
        lang,
        t(lang, "tokens-limits-saved-toast"),
    )
    .await
}

/// POST /tokens/{id}/limits/delete — drop one of this token's own rules.
pub async fn tokens_limits_delete(
    State(state): State<Arc<RamaState>>,
    Path(token_id): Path<String>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (session, user) = require_session!(state, req);
    let (_, body) = req.into_parts();
    let pairs: Vec<(String, String)> = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    if !owns_token(&state, &user.id, &token_id).await {
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-not-found"));
    }
    let rule_id = super::field(&pairs, "id");
    // The scoping lives in the DELETE itself — by id *and* this token *and*
    // `managed_by = 'owner'`. The rule id arrives from the client, so a
    // plain delete-by-id would remove a global rule for anyone who guessed
    // one; and an admin's cap on this very token must survive its owner
    // pressing Remove, or capping a token would be a suggestion.
    match limits::delete_owner_rule(&state.db, &token_id, rule_id).await {
        Ok(true) => {}
        Ok(false) => {
            return sse_toast_response(FlashKind::Error, t(lang, "tokens-limits-not-yours"));
        }
        Err(err) => {
            tracing::warn!(error = %err, %token_id, "deleting token limit");
            return sse_toast_response(FlashKind::Error, t(lang, "tokens-update-failed"));
        }
    }
    patch_row(
        &state,
        &user,
        &token_id,
        &super::viewer_tz(&session, &user),
        lang,
        t(lang, "tokens-limits-removed-toast"),
    )
    .await
}

/// Re-render one row in place with a toast — the tail every per-token
/// settings write shares.
async fn patch_row(
    state: &RamaState,
    user: &User,
    token_id: &str,
    tz: &str,
    lang: Lang,
    message: String,
) -> Response {
    let Some(row_html) = render_row_after_state_change(state, user, token_id, tz, lang).await
    else {
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-not-found"));
    };
    let selector = format!("#token-row-{token_id}");
    sse_response(&[
        sse_patch(Some(&selector), Some("outer"), &row_html),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message,
        }),
    ])
}

/// POST /tokens/{id}/mcp-policy — set whether this token may use `ask`-mode
/// MCP connector tools over the API (the `'*'` default policy). Ownership is
/// verified before the write; the row is re-rendered so the toggle reflects
/// the stored state.
pub async fn tokens_mcp_policy(
    State(state): State<Arc<RamaState>>,
    Path(token_id): Path<String>,
    req: Request,
) -> Response {
    use gateway_core::server::db::user_mcp::{AskOverApi, set_token_policy};
    let lang = Lang::from_headers(req.headers());
    let (session, user) = require_session!(state, req);
    let (_, body) = req.into_parts();
    let form: MasterForm = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    // Verify the token belongs to the caller before touching its policy.
    let owns = tokens::list_for_user(&state.db, &user.id)
        .await
        .map(|list| list.iter().any(|t| t.id == token_id))
        .unwrap_or(false);
    if !owns {
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-not-found"));
    }
    let policy = if form.enabled.is_some() {
        AskOverApi::Allow
    } else {
        AskOverApi::Block
    };
    if let Err(err) = set_token_policy(&state.db, &token_id, "*", policy).await {
        tracing::warn!(error = %err, %token_id, "set token mcp policy");
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-update-failed"));
    }
    let Some(row_html) = render_row_after_state_change(
        &state,
        &user,
        &token_id,
        &super::viewer_tz(&session, &user),
        lang,
    )
    .await
    else {
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-not-found"));
    };
    let selector = format!("#token-row-{token_id}");
    let message_key = if matches!(policy, AskOverApi::Allow) {
        "tokens-mcp-ask-enabled-toast"
    } else {
        "tokens-mcp-ask-disabled-toast"
    };
    sse_response(&[
        sse_patch(Some(&selector), Some("outer"), &row_html),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: t(lang, message_key),
        }),
    ])
}

/// POST /tokens/{id}/tools/toggle — flip one capability for a token.
/// Patches just that capability's row in place.
pub async fn tokens_tools_toggle(
    State(state): State<Arc<RamaState>>,
    Path(token_id): Path<String>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (_session, user) = require_session!(state, req);
    let (_, body) = req.into_parts();
    let form: ToolToggleForm = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    if !owns_token(&state, &user.id, &token_id).await {
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-not-found"));
    }
    // Only a key the user's roles actually expose can be toggled — the
    // panel never offers others, so a request for one is bogus.
    let entries = tool_toggles::entries_for_roles(&state, &user.roles);
    let Some(entry) = entries.iter().find(|e| e.key == form.tool_key) else {
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-unknown-tool"));
    };
    let enabled = form.enabled.is_some();
    if let Err(err) = token_tool_prefs::set(&state.db, &token_id, &entry.key, enabled).await {
        tracing::warn!(error = %err, %token_id, tool_key = %entry.key, "token tool pref save");
        return sse_toast_response(FlashKind::Error, t(lang, "tokens-save-pref-failed"));
    }
    let ctx = token_toggle_ctx(&token_id);
    let selector = format!("#{}", ctx.row_id(&entry.key));
    let row_html = tool_toggles::render_toggle_row(entry, enabled, &ctx).to_string();
    let message_key = if enabled {
        "tokens-capability-enabled-toast"
    } else {
        "tokens-capability-disabled-toast"
    };
    let message = t_args(
        lang,
        message_key,
        &i18n::args([("name", entry.title.clone().into())]),
    );
    sse_response(&[
        sse_patch(Some(&selector), Some("outer"), &row_html),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message,
        }),
    ])
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
    fn new(user: &User, role_ids: &[String], lang: Lang) -> Self {
        let join_or = |items: &[String], empty: String| {
            if items.is_empty() {
                empty
            } else {
                items.join(", ")
            }
        };
        Self {
            email: user.email.clone(),
            user_id: user.id.clone(),
            oidc_roles: join_or(&user.roles, t(lang, "tokens-roles-none")),
            rbac_roles: join_or(role_ids, t(lang, "tokens-roles-none-granted")),
        }
    }
}

/// Compact, deliberately understated identity card. Same data the old
/// dashboard surfaced (email, user id, OIDC roles, RBAC role IDs) but
/// tucked at the foot of the tokens page where it doesn't compete with
/// the primary task.
fn render_account_section(account: &AccountSummary, lang: Lang) -> Html {
    let user_id = account.user_id.clone();
    let oidc_roles = account.oidc_roles.clone();
    let rbac_roles = account.rbac_roles.clone();
    let signed_in_as = t_args(
        lang,
        "tokens-signed-in-as",
        &i18n::args([("email", account.email.clone().into())]),
    );
    html! {
        section(class: "card border border-base-300 mt-6") {
            div(class: "card-body") {
                h2(class: "card-title text-base") { (t(lang, "tokens-account-heading")) }
                p(class: "text-base-content/60 text-sm") { (signed_in_as) }
                // `minmax(0, 1fr)` on the value column lets the long
                // UUID shrink to the card width instead of overflowing.
                dl(class: "grid grid-cols-[8rem_minmax(0,1fr)] gap-y-2 gap-x-4 text-sm mt-2") {
                    dt(class: "text-base-content/60") { (t(lang, "tokens-account-user-id-label")) }
                    dd(class: "font-mono text-xs break-all min-w-0") { (user_id) }
                    dt(class: "text-base-content/60") { (t(lang, "tokens-account-oidc-label")) }
                    dd(class: "min-w-0 break-words") { (oidc_roles) }
                    dt(class: "text-base-content/60") { (t(lang, "tokens-account-rbac-label")) }
                    dd(class: "min-w-0 break-words") { (rbac_roles) }
                }
            }
        }
    }
    .to_html()
}

fn render_tokens_body(
    rows: &[(TokenRowData, HashSet<String>)],
    entries: &[ToolEntry],
    minted: Option<&MintedBanner>,
    account: &AccountSummary,
    push_enabled: bool,
    extras: &TokenExtras,
    lang: Lang,
) -> Html {
    // The banner is either the rendered minted-card or an empty
    // placeholder that the create handler can patch in via SSE
    // (`mode outer` on `#token-minted-banner`).
    let banner = match minted {
        Some(b) => render_minted_banner(b, lang),
        None => empty_banner_placeholder(),
    };
    // Web Push opt-in card — only when the gateway has push enabled. The card
    // is device-local (JS reveals + wires it via `ui/ts/push.ts`), so it ships
    // hidden with every string as a `data-msg-*` attribute.
    let push_card = if push_enabled {
        render_push_card(lang)
    } else {
        html! {}.to_html()
    };
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
        h1(class: "text-2xl font-bold mb-2") { (t(lang, "tokens-page-heading")) }
        p(class: "text-base-content/60 text-sm mb-6") {
            (t(lang, "tokens-intro"))
        }

        (banner)

        (push_card)

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
                h2(class: "card-title") { (t(lang, "tokens-create-heading")) }
                p(class: "text-base-content/70") {
                    (t(lang, "tokens-create-description"))
                }
                label(class: "flex flex-col gap-1 w-full") {
                    div(class: "label") {
                        span(class: "label-text") { (t(lang, "tokens-name-label")) }
                    }
                    input(
                        id: "name",
                        name: "name",
                        type: "text",
                        required: "required",
                        placeholder: (t(lang, "tokens-name-placeholder")),
                        class: "input input-bordered w-full"
                    );
                }
                label(class: "flex flex-col gap-1 w-32") {
                    div(class: "label") {
                        span(class: "label-text") { (t(lang, "tokens-ttl-label")) }
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
                    button(type: "submit", class: "btn btn-primary") { (t(lang, "tokens-create-submit")) }
                }
            }
        }

        section(class: "card border border-base-300") {
            div(class: "card-body") {
                h2(class: "card-title") { (t(lang, "tokens-list-heading")) }
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
                    for (r, disabled) in rows.iter() {
                        (render_token_row(r, entries, disabled, extras, lang))
                    }
                }
                p(class: "token-list-empty text-base-content/60 text-sm") {
                    (t(lang, "tokens-list-empty"))
                }
            }
        }

        (render_account_section(account, lang))
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
    rotate_action: String,
    delete_action: String,
    /// Master "tool use" switch state — drives the per-token tool panel.
    tools_enabled: bool,
    /// Whether this token allows `ask`-mode MCP connector tools over the API
    /// (the `'*'` default policy). Defaults false; set via `row_with_policy`.
    mcp_allow: bool,
    /// This token's two model lists: the owner's, which this page edits, and
    /// the operator's, which it only displays. `None` on a side means that
    /// author has set nothing; what the gateway enforces is the intersection.
    model_lists: token_models::TokenModelLists,
    /// Usage attributable to this token in the current calendar month, or
    /// `None` when usage recording is off.
    usage: Option<super::TokenUsage>,
    /// The token's own limit rules (the additional ceiling), if any.
    limits: Vec<limits::LimitRule>,
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
    fn rotate_directive(&self) -> String {
        format!("@post('{}', {{contentType: 'form'}})", self.rotate_action)
    }
    fn delete_directive(&self) -> String {
        format!("@post('{}', {{contentType: 'form'}})", self.delete_action)
    }
}

impl TokenRowData {
    /// Build a row from a stored token, localizing the "created / last
    /// used / expires" meta line. A plain associated function rather than
    /// `From` since it now needs `lang` to render that text.
    fn from_token(token: &tokens::Token, lang: Lang) -> Self {
        let revoked = token.revoked_at.is_some();
        let created = token.created_at.strftime("%Y-%m-%d").to_string();
        let last_used = token
            .last_used_at
            .map(|lu| lu.strftime("%Y-%m-%d").to_string())
            .unwrap_or_else(|| t(lang, "tokens-last-used-never"));
        let expires = token.expires_at.strftime("%Y-%m-%d").to_string();
        let meta = t_args(
            lang,
            "tokens-row-meta",
            &i18n::args([
                ("created", created.into()),
                ("last_used", last_used.into()),
                ("expires", expires.into()),
            ]),
        );
        Self {
            id: token.id.clone(),
            name: token.name.clone(),
            meta,
            revoked,
            revoke_action: format!("/tokens/{}/revoke", token.id),
            rotate_action: format!("/tokens/{}/rotate", token.id),
            delete_action: format!("/tokens/{}/delete", token.id),
            tools_enabled: token.tools_enabled,
            mcp_allow: false,
            model_lists: token_models::TokenModelLists::default(),
            usage: None,
            limits: Vec::new(),
        }
    }
}

/// Everything the token rows need beyond the `tokens` table itself, read
/// once per page rather than once per row: month-to-date usage per token, the
/// model allowlists, and the per-token limit rules.
struct TokenExtras {
    usage: std::collections::HashMap<String, super::TokenUsage>,
    allowlists: std::collections::HashMap<String, token_models::TokenModelLists>,
    /// Indexed by token id, so applying them to a row is a lookup rather than
    /// a scan of every rule the user owns.
    limits: std::collections::HashMap<String, Vec<limits::LimitRule>>,
    /// Usage recording is off, so the usage column is meaningless rather than
    /// zero. Worth distinguishing: "no traffic" and "we are not counting" look
    /// identical otherwise.
    usage_enabled: bool,
    /// Every model the owner can reach — the allowlist picker's universe.
    models: Vec<String>,
    /// The deployment currency, for the usage line and the quota labels.
    currency: String,
}

/// Read the extras for one user's tokens. Three queries for the whole page.
async fn token_extras(state: &RamaState, user: &User, tz: &str) -> TokenExtras {
    let user_id = user.id.as_str();
    // The allowlist can only narrow what the owner's groups already reach, so
    // the picker's universe is exactly that.
    let models = state
        .upstreams
        .all_models_for(&state.pool_access_for(&user.roles));
    let now = Timestamp::now();
    let bounds = usage::period_bounds(usage::Period::ThisMonth, tz, now);
    let by_token = usage::by_token(
        &state.db,
        bounds,
        Some(user_id),
        state.config().usage.retention_days,
        now,
    )
    .await
    .unwrap_or_default();
    TokenExtras {
        usage: by_token
            .iter()
            .filter(|g| !g.key.is_empty())
            .map(|g| (g.key.clone(), super::TokenUsage::from(g)))
            .collect(),
        // The editor needs the two lists apart, not the resolved one.
        allowlists: token_models::lists_for_user(&state.db, user_id)
            .await
            .unwrap_or_default(),
        limits: group_by_token(
            limits::for_tokens_of_user(&state.db, user_id)
                .await
                .unwrap_or_default(),
        ),
        usage_enabled: state.usage.is_enabled(),
        models,
        currency: state.config().usage.currency.clone(),
    }
}

/// The extras for a *single* token — what the SSE patch path needs.
///
/// The page-wide loader reads one user's whole set, which is right for a full
/// render and wrong for redrawing one row after a button press: it would run
/// the month-to-date scan and both bulk reads to use one entry of each.
async fn token_extras_one(state: &RamaState, user: &User, token_id: &str, tz: &str) -> TokenExtras {
    let user_id = user.id.as_str();
    let models = state
        .upstreams
        .all_models_for(&state.pool_access_for(&user.roles));
    let now = Timestamp::now();
    let bounds = usage::period_bounds(usage::Period::ThisMonth, tz, now);
    let by_token = usage::by_token(
        &state.db,
        bounds,
        Some(user_id),
        state.config().usage.retention_days,
        now,
    )
    .await
    .unwrap_or_default();
    let mut allowlists = std::collections::HashMap::new();
    if let Ok(lists) = token_models::lists_for_token(&state.db, token_id).await {
        allowlists.insert(token_id.to_string(), lists);
    }
    TokenExtras {
        usage: by_token
            .iter()
            .filter(|g| g.key == token_id)
            .map(|g| (g.key.clone(), super::TokenUsage::from(g)))
            .collect(),
        allowlists,
        limits: group_by_token(
            limits::applicable_for_token(&state.db, token_id)
                .await
                .unwrap_or_default(),
        ),
        usage_enabled: state.usage.is_enabled(),
        models,
        currency: state.config().usage.currency.clone(),
    }
}

/// Index rules by their subject token once, rather than rescanning the whole
/// list per row (`O(rows × rules)` on a page that renders every token).
fn group_by_token(
    rules: Vec<limits::LimitRule>,
) -> std::collections::HashMap<String, Vec<limits::LimitRule>> {
    let mut out: std::collections::HashMap<String, Vec<limits::LimitRule>> =
        std::collections::HashMap::new();
    for r in rules {
        out.entry(r.subject_id.clone()).or_default().push(r);
    }
    out
}

impl TokenExtras {
    /// Apply this page's extras to one row.
    fn apply(&self, mut row: TokenRowData) -> TokenRowData {
        row.model_lists = self.allowlists.get(&row.id).cloned().unwrap_or_default();
        row.usage = self
            .usage_enabled
            .then(|| self.usage.get(&row.id).cloned().unwrap_or_default());
        row.limits = self.limits.get(&row.id).cloned().unwrap_or_default();
        row
    }
}

/// Populate the per-token MCP `ask`-over-API policy (the `'*'` default) on a
/// freshly-built row. Separate from `From` because it needs an async DB read.
async fn row_with_policy(
    db: &gateway_core::server::db::Pool,
    mut row: TokenRowData,
) -> TokenRowData {
    row.mcp_allow = matches!(
        gateway_core::server::db::user_mcp::token_ask_policy(db, &row.id, "*").await,
        Ok(gateway_core::server::db::user_mcp::AskOverApi::Allow)
    );
    row
}

/// Datastar directive for the master "tool use" switch form.
fn master_directive(token_id: &str) -> String {
    format!("@post('/tokens/{token_id}/tools/master', {{contentType: 'form'}})")
}

/// The per-token tool controls shown under an active token: a master
/// "tool use" switch and, when on, the capability toggle list (the same
/// grouped component the `/tools` page renders). Tokens start with tool
/// use off, so the capability list is hidden until the owner opts in.
fn render_token_tools_panel(
    token_id: &str,
    tools_enabled: bool,
    mcp_allow: bool,
    entries: &[ToolEntry],
    disabled: &HashSet<String>,
    lang: Lang,
) -> Html {
    let master_action = format!("/tokens/{token_id}/tools/master");
    let directive = master_directive(token_id);
    let tool_use_aria = t(lang, "tokens-tool-use-aria");
    let sections = if tools_enabled {
        Some(tool_toggles::render_toggle_sections(
            entries,
            disabled,
            &token_toggle_ctx(token_id),
        ))
    } else {
        None
    };
    html! {
        div(class: "mt-3 pl-1") {
            form(
                action: (master_action),
                method: "post",
                class: "m-0 flex items-center gap-3",
                "data-on:change__prevent": (directive)
            ) {
                if tools_enabled {
                    input(
                        type: "checkbox",
                        name: "enabled",
                        value: "true",
                        class: "toggle toggle-primary toggle-sm",
                        checked: "checked",
                        "aria-label": (tool_use_aria.clone())
                    );
                } else {
                    input(
                        type: "checkbox",
                        name: "enabled",
                        value: "true",
                        class: "toggle toggle-primary toggle-sm",
                        "aria-label": (tool_use_aria.clone())
                    );
                }
                span(class: "text-sm font-medium text-base-content") { (t(lang, "tokens-tool-use-label")) }
                span(class: "text-xs text-base-content/60") {
                    (t(lang, "tokens-tool-use-description"))
                }
            }
            if let Some(sections) = &sections {
                details(class: "mt-2") {
                    summary(class: "text-sm text-base-content/70 cursor-pointer select-none") {
                        (t(lang, "tokens-capabilities-summary"))
                    }
                    div(class: "mt-2") { (sections) }
                }
            }
            if tools_enabled {
                (render_token_mcp_policy(token_id, mcp_allow, lang))
            }
        }
    }
    .to_html()
}

/// Per-token control for how `ask`-mode MCP connector tools behave over the
/// API. The API can't pause for interactive approval, so `ask` tools are
/// hidden by default; flipping this exposes them (treating `ask` as `always`)
/// for this token. Connected-connector read tools (`always`) are unaffected.
fn render_token_mcp_policy(token_id: &str, allow: bool, lang: Lang) -> Html {
    let action = format!("/tokens/{token_id}/mcp-policy");
    let directive = format!("@post('{action}', {{contentType: 'form'}})");
    let allow_aria = t(lang, "tokens-mcp-allow-aria");
    html! {
        form(
            action: (action),
            method: "post",
            class: "m-0 mt-2 flex items-center gap-3",
            "data-on:change__prevent": (directive)
        ) {
            if allow {
                input(type: "checkbox", name: "enabled", value: "true",
                      class: "toggle toggle-warning toggle-sm", checked: "checked",
                      "aria-label": (allow_aria.clone()));
            } else {
                input(type: "checkbox", name: "enabled", value: "true",
                      class: "toggle toggle-warning toggle-sm",
                      "aria-label": (allow_aria.clone()));
            }
            span(class: "text-sm font-medium text-base-content") { (t(lang, "tokens-mcp-allow-label")) }
            span(class: "text-xs text-base-content/60") {
                (t(lang, "tokens-mcp-allow-description"))
            }
        }
    }
    .to_html()
}

/// The token's month-to-date usage, as a compact line under the name. Shown
/// for every token including revoked ones — a revoked token's spend is
/// exactly what someone auditing the page came to see.
fn render_token_usage(u: &super::TokenUsage, currency: &str, lang: Lang) -> Html {
    let cost = format!("{:.2} {currency}", u.cost);
    let line = t_args(
        lang,
        "tokens-usage-line",
        &i18n::args([
            ("requests", u.requests.into()),
            ("tokens", u.tokens.into()),
            ("cost", cost.into()),
        ]),
    );
    html! {
        div(class: "text-xs text-base-content/60") { (line) }
    }
    .to_html()
}

/// The per-token model allowlist editor.
///
/// Two controls, deliberately: a "limit this token" checkbox that decides
/// *whether* there is an allowlist, and the per-model ticks that say what is
/// on it. The state "restricted to everything currently available" and the
/// state "unrestricted" look identical in the ticks alone but behave
/// differently the next time the operator adds a model, so the form has to
/// carry the difference explicitly rather than infer it.
///
/// An unrestricted token renders every box ticked, so switching the limit on
/// starts from "everything" and the owner unticks what they don't want.
fn render_token_models(
    token_id: &str,
    lists: &token_models::TokenModelLists,
    available: &[String],
    lang: Lang,
) -> Html {
    let action = format!("/tokens/{token_id}/models");
    let directive = format!("@post('{action}', {{contentType: 'form'}})");
    // This form edits the *owner's* list. An operator's list, when there is
    // one, is shown below it and is not editable here — it narrows this
    // token regardless of what the owner ticks.
    let allowed = lists.owner.as_ref();
    let restricted = allowed.is_some();
    let admin_note = lists.admin.as_ref().map(|a| {
        t_args(
            lang,
            "tokens-models-admin-set",
            &i18n::args([("models", a.join(", ").into())]),
        )
    });
    let boxes: Vec<Html> = available
        .iter()
        .map(|m| {
            // Unrestricted = every model ticked. A restricted token ticks
            // only what it lists.
            let on = match allowed {
                None => true,
                Some(list) => list.iter().any(|a| a == m),
            };
            super::bool_checkbox("models", m, m, on, true)
        })
        .collect();
    // A model on the allowlist that no pool serves any more: keep it, ticked,
    // as its own row. Dropping it silently on the next save would widen the
    // token without anyone asking.
    let stale: Vec<Html> = allowed
        .map(|list| {
            list.iter()
                .filter(|m| !available.iter().any(|a| a == *m))
                .map(|m| super::bool_checkbox("models", m, m, true, true))
                .collect()
        })
        .unwrap_or_default();
    let summary = if restricted {
        t_args(
            lang,
            "tokens-models-summary-restricted",
            &i18n::args([("count", (allowed.map_or(0, Vec::len) as i64).into())]),
        )
    } else {
        t(lang, "tokens-models-summary-all")
    };
    html! {
        details(class: "mt-2") {
            summary(class: "text-sm text-base-content/70 cursor-pointer select-none") {
                (summary)
            }
            form(
                action: (action),
                method: "post",
                class: "m-0 mt-2 flex flex-col gap-2",
                "data-on:submit__prevent": (directive)
            ) {
                p(class: "text-xs text-base-content/60") {
                    (t(lang, "tokens-models-help"))
                }
                if let Some(note) = &admin_note {
                    p(class: "text-xs text-warning") { (note.clone()) }
                }
                (super::bool_checkbox(
                    "restrict",
                    "on",
                    &t(lang, "tokens-models-restrict-label"),
                    restricted,
                    false,
                ))
                div(class: "flex flex-wrap gap-x-4 gap-y-1") {
                    for b in boxes.iter() { (b.clone()) }
                    for b in stale.iter() { (b.clone()) }
                }
                div {
                    button(type: "submit", class: "btn btn-outline btn-sm") {
                        (t(lang, "tokens-models-save"))
                    }
                }
            }
        }
    }
    .to_html()
}

/// The token's own quota rules plus a one-line editor to add another. This is
/// the *additional* ceiling: the owner's personal budget still applies, so
/// nothing here can widen what the token may spend.
fn render_token_limits(
    token_id: &str,
    rules: &[limits::LimitRule],
    currency: &str,
    lang: Lang,
) -> Html {
    let action = format!("/tokens/{token_id}/limits");
    let directive = format!("@post('{action}', {{contentType: 'form'}})");
    let del_action = format!("/tokens/{token_id}/limits/delete");
    let rows: Vec<Html> = rules
        .iter()
        .map(|r| render_token_limit_row(r, &del_action, currency, lang))
        .collect();
    let dim_opts: Vec<Html> = [Dimension::Requests, Dimension::Tokens, Dimension::Cost]
        .into_iter()
        .map(|d| {
            super::select_option(
                d.as_str(),
                &super::dim_label(lang, d, Some(currency)),
                d == Dimension::Requests,
            )
        })
        .collect();
    let win_opts: Vec<Html> = Window::ALL
        .into_iter()
        .map(|w| super::select_option(w.as_str(), &super::win_label(lang, w), w == Window::Day))
        .collect();
    let summary = if rules.is_empty() {
        t(lang, "tokens-limits-summary-none")
    } else {
        t_args(
            lang,
            "tokens-limits-summary-some",
            &i18n::args([("count", (rules.len() as i64).into())]),
        )
    };
    html! {
        details(class: "mt-2") {
            summary(class: "text-sm text-base-content/70 cursor-pointer select-none") {
                (summary)
            }
            div(class: "mt-2 flex flex-col gap-2") {
                p(class: "text-xs text-base-content/60") { (t(lang, "tokens-limits-help")) }
                if !rows.is_empty() {
                    ul(class: "flex flex-col gap-1") {
                        for r in rows.iter() { (r.clone()) }
                    }
                }
                form(
                    action: (action),
                    method: "post",
                    class: "m-0 flex flex-wrap items-end gap-2",
                    "data-on:submit__prevent": (directive)
                ) {
                    label(class: "flex flex-col gap-1") {
                        span(class: "label-text text-xs opacity-70") { (t(lang, "limits-field-dimension")) }
                        select(name: "dimension", class: "select select-bordered select-sm") {
                            for o in dim_opts.iter() { (o.clone()) }
                        }
                    }
                    label(class: "flex flex-col gap-1") {
                        span(class: "label-text text-xs opacity-70") { (t(lang, "limits-field-window")) }
                        select(name: "window", class: "select select-bordered select-sm") {
                            for o in win_opts.iter() { (o.clone()) }
                        }
                    }
                    label(class: "flex flex-col gap-1") {
                        span(class: "label-text text-xs opacity-70") { (t(lang, "limits-field-value")) }
                        input(
                            type: "text", name: "value", inputmode: "decimal",
                            class: "input input-bordered input-sm w-28"
                        );
                    }
                    button(type: "submit", class: "btn btn-outline btn-sm") {
                        (t(lang, "tokens-limits-add"))
                    }
                }
            }
        }
    }
    .to_html()
}

fn render_token_limit_row(
    r: &limits::LimitRule,
    del_action: &str,
    currency: &str,
    lang: Lang,
) -> Html {
    let text = super::describe_rule(lang, r, currency);
    let directive = format!("@post('{del_action}', {{contentType: 'form'}})");
    let id = r.id.clone();
    // An operator's cap is shown here — the owner should know why their token
    // stops — but it is theirs to see, not to remove. The handler enforces
    // this independently; hiding the button just keeps the UI honest.
    let admin_set = r.managed_by == ManagedBy::Admin;
    html! {
        li(class: "flex items-center gap-2 text-sm") {
            span(class: "tabular-nums") { (text) }
            if admin_set {
                span(class: "badge badge-ghost badge-sm") {
                    (t(lang, "tokens-limits-admin-badge"))
                }
            } else {
                form(
                    action: (del_action.to_string()),
                    method: "post",
                    class: "m-0",
                    "data-on:submit__prevent": (directive)
                ) {
                    input(type: "hidden", name: "id", value: (id));
                    button(type: "submit", class: "btn btn-ghost btn-xs") {
                        (t(lang, "tokens-limits-remove"))
                    }
                }
            }
        }
    }
    .to_html()
}

/// Single row in the token list. Single source of truth for both the
/// initial page render and the datastar SSE patches that surgically
/// swap (revoke) or replace (active ↔ revoked) a row in place. Active
/// tokens carry a per-token tool panel below the row line; `entries` is
/// the capability catalog and `disabled` this token's off keys.
fn render_token_row(
    r: &TokenRowData,
    entries: &[ToolEntry],
    disabled: &HashSet<String>,
    extras: &TokenExtras,
    lang: Lang,
) -> Html {
    let (models, currency) = (extras.models.as_slice(), extras.currency.as_str());
    let dom_id = r.dom_id();
    // A revoked token can't authenticate, so its tool config is moot — no
    // panel there.
    let panel = (!r.revoked).then(|| {
        render_token_tools_panel(&r.id, r.tools_enabled, r.mcp_allow, entries, disabled, lang)
    });
    // Scope and quota panels, same rule: nothing to configure on a token that
    // can no longer authenticate.
    let models_panel =
        (!r.revoked).then(|| render_token_models(&r.id, &r.model_lists, models, lang));
    let limits_panel = (!r.revoked).then(|| render_token_limits(&r.id, &r.limits, currency, lang));
    // Usage stays on a revoked row: what it spent before it was revoked is
    // exactly what an audit is looking for.
    let usage_line = r
        .usage
        .as_ref()
        .map(|u| render_token_usage(u, currency, lang));
    html! {
        li(id: (dom_id), class: "py-3") {
        div(class: "flex items-center gap-4") {
            div(class: "flex-1 min-w-0") {
                div(class: "text-sm font-medium text-base-content") {
                    (r.name.clone())
                }
                div(class: "text-xs text-base-content/60") { (r.meta.clone()) }
                if let Some(u) = &usage_line { (u) }
            }
            if r.revoked {
                // shadcn destructive badge: filled error background,
                // light error-content text. Matches the look of the
                // Revoke action that produced this state.
                span(class: "badge badge-error") { (t(lang, "tokens-badge-revoked")) }
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
                    ) { (t(lang, "tokens-remove-button")) }
                }
            } else {
                // shadcn secondary badge: filled neutral surface,
                // base-content text. "Active" is the normal state —
                // the eye shouldn't be drawn to it.
                span(class: "badge badge-secondary") { (t(lang, "tokens-badge-active")) }
                // Rotate: re-mints the secret in place (same name + tool
                // config), so the owner doesn't have to revoke + recreate
                // just to roll a key. Outline variant — it's a normal
                // maintenance action, not the loud one-way Revoke beside it.
                // The old plaintext stops working the instant this fires.
                form(
                    action: (r.rotate_action.clone()),
                    method: "post",
                    class: "m-0",
                    "data-on:submit__prevent": (r.rotate_directive())
                ) {
                    button(
                        type: "submit",
                        class: "btn btn-outline btn-sm",
                        title: (t(lang, "tokens-rotate-title"))
                    ) { (t(lang, "tokens-rotate-button")) }
                }
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
                    ) { (t(lang, "tokens-revoke-button")) }
                }
            }
        }
        if let Some(panel) = &panel {
            (panel)
        }
        if let Some(panel) = &models_panel {
            (panel)
        }
        if let Some(panel) = &limits_panel {
            (panel)
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
fn render_minted_banner(banner: &MintedBanner, lang: Lang) -> Html {
    let plain = banner.plaintext.clone();
    let name_line = t_args(
        lang,
        "tokens-minted-name",
        &i18n::args([("name", banner.name.clone().into())]),
    );
    let copy_aria = t(lang, "tokens-copy-aria");
    let copy_title = t(lang, "tokens-copy-title");
    html! {
        div(
            id: "token-minted-banner",
            class: "card mb-6"
        ) {
            div(class: "card-body") {
                div(class: "flex items-center gap-2") {
                    span(class: "text-success") { (icons::check(18)) }
                    h2(class: "card-title text-base m-0") { (t(lang, "tokens-minted-heading")) }
                }
                p(class: "text-sm text-base-content/70 mt-1 mb-3") {
                    (t(lang, "tokens-minted-copy-warning"))
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
                        "aria-label": (copy_aria),
                        title: (copy_title),
                        class: "btn btn-ghost btn-sm btn-square \
                                absolute top-1.5 right-1.5"
                    ) {
                        (icons::copy(16))
                    }
                }
                p(class: "text-xs text-base-content/60 mt-3 mb-0") {
                    (name_line)
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

/// The "Notifications" card on `/tokens`. Ships hidden with every user-visible
/// string as a `data-msg-*` attribute; `ui/ts/push.ts` reveals it, reflects
/// this browser's subscription state into `[data-push-status]`, and shows the
/// enable/disable button that applies. Device-local state, so all the logic is
/// client-side — the server only renders the (localized) shell.
fn render_push_card(lang: Lang) -> Html {
    html! {
        section(
            class: "card border border-base-300 mb-6",
            "data-push-card": "",
            hidden: "hidden",
            "data-msg-on": (t(lang, "tokens-push-on")),
            "data-msg-off": (t(lang, "tokens-push-off")),
            "data-msg-denied": (t(lang, "tokens-push-denied")),
            "data-msg-unsupported": (t(lang, "tokens-push-unsupported")),
            "data-msg-enabled": (t(lang, "tokens-push-enabled")),
            "data-msg-disabled": (t(lang, "tokens-push-disabled")),
            "data-msg-error": (t(lang, "tokens-push-error"))
        ) {
            div(class: "card-body") {
                h2(class: "card-title") { (t(lang, "tokens-push-heading")) }
                p(class: "text-base-content/70") { (t(lang, "tokens-push-description")) }
                p(class: "text-sm text-base-content/60", "data-push-status") {}
                div(class: "card-actions justify-end mt-2") {
                    button(
                        type: "button",
                        class: "btn btn-primary",
                        "data-push-enable": "",
                        hidden: "hidden",
                        "data-on:click": "window.gatewayPush.enable(el)"
                    ) { (t(lang, "tokens-push-enable")) }
                    button(
                        type: "button",
                        class: "btn btn-ghost",
                        "data-push-disable": "",
                        hidden: "hidden",
                        "data-on:click": "window.gatewayPush.disable(el)"
                    ) { (t(lang, "tokens-push-disable")) }
                }
            }
        }
    }
    .to_html()
}
