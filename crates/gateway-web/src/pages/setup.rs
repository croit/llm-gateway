// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The deployment setup wizard: `/setup`.
//!
//! A container starts with one environment variable and nothing else. This is
//! where an operator turns that into a working gateway — entirely in the
//! browser, with no configuration file and no shell.
//!
//! # Two screens, one proof
//!
//! 1. **Provider** — the gateway's public URL (pre-filled from the request, so
//!    it is already right in the common case) and the OIDC provider's issuer,
//!    client id and secret. Submitting does not save anything: it starts a real
//!    authorization-code round trip against the provider just entered.
//! 2. **Administrator** — reached only by coming back from the provider with a
//!    verified ID token. The operator picks which claim, and which of its
//!    values, grants admin, choosing from the values their own token actually
//!    carried. Finishing writes everything and swaps the live OIDC client in,
//!    so `/login` works immediately with no restart.
//!
//! The round trip is the point. Probing `/.well-known/openid-configuration`
//! proves only that a URL answers; it does not prove the client secret is
//! right, that the redirect URI is whitelisted, or — the thing nobody can
//! guess — what the provider calls its groups claim and what the values in it
//! look like. So step 1's submit button *is* the test: there is no way to
//! reach step 2 without a login that genuinely worked.
//!
//! The probe deliberately reuses the production redirect URI
//! (`{public_url}/auth/callback`) rather than a wizard-specific path, so the
//! operator whitelists exactly one URI in their IdP and the thing being tested
//! is the thing that will run. `/auth/callback` tells the two apart by the
//! `purpose` column on `pending_logins` and routes a probe here instead of
//! minting a session.
//!
//! # Who may reach it
//!
//! See [`gateway_core::server::setup::SetupAccess`]. On a first run `/setup` is
//! open — there is no account to authenticate against yet, and nothing
//! configured worth stealing. Once setup has completed it is gone, and comes
//! back only for the 30 minutes after an operator runs `restore-setup` on the
//! host, gated by the one-time token that command prints. A recovery run does
//! **not** interrupt anyone: the gateway keeps serving while it is open.

use std::sync::Arc;

use plait::{ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{HeaderMap, Request, Response, StatusCode, header};

use session_core::chrome::{self, Theme, read_cookie, see_other};
use session_core::i18n::{Lang, t};

use gateway_core::rama_server::session::secure_cookies;

use gateway_core::server::auth::oidc::{self, OidcClient, OidcParams};
use gateway_core::server::auth::pending;
use gateway_core::server::db::gateway_groups;
use gateway_core::server::oidc_settings;
use gateway_core::server::setup::{self, Draft, Proof, SetupAccess};
use gateway_runtime::rama_server::state::RamaState;
use gateway_runtime::server::state::RuntimeSettings;

use super::{field, read_form};

/// Carries a verified recovery claim across the wizard's requests, so the
/// one-time token only has to be pasted once. Scoped to `/setup` and dropped
/// when setup completes.
const SETUP_CLAIM_COOKIE: &str = "gw_setup";

/// The gateway group the wizard creates for administrators. A name, not a
/// magic value — the admin can rename or replace it at `/admin/groups`
/// afterwards; what makes it privileged is its `is_admin` flag.
const ADMIN_GROUP: &str = "admins";
/// The group every other signed-in user falls into. Created with no grants on
/// purpose: who may use which tools, pools and skills is a decision for the
/// operator at `/admin/groups`, not something a wizard should presume.
const DEFAULT_GROUP: &str = "users";

/// Claims that are part of the OIDC protocol rather than a description of the
/// person, filtered out of the group-claim picker so the interesting values
/// are not buried. Anything else the provider sent is offered.
const PROTOCOL_CLAIMS: &[&str] = &[
    "iss",
    "aud",
    "exp",
    "iat",
    "nbf",
    "jti",
    "nonce",
    "at_hash",
    "c_hash",
    "s_hash",
    "auth_time",
    "azp",
    "sid",
    "typ",
    "acr",
    "amr",
];

// ---------------------------------------------------------------------------
// Access

/// Resolve whether this request may see the wizard at all.
///
/// On success the caller gets the verified recovery claim, if the request
/// carried one as a query parameter — only `GET /setup` ever does, and only it
/// needs to put the claim on a cookie. Every later request in the run presents
/// that cookie instead.
async fn gate(state: &RamaState, req: &Request) -> Result<Option<String>, Response> {
    let headers = req.headers();
    let access = match setup::access(&state.db).await {
        Ok(a) => a,
        Err(err) => {
            tracing::error!(error = %err, "reading setup state");
            return Err(problem(
                headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not read the gateway's setup state",
            ));
        }
    };
    match access {
        // Nothing configured: open. Deliberate — there is no account to
        // authenticate against, and a token nobody can retrieve without shell
        // access would just move the lockout one step earlier.
        SetupAccess::FirstRun => Ok(None),
        SetupAccess::Recovery => {
            let presented = req
                .uri()
                .query()
                .and_then(|q| serde_urlencoded::from_str::<ClaimQuery>(q).ok())
                .and_then(|q| q.claim)
                .or_else(|| read_cookie(req.headers(), SETUP_CLAIM_COOKIE));
            match presented {
                Some(token)
                    if setup::recovery_token_matches(&state.db, access, &token)
                        .await
                        .unwrap_or(false) =>
                {
                    Ok(Some(token))
                }
                _ => Err(problem(
                    headers,
                    StatusCode::FORBIDDEN,
                    "This gateway is already configured. Reopening setup needs the one-time \
                     link printed by `restore-setup` on the host.",
                )),
            }
        }
        // Configured and no window open: the wizard does not exist.
        SetupAccess::Closed => Err(problem(
            headers,
            StatusCode::NOT_FOUND,
            "This gateway is already configured. To reconfigure it, run `restore-setup` on \
             the host — it prints a one-time link.",
        )),
    }
}

#[derive(serde::Deserialize)]
struct ClaimQuery {
    claim: Option<String>,
}

/// Attach the recovery claim to a response so the rest of the wizard works
/// without repeating the token in every form.
fn with_claim(state: &RamaState, resp: Response, claim: Option<&String>) -> Response {
    match claim {
        Some(token) => with_cookie(
            resp,
            &claim_cookie(token, secure_cookies(&state.public_url())),
        ),
        None => resp,
    }
}

/// Set/clear pair for the recovery claim, kept adjacent so the `Path` they
/// agree on cannot drift — the same reason `pending::binding_cookie` and its
/// clear-counterpart live side by side. A mismatched `Path` leaves a stale
/// cookie that then fails every later check.
fn claim_cookie(token: &str, secure: bool) -> String {
    // `Secure` matters more here than on the session cookie: this token
    // reconfigures a *live* gateway with real users. It dies with the browser
    // session and is worthless once the window closes, so no Max-Age.
    let secure = if secure { "; Secure" } else { "" };
    format!("{SETUP_CLAIM_COOKIE}={token}; Path=/setup; HttpOnly; SameSite=Lax{secure}")
}

fn clear_claim_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{SETUP_CLAIM_COOKIE}=; Path=/setup; HttpOnly; SameSite=Lax{secure}; Max-Age=0")
}

fn with_cookie(mut resp: Response, cookie: &str) -> Response {
    if let Ok(value) = header::HeaderValue::from_str(cookie) {
        resp.headers_mut().append(header::SET_COOKIE, value);
    }
    resp
}

// ---------------------------------------------------------------------------
// Screens

/// GET /setup — screen 1, or screen 2 once a test login has succeeded.
pub async fn setup_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let claim = match gate(&state, &req).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());

    let proof = setup::load_proof(&state.db, &state.crypto)
        .await
        .ok()
        .flatten();

    let page = match proof {
        Some(proof) => render_admin_screen(lang, theme, &proof),
        None => {
            // Only screen 1 needs the draft, so only screen 1 pays to unseal
            // and deserialise it.
            let draft = setup::load_draft(&state.db, &state.crypto)
                .await
                .ok()
                .flatten();
            // Pre-fill from the draft if the operator is coming back after a
            // failed attempt, otherwise from the request itself — which is
            // right whenever the browser reached the gateway by the URL
            // everyone else will use, i.e. almost always.
            let public_url = draft
                .as_ref()
                .map(|d| d.public_url.clone())
                .unwrap_or_else(|| public_url_from_request(&req));
            render_provider_screen(lang, theme, &public_url, draft.as_ref())
        }
    };
    with_claim(&state, page, claim.as_ref())
}

/// Guess the gateway's public base URL from the request that reached it.
///
/// `X-Forwarded-Proto` is honoured because the overwhelmingly common
/// deployment is behind a TLS-terminating reverse proxy, where the request the
/// gateway sees is plain HTTP while the URL the world uses is HTTPS. It is a
/// pre-filled suggestion in an editable field, not a trust decision — the
/// operator confirms it before anything is stored.
fn public_url_from_request(req: &Request) -> String {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8080");
    let scheme = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').next().unwrap_or(v).trim().to_owned())
        .filter(|v| v == "http" || v == "https")
        .unwrap_or_else(|| "http".into());
    format!("{scheme}://{host}")
}

// ---------------------------------------------------------------------------
// Rendering helpers
//
// The wizard runs before any of the usual chrome exists — no session, no nav,
// possibly no configuration at all — so all three of its screens are the same
// standalone centred card. These two helpers hold that shape, and the labelled
// text field, in one place each rather than in three and six copies.

/// The page shell every wizard screen shares. Pins the title too, so the
/// browser tab reads the same on all of them.
fn shell(theme: Theme, lang: Lang, inner: plait::Html) -> Response {
    let body = html! {
        main(class: "min-h-dvh flex items-center justify-center p-6") {
            div(class: "card border border-base-300 w-full max-w-2xl") {
                div(class: "card-body gap-4") { (inner) }
            }
        }
    }
    .to_html();
    chrome::html_page(theme, lang, "/setup", "Setup — LLM Gateway", body)
}

/// A labelled text input, optionally with help text underneath.
struct TextField<'a> {
    name: &'a str,
    /// Already localised — `t()` returns an owned `String`.
    label: String,
    value: &'a str,
    help: Option<String>,
    placeholder: &'a str,
    kind: &'a str,
    required: bool,
}

/// Render a [`TextField`].
///
/// `required` gets two whole branches rather than a conditional attribute for
/// the reason `pk_name_input` in the parent module documents: plait renders
/// `required: (false)` as `required="false"`, which browsers honour as
/// required.
fn text_field(f: TextField<'_>) -> plait::Html {
    let (name, label, value) = (f.name.to_string(), f.label, f.value.to_string());
    let (kind, placeholder) = (f.kind.to_string(), f.placeholder.to_string());
    let help = f.help;
    const INPUT: &str = "input input-bordered w-full";
    let input = if f.required {
        html! {
            input(type: (kind), name: (name), value: (value), placeholder: (placeholder),
                  required: "required", autocomplete: "off", class: (INPUT));
        }
        .to_html()
    } else {
        html! {
            input(type: (kind), name: (name), value: (value), placeholder: (placeholder),
                  autocomplete: "off", class: (INPUT));
        }
        .to_html()
    };
    // `flex flex-col gap-1`, not daisyUI's `form-control` — that class was
    // removed in daisyUI 5, so it styles nothing and the label, input and help
    // text run together inline.
    html! {
        label(class: "flex flex-col gap-1") {
            span(class: "label-text font-medium") { (label) }
            (input)
            if let Some(help) = &help {
                span(class: "label-text-alt text-xs text-base-content/60") { (help) }
            }
        }
    }
    .to_html()
}

fn render_provider_screen(
    lang: Lang,
    theme: Theme,
    public_url: &str,
    draft: Option<&Draft>,
) -> Response {
    let redirect_uri = oidc::redirect_uri_for(public_url);
    let issuer = draft.map(|d| d.params.issuer.clone()).unwrap_or_default();
    let client_id = draft
        .map(|d| d.params.client_id.clone())
        .unwrap_or_default();
    let scopes = draft
        .map(|d| d.params.scopes.join(" "))
        .unwrap_or_else(|| oidc_settings::default_scopes().join(" "));
    let roles_claim = draft
        .and_then(|d| d.params.roles_claim.clone())
        .unwrap_or_else(|| "groups".into());

    // Localise here so the call sites below read as a table of field
    // definitions rather than six copies of the same markup.
    let field = |name: &str,
                 label_key: &str,
                 value: &str,
                 help_key: Option<&str>,
                 placeholder: &str,
                 kind: &str,
                 required: bool| {
        text_field(TextField {
            name,
            label: t(lang, label_key),
            value,
            help: help_key.map(|k| t(lang, k)),
            placeholder,
            kind,
            required,
        })
    };
    let inner = html! {
        div {
            p(class: "text-xs uppercase tracking-wide text-base-content/50") {
                (t(lang, "setup-step-1-of-2"))
            }
            h1(class: "card-title text-2xl") { (t(lang, "setup-provider-heading")) }
            p(class: "text-base-content/70 mt-1") { (t(lang, "setup-provider-intro")) }
        }

        form(action: "/setup/test", method: "post", class: "flex flex-col gap-4") {
            (field("public_url", "setup-field-public-url", public_url,
                   Some("setup-field-public-url-help"), "", "url", true))

            div(class: "alert alert-info text-sm") {
                div {
                    p(class: "font-medium") { (t(lang, "setup-redirect-uri-heading")) }
                    code(class: "break-all") { (redirect_uri) }
                    p(class: "mt-1 opacity-80") { (t(lang, "setup-redirect-uri-help")) }
                }
            }

            (field("issuer", "setup-field-issuer", &issuer, Some("setup-field-issuer-help"),
                   "https://id.example.com/realms/company", "url", true))

            div(class: "grid gap-4 sm:grid-cols-2") {
                (field("client_id", "setup-field-client-id", &client_id, None, "", "text", true))
                // Never echoed back: a stored secret is write-only from the
                // moment it is saved, so the field starts empty on a retry.
                (field("client_secret", "setup-field-client-secret", "", None, "", "password", true))
            }

            div(class: "grid gap-4 sm:grid-cols-2") {
                (field("scopes", "setup-field-scopes", &scopes, Some("setup-field-scopes-help"),
                       "", "text", false))
                (field("roles_claim", "setup-field-roles-claim", &roles_claim,
                       Some("setup-field-roles-claim-help"), "", "text", false))
            }

            button(type: "submit", class: "btn btn-primary btn-block mt-2") {
                (t(lang, "setup-test-button"))
            }
            p(class: "text-center text-xs text-base-content/60") {
                (t(lang, "setup-test-button-help"))
            }
        }
    }
    .to_html();
    shell(theme, lang, inner)
}

/// One offered (claim, value) pair from the proven ID token.
struct ClaimChoice {
    claim: String,
    value: String,
}

/// Every string-ish claim value in the token, minus protocol plumbing. These
/// become the radio list the operator picks their admin group from — the
/// entire reason the wizard insists on a real login first.
fn claim_choices(claims: &serde_json::Value) -> Vec<ClaimChoice> {
    let Some(obj) = claims.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, value) in obj {
        if PROTOCOL_CLAIMS.contains(&key.as_str()) {
            continue;
        }
        match value {
            serde_json::Value::String(s) if !s.is_empty() => out.push(ClaimChoice {
                claim: key.clone(),
                value: s.clone(),
            }),
            serde_json::Value::Array(items) => {
                for item in items {
                    if let Some(s) = item.as_str().filter(|s| !s.is_empty()) {
                        out.push(ClaimChoice {
                            claim: key.clone(),
                            value: s.to_owned(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    out.sort_by(|a, b| a.claim.cmp(&b.claim).then_with(|| a.value.cmp(&b.value)));
    out
}

fn render_admin_screen(lang: Lang, theme: Theme, proof: &Proof) -> Response {
    let choices = claim_choices(&proof.claims);
    let who = if proof.email.is_empty() {
        proof.subject.clone()
    } else {
        proof.email.clone()
    };
    let has_choices = !choices.is_empty();

    let manual = |name: &str, label_key: &str, placeholder: &str| {
        text_field(TextField {
            name,
            label: t(lang, label_key),
            value: "",
            help: None,
            placeholder,
            kind: "text",
            required: false,
        })
    };
    let inner = html! {
        div {
            p(class: "text-xs uppercase tracking-wide text-base-content/50") {
                (t(lang, "setup-step-2-of-2"))
            }
            h1(class: "card-title text-2xl") { (t(lang, "setup-admin-heading")) }
        }

        div(class: "alert alert-success text-sm") {
            div {
                p(class: "font-medium") { (t(lang, "setup-login-worked")) }
                p { (who) }
            }
        }

        p(class: "text-base-content/70") { (t(lang, "setup-admin-intro")) }

        form(action: "/setup/finish", method: "post", class: "flex flex-col gap-4") {
            if has_choices {
                div(class: "flex flex-col gap-2 max-h-80 overflow-y-auto rounded-box border border-base-300 p-2") {
                    for (index, choice) in choices.iter().enumerate() {
                        (render_choice(choice, index == 0))
                    }
                }
            } else {
                div(class: "alert alert-warning text-sm") { (t(lang, "setup-no-claims")) }
            }

            div(class: "divider text-xs") { (t(lang, "setup-or-manual")) }

            div(class: "grid gap-4 sm:grid-cols-2") {
                (manual("manual_claim", "setup-manual-claim", "groups"))
                (manual("manual_value", "setup-manual-value", "platform-admins"))
            }
            p(class: "text-xs text-base-content/60 -mt-2") { (t(lang, "setup-manual-help")) }

            button(type: "submit", class: "btn btn-primary btn-block mt-2") {
                (t(lang, "setup-finish-button"))
            }
        }

        form(action: "/setup/restart", method: "post") {
            button(type: "submit", class: "btn btn-ghost btn-sm btn-block") {
                (t(lang, "setup-back-button"))
            }
        }

        details(class: "text-xs") {
            summary(class: "cursor-pointer text-base-content/60") {
                (t(lang, "setup-show-token"))
            }
            pre(class: "mt-2 overflow-x-auto rounded-box bg-base-200 p-3") {
                (serde_json::to_string_pretty(&proof.claims).unwrap_or_default())
            }
        }
    }
    .to_html();
    shell(theme, lang, inner)
}

/// One radio row.
///
/// Two whole `html!` branches rather than a conditional attribute: plait
/// renders `checked: (false)` as `checked="false"`, which browsers still treat
/// as checked. Same reason — and same shape — as `select_option` and
/// `bool_checkbox` in the parent module.
fn render_choice(choice: &ClaimChoice, checked: bool) -> plait::Html {
    let value = format!("{}{PAIR_SEP}{}", choice.claim, choice.value);
    let claim = choice.claim.clone();
    let val = choice.value.clone();
    const ROW: &str =
        "flex items-center gap-3 rounded-btn px-3 py-2 hover:bg-base-200 cursor-pointer";
    if checked {
        html! {
            label(class: (ROW)) {
                input(type: "radio", name: "pair", value: (value), checked: "checked", class: "radio radio-sm");
                span(class: "flex-1 text-sm") {
                    code(class: "text-base-content/60") { (claim) } " = " strong { (val) }
                }
            }
        }
        .to_html()
    } else {
        html! {
            label(class: (ROW)) {
                input(type: "radio", name: "pair", value: (value), class: "radio radio-sm");
                span(class: "flex-1 text-sm") {
                    code(class: "text-base-content/60") { (claim) } " = " strong { (val) }
                }
            }
        }
        .to_html()
    }
}

/// Delimiter packing a (claim, value) pair into one radio value.
///
/// U+001F (unit separator) because a claim name or a group value may
/// legitimately contain almost anything printable — colons and slashes are
/// common in provider group DNs — but not a C0 control character.
const PAIR_SEP: char = '\u{1f}';

fn decode_pair(encoded: &str) -> Option<(&str, &str)> {
    encoded
        .split_once(PAIR_SEP)
        .filter(|(c, v)| !c.is_empty() && !v.is_empty())
}

// ---------------------------------------------------------------------------
// Actions

/// POST /setup/test — stash what was typed, then start a real login against it.
///
/// Nothing is written to the live settings here. The draft is sealed and kept
/// only so `/auth/callback` can finish the exchange against the same provider,
/// and so a failed attempt comes back with the fields still filled in.
pub async fn setup_test(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = gate(&state, &req).await {
        return resp;
    }
    let (parts, body) = req.into_parts();
    let headers = &parts.headers;
    let form: Vec<(String, String)> = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };

    let public_url = field(&form, "public_url").trim().trim_end_matches('/');
    let issuer = field(&form, "issuer").trim();
    let client_id = field(&form, "client_id").trim();
    let client_secret = field(&form, "client_secret");
    if public_url.is_empty()
        || issuer.is_empty()
        || client_id.is_empty()
        || client_secret.is_empty()
    {
        return problem(
            headers,
            StatusCode::BAD_REQUEST,
            "Public URL, issuer, client id and client secret are all required.",
        );
    }
    // Any proof on file belongs to the *previous* draft, and this request
    // replaces that draft. Leaving it would let `setup_index` render screen 2
    // with one provider's claims while the draft names another — and
    // `setup_finish` would then persist provider B with an admin value taken
    // from a token issued by provider A. Two tabs, or a back button, is all it
    // takes. The proof and the draft it proves have to move together.
    if let Err(err) = setup::clear_proof(&state.db).await {
        tracing::error!(error = %err, "clearing the previous setup proof");
        return problem(
            headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not clear the previous attempt",
        );
    }

    let roles_claim = field(&form, "roles_claim").trim();
    let draft = Draft {
        public_url: public_url.to_owned(),
        params: OidcParams {
            issuer: issuer.to_owned(),
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            scopes: oidc_settings::parse_scopes_or_default(field(&form, "scopes")),
            roles_claim: (!roles_claim.is_empty()).then(|| roles_claim.to_owned()),
        },
    };

    // Build a client against the draft. This is where a wrong issuer, an
    // unreachable provider or a discovery-document mismatch surfaces — with
    // the provider's own words, which are far more useful than "login failed"
    // would be two redirects later.
    let client = match OidcClient::build(&draft.params, &draft.public_url).await {
        Ok(c) => c,
        Err(err) => {
            // Keep the draft so the operator's typing survives the round trip.
            let _ = setup::save_draft(&state.db, &state.crypto, &draft).await;
            return problem(
                headers,
                StatusCode::BAD_GATEWAY,
                &format!("Could not reach that provider: {err}"),
            );
        }
    };

    if let Err(err) = setup::save_draft(&state.db, &state.crypto, &draft).await {
        tracing::error!(error = %err, "saving setup draft");
        return problem(
            headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not save the entered settings",
        );
    }

    let start = client.begin();
    if let Err(err) =
        pending::insert(&state.db, &start, Some("/setup"), pending::Purpose::Setup).await
    {
        tracing::error!(error = %err, "persisting setup probe");
        return problem(
            headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not start the test login",
        );
    }

    // Same 303 + browser-binding cookie the login path emits, so the callback's
    // anti-CSRF check treats a probe exactly like a real sign-in.
    pending::authorization_redirect(&start)
}

/// Called by `/auth/callback` when the in-flight row was a setup probe.
///
/// Completes the exchange against the *draft* provider — the live client is
/// normally absent at this point, which is precisely why the wizard exists —
/// and records the verified claims for screen 2. No user row, no session:
/// nothing has authorised anyone yet.
pub async fn setup_probe_callback(
    state: &RamaState,
    headers: &HeaderMap,
    code: &str,
    verifier: &str,
    nonce: &str,
) -> Response {
    // A `pending_logins` row lives 15 minutes, but the wizard can close inside
    // that span — the recovery window expires, or another tab finishes setup.
    // Without this check a probe started while it was open could still land
    // afterwards and write a real person's full ID-token claims into a settings
    // row on a gateway that is no longer accepting setup at all.
    if setup::access(&state.db)
        .await
        .unwrap_or(SetupAccess::Closed)
        == SetupAccess::Closed
    {
        return problem(
            headers,
            StatusCode::NOT_FOUND,
            "Setup is no longer open on this gateway, so this test sign-in was discarded.",
        );
    }
    let Ok(Some(draft)) = setup::load_draft(&state.db, &state.crypto).await else {
        return problem(
            headers,
            StatusCode::BAD_REQUEST,
            "The setup attempt this login belongs to is gone. Start again at /setup.",
        );
    };
    let client = match OidcClient::build(&draft.params, &draft.public_url).await {
        Ok(c) => c,
        Err(err) => {
            return problem(
                headers,
                StatusCode::BAD_GATEWAY,
                &format!("Could not reach that provider: {err}"),
            );
        }
    };
    let claims = match client.complete(code, verifier, nonce).await {
        Ok(c) => c,
        Err(err) => {
            // The most common causes are a wrong client secret and a redirect
            // URI the provider does not know. Both are fixable on screen 1, so
            // send the operator back with the message intact.
            return problem(
                headers,
                StatusCode::BAD_GATEWAY,
                &format!(
                    "The provider accepted the sign-in but the gateway could not complete it: \
                     {err}. Check the client secret, and that {} is whitelisted as a redirect \
                     URI.",
                    oidc::redirect_uri_for(&draft.public_url)
                ),
            );
        }
    };

    let proof = Proof {
        subject: claims.subject,
        email: claims.email,
        name: claims.name,
        claims: claims.raw,
    };
    if let Err(err) = setup::save_proof(&state.db, &state.crypto, &proof).await {
        tracing::error!(error = %err, "saving setup proof");
        return problem(
            headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not record the successful test login",
        );
    }
    see_other("/setup")
}

/// POST /setup/restart — throw away the proof and go back to screen 1.
pub async fn setup_restart(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = gate(&state, &req).await {
        return resp;
    }
    if let Err(err) = setup::clear_proof(&state.db).await {
        tracing::warn!(error = %err, "clearing setup proof");
    }
    see_other("/setup")
}

/// POST /setup/finish — promote the draft to live settings and open for
/// business.
pub async fn setup_finish(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = gate(&state, &req).await {
        return resp;
    }
    let (parts, body) = req.into_parts();
    let headers = &parts.headers;
    let form: Vec<(String, String)> = match read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };

    let (Ok(Some(draft)), Ok(Some(_proof))) = (
        setup::load_draft(&state.db, &state.crypto).await,
        setup::load_proof(&state.db, &state.crypto).await,
    ) else {
        return problem(
            headers,
            StatusCode::BAD_REQUEST,
            "This setup run has expired. Start again at /setup.",
        );
    };

    // A manually typed pair wins over the radio selection: the operator went
    // out of their way to type it.
    let manual_claim = field(&form, "manual_claim").trim();
    let manual_value = field(&form, "manual_value").trim();
    let (admin_claim, admin_value) = if !manual_claim.is_empty() && !manual_value.is_empty() {
        (manual_claim.to_owned(), manual_value.to_owned())
    } else {
        match decode_pair(field(&form, "pair")) {
            Some((c, v)) => (c.to_owned(), v.to_owned()),
            None => {
                return problem(
                    headers,
                    StatusCode::BAD_REQUEST,
                    "Pick which claim value should grant administrator access, or type one in.",
                );
            }
        }
    };

    // The chosen claim IS the roles claim — the gateway reads group membership
    // from exactly one claim, and picking a value from `email` while resolving
    // groups from `groups` would produce an admin nobody can be.
    let params = OidcParams {
        roles_claim: Some(admin_claim.clone()),
        ..draft.params.clone()
    };

    // Build the live client before writing anything: if this fails there is no
    // point marking the gateway configured.
    let client = match OidcClient::build(&params, &draft.public_url).await {
        Ok(c) => c,
        Err(err) => {
            return problem(
                headers,
                StatusCode::BAD_GATEWAY,
                &format!("Could not reach that provider: {err}"),
            );
        }
    };

    if let Err(err) = persist(&state, &draft, &params, &admin_value).await {
        tracing::error!(error = %err, "persisting setup");
        return problem(
            headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not save the configuration",
        );
    }

    // Swap the live settings in. This is what makes `/login` work in the very
    // next request instead of after a restart the operator would have to reach
    // a shell to perform.
    state.set_runtime(RuntimeSettings {
        public_url: draft.public_url.clone(),
        oidc: Some(client),
        setup_completed: true,
    });
    state.reload_rbac().await;

    tracing::info!(
        public_url = %draft.public_url,
        issuer = %draft.params.issuer,
        admin_claim = %admin_claim,
        "setup completed; OIDC is live"
    );

    // Sign in, then land on `/admin/settings` rather than the chat surface.
    // Setup proved a provider and made somebody admin; it did not make the
    // gateway *useful*, and the operator's next job is the settings and
    // backends the wizard deliberately does not ask about. Carrying it as
    // `return_to` reuses the ordinary deep-link path — `/login` forwards it,
    // `/auth/login` persists it on the `pending_logins` row, and the callback
    // honours it — so there is no new state and no second way to land somewhere.
    //
    // If the admin claim was mistyped this ends on a 403, which is the right
    // answer: better than a silent non-admin session that looks like success.
    let landing = format!(
        "/login?return_to={}",
        urlencoding_encode(POST_SETUP_LANDING)
    );
    // Drop the recovery cookie on the way out — the window is closed now.
    // Uses the URL just installed, so the flag matches the cookie that was set.
    with_cookie(
        see_other(&landing),
        &clear_claim_cookie(secure_cookies(&state.public_url())),
    )
}

/// Where a freshly configured gateway sends its operator after they sign in.
///
/// `/admin/settings` rather than `/admin/upstreams`, even though the latter is
/// the more urgent job: settings is where the blocks a config file used to hold
/// now live, so an operator arriving from an upgrade finds them, and the page
/// itself points at upstreams when no pool exists yet.
const POST_SETUP_LANDING: &str = "/admin/settings";

/// Percent-encode a path for use in a query string. Hand-rolled because the
/// only thing that ever goes through it is [`POST_SETUP_LANDING`] — a constant
/// with two slashes in it — and pulling in a dependency to encode two
/// characters would be worse than the six lines.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Write everything the wizard decided. Ordered so that a failure part-way
/// through leaves the gateway *un*configured (still in setup mode) rather than
/// half-configured: `mark_completed` is last.
async fn persist(
    state: &RamaState,
    draft: &Draft,
    params: &OidcParams,
    admin_value: &str,
) -> Result<(), gateway_core::server::db::DbError> {
    setup::set_public_url(&state.db, &draft.public_url).await?;
    oidc_settings::set_params(&state.db, &state.crypto, params).await?;

    gateway_groups::upsert_group(
        &state.db,
        ADMIN_GROUP,
        "Full administrative access. Created by the setup wizard.",
        true,
        false,
    )
    .await?;
    gateway_groups::set_mappings_for_group(&state.db, ADMIN_GROUP, &[admin_value.to_owned()])
        .await?;
    gateway_groups::set_tools_for_group(&state.db, ADMIN_GROUP, &["*".to_owned()]).await?;

    // Everyone else lands here. No grants on purpose — see DEFAULT_GROUP.
    gateway_groups::upsert_group(
        &state.db,
        DEFAULT_GROUP,
        "Everyone who signs in. Grant it tools and pools at /admin/groups.",
        false,
        true,
    )
    .await?;

    setup::mark_completed(&state.db).await
}

// ---------------------------------------------------------------------------

/// A standalone message page. The wizard runs before any of the usual chrome
/// (no session, no nav, possibly no configuration at all), so its errors are
/// deliberately plain and self-contained.
///
/// The text is English regardless of `lang`: these messages quote the identity
/// provider's own error verbatim, which is untranslated anyway, and every one
/// of them is a deployment fault an operator will paste into a search box or a
/// support ticket. A half-translated diagnostic helps nobody.
fn problem(headers: &HeaderMap, status: StatusCode, message: &str) -> Response {
    let message = message.to_owned();
    // `shell` already supplies the <main> and the card — this is only what goes
    // inside the card body.
    let inner = html! {
        h1(class: "card-title") { "Setup" }
        p(class: "text-base-content/80") { (message) }
        a(href: "/setup", class: "btn btn-sm btn-outline self-start") { "Back to setup" }
    }
    .to_html();
    let mut resp = shell(Theme::from_headers(headers), Lang::En, inner);
    *resp.status_mut() = status;
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claim_choices_offer_strings_and_arrays_but_not_protocol_plumbing() {
        let claims = json!({
            "iss": "https://id.example.com",
            "aud": "gateway",
            "exp": 1_700_000_000u64,
            "sub": "alice-sub",
            "email": "alice@example.com",
            "groups": ["platform-admins", "engineering"],
            "department": "R&D",
            "email_verified": true,
        });
        let pairs: Vec<(String, String)> = claim_choices(&claims)
            .into_iter()
            .map(|c| (c.claim, c.value))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("department".into(), "R&D".into()),
                ("email".into(), "alice@example.com".into()),
                ("groups".into(), "engineering".into()),
                ("groups".into(), "platform-admins".into()),
                ("sub".into(), "alice-sub".into()),
            ]
        );
    }

    #[test]
    fn claim_choices_survive_a_token_with_nothing_pickable() {
        // A provider that sends no group-ish claim at all must not panic the
        // screen — the operator falls back to typing a value by hand.
        assert!(claim_choices(&json!({"iss": "x", "exp": 1})).is_empty());
        assert!(claim_choices(&json!("not an object")).is_empty());
    }

    #[test]
    fn public_url_prefers_the_forwarded_scheme() {
        let req = Request::builder()
            .header("host", "gw.example.com")
            .header("x-forwarded-proto", "https")
            .body(rama::http::Body::empty())
            .unwrap();
        assert_eq!(public_url_from_request(&req), "https://gw.example.com");
    }

    #[test]
    fn public_url_ignores_a_nonsense_forwarded_scheme() {
        // The header is attacker-controllable in some topologies; anything
        // that is not http/https falls back rather than landing in the field.
        let req = Request::builder()
            .header("host", "gw.example.com")
            .header("x-forwarded-proto", "javascript:")
            .body(rama::http::Body::empty())
            .unwrap();
        assert_eq!(public_url_from_request(&req), "http://gw.example.com");
    }

    #[test]
    fn public_url_takes_the_first_hop_of_a_forwarded_chain() {
        let req = Request::builder()
            .header("host", "gw.example.com")
            .header("x-forwarded-proto", "https, http")
            .body(rama::http::Body::empty())
            .unwrap();
        assert_eq!(public_url_from_request(&req), "https://gw.example.com");
    }
}
