// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/settings` — the editor for the operator settings that used to live
//! in `gateway.toml`.
//!
//! One page for twelve blocks, rendered from
//! [`gateway_core::server::settings::SECTIONS`] rather than hand-built per
//! block. That is what keeps this file from being a thousand lines of nearly
//! identical form markup, and it means adding a setting is one entry in that
//! table — the control, the parsing, the save and the drift test all follow.
//!
//! # Labels, and the identifier under them
//!
//! Every card title, field label and line of help text is localised in all six
//! languages, keyed off the spec entry (`settings-f-sandbox-runner_url`,
//! `settings-f-sandbox-runner_url-help`) so the Rust table holds no prose to
//! drift out of sync with the locale files.
//!
//! Under each control the TOML path itself (`sandbox.runner_url`) is still
//! printed, verbatim and untranslated, ahead of the help text. An operator
//! working here is matching what they see against `gateway.example.toml`,
//! `docs/`, a log line or a support thread, all of which say `runner_url`, and
//! a translated label alone would cut the only thread connecting them. Both,
//! rather than either: the label says what it does, the identifier says what
//! to grep for.
//!
//! # Secrets
//!
//! A [`Kind::Secret`] field renders as "set" or "not set" and an empty box. An
//! empty submission means "leave it alone", because the page never had the
//! value to submit back; clearing one is an explicit button. See
//! [`gateway_core::server::settings::store`].

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};

use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_admin_or_403};
use session_core::chrome::{
    Flash, FlashKind, Theme, is_datastar_request, read_body_to_bytes, sse_patch, sse_response,
    sse_toast, sse_toast_response,
};
use session_core::i18n::{self as i18n, Lang, t, t_args};
use session_core::icons;

use gateway_core::server::config::Config;
use gateway_core::server::settings::{self, Category, FieldSpec, Kind, SECTIONS, Settings, Span};
use gateway_core::server::upstreams::UpstreamRegistry;
use gateway_runtime::rama_server::state::RamaState;

/// Days → `Duration`, clamped the same way the boot path clamps it so a
/// hand-edited row cannot produce a zero-length or absurd session lifetime.
fn session_days(days: i64) -> std::time::Duration {
    std::time::Duration::from_secs(days.clamp(1, 400) as u64 * 24 * 60 * 60)
}

#[derive(serde::Deserialize)]
struct TabQuery {
    tab: Option<String>,
}

/// GET /admin/settings
pub async fn settings_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = session_core::chrome::NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    // The wizard sends a freshly configured gateway here, and this page cannot
    // make it answer a single chat request — that needs a backend. Say so, at
    // the top, while it is true.
    let needs_a_backend = state.upstreams.all_models().is_empty();

    // `?tab=` decides which category is shown; anything unrecognised (a stale
    // bookmark, a hand-typed URL) falls back to the first tab rather than
    // rendering a page with no cards on it.
    let tab = req
        .uri()
        .query()
        .and_then(|q| serde_urlencoded::from_str::<TabQuery>(q).ok())
        .and_then(|q| q.tab)
        .and_then(|slug| Category::from_slug(&slug))
        .unwrap_or(Category::ALL[0]);

    // Display the values *in force*, not the raw rows: a field with no row
    // falls back to its built-in default, and the editor has to show that
    // rather than an empty box or an off toggle. `effective` also refuses to
    // hand out secret values, so no control can leak one.
    let config = state.config();
    let shown = settings::effective(&config);
    // Survives the page being closed, so whoever restarts the container still
    // sees which change is waiting on them.
    let restart_pending = settings::restart_pending(&state.db)
        .await
        .unwrap_or_default();
    let body = render_body(
        lang,
        &shown,
        needs_a_backend,
        tab,
        &state.upstreams,
        &config,
        &restart_pending,
    );
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    let title = t(lang, "settings-heading");
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
        NavItem::Settings,
        &title,
        body,
        "/admin/settings",
        &chat,
    )
}

/// POST /admin/settings — save one section.
///
/// One section at a time, not the whole page. Twelve blocks in a single
/// submit would mean an operator adjusting an OCR limit also rewrites every
/// other row from whatever the form happened to contain, and a stale tab would
/// silently revert changes made elsewhere in the meantime.
pub async fn settings_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let bytes = match read_body_to_bytes(req.into_body()).await {
        Ok(b) => b,
        Err(_) => return sse_toast_response(FlashKind::Error, t(lang, "settings-save-failed")),
    };
    let form: Vec<(String, String)> = serde_urlencoded::from_bytes(&bytes).unwrap_or_default();

    let Some(section) = form
        .iter()
        .find(|(k, _)| k == "section")
        .map(|(_, v)| v.as_str())
        .and_then(|name| SECTIONS.iter().find(|s| s.name == name))
    else {
        return sse_toast_response(FlashKind::Error, t(lang, "settings-save-failed"));
    };

    // Only this section's fields, and only ones the table declares — a form
    // value naming anything else is a stale tab or a hand-built request.
    let mut pairs: Vec<(String, String)> = Vec::new();
    for field in section.fields {
        let submitted = form
            .iter()
            .find(|(k, _)| k == field.key)
            .map(|(_, v)| v.trim().to_owned());
        match field.kind {
            // An unchecked checkbox submits nothing at all, so absence is the
            // value here rather than "leave it alone".
            Kind::Bool => pairs.push((
                field.key.to_owned(),
                submitted
                    .is_some_and(|v| v == "on" || v == "true")
                    .to_string(),
            )),
            // A blank secret box means "keep what is stored"; `store` skips it.
            Kind::Secret => {
                if let Some(v) = submitted.filter(|v| !v.is_empty()) {
                    pairs.push((field.key.to_owned(), v));
                }
            }
            Kind::List => {
                let items: Vec<String> = submitted
                    .unwrap_or_default()
                    .split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
                    .collect();
                pairs.push((
                    field.key.to_owned(),
                    serde_json::to_string(&items).unwrap_or_else(|_| "[]".into()),
                ));
            }
            _ => pairs.push((field.key.to_owned(), submitted.unwrap_or_default())),
        }
    }

    if let Err(err) = settings::store(&state.db, &state.crypto, &pairs).await {
        tracing::error!(error = %err, section = section.name, "saving settings");
        return sse_toast_response(FlashKind::Error, t(lang, "settings-save-failed"));
    }
    // A human has now chosen these values, so the config file stops being
    // authoritative for them — otherwise a file appearing on a later boot
    // (a bind-mount restored, a migration finished) could import over this.
    // Idempotent; already true on any deployment that imported at boot.
    if let Err(err) = settings::mark_imported(&state.db).await {
        tracing::warn!(error = %err, "recording that settings are operator-owned");
    }

    // Swap the new values in, so everything that reads them per request picks
    // them up on the very next one. Fields marked `restart` are the exception,
    // and the page says so beside them.
    state.reload_settings().await;
    // The session policy lives on the store, not in the config snapshot, and
    // `reload_settings` cannot reach it (the store hangs off the request state,
    // not off `AppState`). Pushing it here is what makes the two
    // `gateway.session_*` fields take effect without a restart.
    let config = state.config();
    state.sessions.set_policy(
        session_days(config.gateway.session_ttl_days),
        session_days(config.gateway.session_absolute_max_days),
    );
    tracing::info!(section = section.name, "operator settings saved");

    // Which of *this* section's restart-flagged fields the operator just
    // changed. Recorded in the database, not just toasted: the person who
    // saves and the person who restarts the container are often not the same,
    // and a toast is gone in three seconds.
    let changed_restart_fields: Vec<String> = section
        .fields
        .iter()
        .filter(|f| f.restart)
        .map(|f| f.key.to_owned())
        .collect();
    let needs_restart = !changed_restart_fields.is_empty();
    if needs_restart {
        let mut pending = settings::restart_pending(&state.db)
            .await
            .unwrap_or_default();
        for key in changed_restart_fields {
            if !pending.contains(&key) {
                pending.push(key);
            }
        }
        if let Err(err) = settings::mark_restart_pending(&state.db, &pending).await {
            tracing::warn!(error = %err, "recording the pending restart");
        }
    }
    let message = if needs_restart {
        t(lang, "settings-saved-restart")
    } else {
        t(lang, "settings-saved")
    };

    // Toast *and* re-render the card, the way every sibling save does. Without
    // the patch the card keeps showing pre-save state: a secret that now reads
    // "stored" still says "not set", its Clear button is still missing, and a
    // feature just switched on still has its fields folded away.
    let config = state.config();
    let card = render_section(
        lang,
        section,
        &settings::effective(&config),
        &state.upstreams,
        &config,
    )
    .to_string();
    sse_response(&[
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message,
        }),
        sse_patch(Some(&format!("#{}", section_dom_id(section))), None, &card),
    ])
}

/// POST /admin/settings/clear — delete one field's row so it falls back to its
/// built-in default. The only way to remove a stored secret, which the editor
/// cannot do by submitting an empty box.
pub async fn settings_clear(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let bytes = match read_body_to_bytes(req.into_body()).await {
        Ok(b) => b,
        Err(_) => return sse_toast_response(FlashKind::Error, t(lang, "settings-save-failed")),
    };
    let form: Vec<(String, String)> = serde_urlencoded::from_bytes(&bytes).unwrap_or_default();
    let Some(key) = form
        .iter()
        .find(|(k, _)| k == "key")
        .map(|(_, v)| v.as_str())
        .filter(|k| settings::field(k).is_some())
    else {
        return sse_toast_response(FlashKind::Error, t(lang, "settings-save-failed"));
    };

    if let Err(err) = settings::clear(&state.db, key).await {
        tracing::error!(error = %err, %key, "clearing setting");
        return sse_toast_response(FlashKind::Error, t(lang, "settings-save-failed"));
    }
    state.reload_settings().await;
    tracing::info!(%key, "operator setting cleared");
    sse_toast_response(FlashKind::Success, t(lang, "settings-cleared"))
}

// ---------------------------------------------------------------------------
// Rendering

fn render_body(
    lang: Lang,
    stored: &Settings,
    needs_a_backend: bool,
    tab: Category,
    upstreams: &UpstreamRegistry,
    config: &Config,
    restart_pending: &[String],
) -> Html {
    let notice = if needs_a_backend {
        no_backend_notice(lang)
    } else {
        html! {}.to_html()
    };
    // The page shell every other admin page uses, verbatim: centred, padded,
    // with the extra top padding that clears the mobile header. Hand-rolling a
    // different container here is what made this page look unlike its siblings.
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6 flex flex-col gap-4") {
            div {
                h1(class: "text-2xl font-bold mb-2") { (t(lang, "settings-heading")) }
                p(class: "text-base-content/60 text-sm") { (t(lang, "settings-intro")) }
            }
            (notice)
            (restart_banner(lang, restart_pending))
            // Rail beside the cards on a wide screen, stacked above them on a
            // narrow one — the tab list is five short labels, so it needs no
            // separate mobile control.
            div(class: "flex flex-col sm:flex-row gap-4 items-start") {
                (render_rail(lang, tab, config))
                div(class: "flex flex-col gap-4 min-w-0 grow") {
                    for section in tab.sections() {
                        (render_section(lang, section, stored, upstreams, config))
                    }
                }
            }
        }
    }
    .to_html()
}

/// The category rail. Plain links carrying `?tab=`, not client-side state:
/// a tab worth navigating to is worth being able to bookmark, reload and send
/// to a colleague, and it keeps the page working with JavaScript off.
fn render_rail(lang: Lang, current: Category, config: &Config) -> Html {
    let items: Vec<Html> = Category::ALL
        .iter()
        .map(|c| rail_item(lang, *c, *c == current, config))
        .collect();
    html! {
        nav(
            class: "menu menu-sm bg-base-200 rounded-box w-full sm:w-52 shrink-0",
            "aria-label": (t(lang, "settings-heading")),
        ) {
            for item in items.iter() { (item.clone()) }
        }
    }
    .to_html()
}

fn rail_item(lang: Lang, category: Category, active: bool, config: &Config) -> Html {
    // "2/4 on" per tab, so an operator scanning the rail can see where anything
    // is switched on without opening each one.
    let sections: Vec<&settings::SectionSpec> = category.sections().collect();
    let switchable = sections
        .iter()
        .filter(|s| settings::section_is_enabled(config, s).is_some())
        .count();
    let on = sections
        .iter()
        .filter(|s| settings::section_is_enabled(config, s) == Some(true))
        .count();
    let badge = if switchable > 0 {
        format!("{on}/{switchable}")
    } else {
        String::new()
    };
    let href = format!("/admin/settings?tab={}", category.slug());
    let label = t(lang, category.i18n_key());
    if active {
        html! {
            li {
                a(href: (href), class: "menu-active flex justify-between gap-2") {
                    span { (label) }
                    span(class: "badge badge-xs badge-ghost") { (badge) }
                }
            }
        }
        .to_html()
    } else {
        html! {
            li {
                a(href: (href), class: "flex justify-between gap-2") {
                    span { (label) }
                    span(class: "badge badge-xs badge-ghost") { (badge) }
                }
            }
        }
        .to_html()
    }
}

/// Shown until the process comes back, listing the restart-flagged fields that
/// were changed. A banner rather than only a toast: the operator who saves and
/// the operator who restarts are often not the same person.
fn restart_banner(lang: Lang, pending: &[String]) -> Html {
    if pending.is_empty() {
        return html! {}.to_html();
    }
    let fields = pending.join(", ");
    html! {
        div(class: "alert alert-warning") {
            div(class: "flex flex-col gap-1") {
                span(class: "font-medium") { (t(lang, "settings-restart-pending-heading")) }
                span(class: "text-sm") { (t(lang, "settings-restart-pending-body")) }
                code(class: "text-xs") { (fields) }
            }
        }
    }
    .to_html()
}

/// Shown while no backend serves any model — i.e. right after the setup wizard,
/// which is what sends the operator to this page. Nothing on this page fixes
/// that, so it links to the page that does.
fn no_backend_notice(lang: Lang) -> Html {
    html! {
        div(class: "alert alert-warning") {
            div(class: "flex flex-col gap-1") {
                span(class: "font-medium") { (t(lang, "settings-no-backend-heading")) }
                span(class: "text-sm") { (t(lang, "settings-no-backend-body")) }
                a(href: "/admin/upstreams", class: "link link-neutral text-sm self-start") {
                    (t(lang, "settings-no-backend-cta"))
                }
            }
        }
    }
    .to_html()
}

/// The `id` of a section's card, used both as the form's id and as the target
/// [`sse_patch`] replaces after a save. Derived, so the two cannot drift.
fn section_dom_id(section: &settings::SectionSpec) -> String {
    format!("settings-{}", section.name.replace('.', "-"))
}

/// Whether a section is a feature that is currently switched off.
///
/// Asks the *effective* configuration, not the stored rows: a missing row means
/// the built-in default applies, and for compaction, usage, limits and push that
/// default is on. Reading the rows drew those toggles as off on a database that
/// had none — a control disagreeing with the running gateway. See
/// [`settings::section_is_enabled`].
fn is_switched_off(section: &settings::SectionSpec, config: &Config) -> bool {
    settings::section_is_enabled(config, section) == Some(false)
}

fn render_section(
    lang: Lang,
    section: &settings::SectionSpec,
    stored: &Settings,
    upstreams: &UpstreamRegistry,
    config: &Config,
) -> Html {
    let restart_note = section.fields.iter().any(|f| f.restart);
    html! {
        // `data-on:submit__prevent` (not `data-on-submit`) plus a real
        // `method`/`action`: the house pattern. Without `__prevent` the browser
        // runs its own submit alongside datastar's, so saving navigated away
        // instead of patching in place; with `action` set, a no-JS client still
        // posts to the right endpoint.
        form(
            id: (section_dom_id(section)),
            method: "post",
            action: "/admin/settings",
            class: "card border border-base-300",
            "data-on:submit__prevent": "@post('/admin/settings', {contentType: 'form'})",
        ) {
            input(type: "hidden", name: "section", value: (section.name));
            div(class: "card-body gap-3") {
                div {
                    h2(class: "card-title text-base") { (t(lang, &section.title_key())) }
                    p(class: "text-sm text-base-content/70") { (t(lang, &section.blurb_key())) }
                }
                (section_fields(lang, section, stored, upstreams, config))
                if restart_note {
                    p(class: "text-xs text-warning") { (t(lang, "settings-restart-note")) }
                }
                div(class: "card-actions justify-end") {
                    button(type: "submit", class: "btn btn-sm btn-primary") {
                        (t(lang, "settings-save"))
                    }
                }
            }
        }
    }
    .to_html()
}

/// A section's fields — all of them open, or, for a feature that is switched
/// off, just the toggle with the rest folded away.
///
/// A `<details>` element rather than a datastar signal: it is native, works
/// with JavaScript off, and inputs inside a *closed* `<details>` still submit,
/// so folding changes nothing about what a save sends. On a typical install
/// most features are off, and this is what stops their fields from dominating a
/// page the operator opened to change one thing.
fn section_fields(
    lang: Lang,
    section: &settings::SectionSpec,
    stored: &Settings,
    upstreams: &UpstreamRegistry,
    config: &Config,
) -> Html {
    // The block's own `enabled` toggle, when it has one, is the section's master
    // switch and gets its own row above the grid — both so it reads as governing
    // what follows, and so the layout does not change shape when a feature is
    // switched on.
    let has_master_switch = section
        .fields
        .first()
        .is_some_and(|f| f.key == format!("{}.enabled", section.name) && f.kind == Kind::Bool);
    let (master, rest) = if has_master_switch {
        (
            Some(render_field(lang, &section.fields[0], stored, upstreams)),
            &section.fields[1..],
        )
    } else {
        (None, section.fields)
    };
    let master = master.unwrap_or_else(|| html! {}.to_html());
    let controls: Vec<Html> = rest
        .iter()
        .map(|f| render_field(lang, f, stored, upstreams))
        .collect();
    let count = controls.len();

    if !is_switched_off(section, config) {
        return html! {
            div(class: "flex flex-col gap-3") {
                (master)
                div(class: "grid grid-cols-1 sm:grid-cols-2 gap-x-4 gap-y-3") {
                    for control in controls.iter() { (control.clone()) }
                }
            }
        }
        .to_html();
    }

    html! {
        div(class: "flex flex-col gap-3") {
            (master)
            details {
                summary(class: "text-sm link link-hover cursor-pointer") {
                    (t_args(lang, "settings-show-fields", &i18n::args([("count", (count as i64).into())])))
                }
                div(class: "grid grid-cols-1 sm:grid-cols-2 gap-x-4 gap-y-3 pt-3") {
                    for control in controls.iter() { (control.clone()) }
                }
            }
        }
    }
    .to_html()
}

fn render_field(
    lang: Lang,
    field: &FieldSpec,
    stored: &Settings,
    upstreams: &UpstreamRegistry,
) -> Html {
    let value = stored.shown(field.key).unwrap_or_default();
    let control = match field.kind {
        Kind::Bool => bool_control(field, value),
        Kind::Secret => secret_control(lang, field, stored),
        Kind::List => list_control(field, value),
        Kind::Int => text_control(field, value, "number", ""),
        Kind::Float => number_control(field, value),
        Kind::Path => text_control(field, value, "text", "/var/lib/gateway/…"),
        Kind::Text => text_control(field, value, "text", ""),
        Kind::Model(kind) => model_control(lang, field, value, &upstreams.models_for_kind(kind)),
    };
    // `sm:col-span-2` is what makes a full-width field take the whole row of
    // the two-column grid its section renders into; a half-width one occupies
    // one cell and pairs with its neighbour.
    let span_class = match field.span {
        Span::Full => "flex flex-col gap-1 sm:col-span-2",
        Span::Half => "flex flex-col gap-1",
    };
    // The TOML path shares the help line rather than getting one of its own:
    // three stacked lines of text per field turns a card of seven numbers back
    // into the page of scrolling that `Span::Half` exists to prevent.
    html! {
        div(class: (span_class)) {
            label(class: "flex items-baseline gap-2 flex-wrap", for: (field.key)) {
                span(class: "text-sm font-medium") { (t(lang, &field.label_key())) }
                if field.restart {
                    span(class: "badge badge-xs badge-warning") { (t(lang, "settings-restart-badge")) }
                }
            }
            (control)
            p(class: "text-xs text-base-content/60") {
                code(class: "text-base-content/45") { (field.key) }
                " · "
                (t(lang, &field.help_key()))
            }
        }
    }
    .to_html()
}

fn bool_control(field: &FieldSpec, value: &str) -> Html {
    // `checked` is emitted only when true. `attr:(bool)` would render
    // `checked="false"`, which every browser still treats as checked.
    let on = matches!(value.trim(), "true" | "1" | "yes" | "on");
    let base = html! {
        input(type: "checkbox", id: (field.key), name: (field.key), class: "toggle toggle-sm");
    }
    .to_html();
    if on {
        checked_checkbox(field.key)
    } else {
        base
    }
}

/// A checked checkbox, built separately because the `checked` attribute has to
/// be absent rather than `="false"` when off — see `pages::mod`'s
/// `bool_checkbox` for the same dodge, and note the macro moves its input into
/// a closure, so the two branches cannot share one `html!`.
fn checked_checkbox(key: &str) -> Html {
    html! {
        input(
            type: "checkbox", id: (key), name: (key),
            class: "toggle toggle-sm", checked: "checked",
        );
    }
    .to_html()
}

fn text_control(field: &FieldSpec, value: &str, kind: &str, placeholder: &str) -> Html {
    html! {
        input(
            type: (kind), id: (field.key), name: (field.key), value: (value),
            placeholder: (placeholder), class: "input input-sm input-bordered w-full",
        );
    }
    .to_html()
}

/// A [`Kind::List`] field, shown comma-separated.
///
/// The row itself holds JSON — that is what `settings::list()` parses and what
/// the TOML import writes — but the box must show what the save handler reads
/// back, which is a comma-separated line. Rendering the raw `["feedback"]` was
/// worse than ugly: saving the section without touching the field would split
/// that on commas and store a label literally named `["feedback"]`.
///
/// A row that is not valid JSON (hand-edited, or written by an older build) is
/// shown verbatim rather than blanked, so the operator can see and fix it.
fn list_control(field: &FieldSpec, value: &str) -> Html {
    let shown = match serde_json::from_str::<Vec<String>>(value) {
        Ok(items) => items.join(", "),
        Err(_) => value.to_owned(),
    };
    text_control(field, &shown, "text", "comma, separated, list")
}

fn number_control(field: &FieldSpec, value: &str) -> Html {
    html! {
        input(
            type: "number", step: "any", id: (field.key), name: (field.key), value: (value),
            class: "input input-sm input-bordered w-full",
        );
    }
    .to_html()
}

/// A model picker: the models actually configured for this pool kind, plus an
/// "automatic" choice that stores the empty value these fields already read as
/// "use the first available one".
///
/// Two cases make this more than a plain `<select>`:
///
/// * **No pool of that kind exists yet.** The list is empty, so the control
///   says so instead of rendering a lone blank option. An operator who has not
///   added an `ocr` pool cannot pick an OCR model, and the honest thing is to
///   tell them that rather than imply a choice.
/// * **A model is stored that is no longer configured** — a backend was
///   removed, or a pool renamed. Dropping it from the list would mean the next
///   save of this section silently rewrote the setting to "automatic". It is
///   kept, selected, and marked unavailable.
fn model_control(lang: Lang, field: &FieldSpec, value: &str, models: &[String]) -> Html {
    let value = value.trim();
    if models.is_empty() && value.is_empty() {
        return html! {
            p(class: "text-sm text-base-content/60 italic") {
                (t(lang, "settings-model-none-configured"))
            }
            input(type: "hidden", name: (field.key), value: "");
        }
        .to_html();
    }

    let mut options: Vec<Html> = Vec::with_capacity(models.len() + 2);
    options.push(model_option(
        "",
        &t(lang, "settings-model-automatic"),
        value.is_empty(),
    ));
    for m in models {
        options.push(model_option(m, m, m == value));
    }
    if !value.is_empty() && !models.iter().any(|m| m == value) {
        options.push(model_option(
            value,
            &t_args(
                lang,
                "settings-model-unavailable",
                &i18n::args([("model", value.to_string().into())]),
            ),
            true,
        ));
    }
    html! {
        select(id: (field.key), name: (field.key), class: "select select-sm select-bordered w-full") {
            for option in options.iter() { (option.clone()) }
        }
    }
    .to_html()
}

/// One `<option>`. `selected` is emitted only when true — `selected="false"` is
/// still selected as far as a browser is concerned (see `pages::mod`'s
/// `select_option` for the same trap).
fn model_option(value: &str, label: &str, selected: bool) -> Html {
    if selected {
        html! { option(value: (value), selected: "selected") { (label) } }.to_html()
    } else {
        html! { option(value: (value)) { (label) } }.to_html()
    }
}

fn secret_control(lang: Lang, field: &FieldSpec, stored: &Settings) -> Html {
    let is_set = stored.secret_is_set(field.key);
    let status = if is_set {
        t(lang, "settings-secret-set")
    } else {
        t(lang, "settings-secret-unset")
    };
    let clear = if is_set {
        clear_button(lang, field.key)
    } else {
        html! {}.to_html()
    };
    html! {
        div(class: "flex items-center gap-2") {
            input(
                type: "password", id: (field.key), name: (field.key), value: "",
                placeholder: (status), autocomplete: "new-password",
                class: "input input-sm input-bordered w-full",
            );
            (clear)
        }
    }
    .to_html()
}

fn clear_button(lang: Lang, key: &str) -> Html {
    html! {
        button(
            type: "button", class: "btn btn-sm btn-ghost",
            "data-on:click": (format!(
                "@post('/admin/settings/clear', {{contentType: 'form', body: {{key: '{key}'}}}})"
            )),
        ) { (icons::trash(14)) (t(lang, "settings-secret-clear")) }
    }
    .to_html()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every key the spec table derives must exist in every locale.
    ///
    /// [`t`] degrades a missing key to the key itself rather than panicking (a
    /// translation gap should not take a page down), which is exactly what
    /// makes this checkable: a resolved label never equals its own key.
    /// `session-core/build.rs` enforces parity across the other five locales
    /// from `en`, so this closes the remaining gap — a field added to
    /// [`SECTIONS`] with no prose written for it anywhere.
    #[test]
    fn every_spec_entry_has_a_label_and_help_in_every_locale() {
        let mut missing = Vec::new();
        for lang in Lang::ALL {
            for section in SECTIONS {
                let keys = section
                    .fields
                    .iter()
                    .flat_map(|f| [f.label_key(), f.help_key()]);
                for key in [section.title_key(), section.blurb_key()]
                    .into_iter()
                    .chain(keys)
                {
                    if t(lang, &key) == key {
                        missing.push(format!("{} {key}", lang.code()));
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "settings strings missing from locale files: {missing:#?}"
        );
    }

    /// And the other direction: a `settings-s-*`/`settings-f-*` message that no
    /// spec entry claims. Left behind by a renamed or deleted field, it is dead
    /// prose that six locale files keep translating.
    #[test]
    fn no_locale_string_outlives_the_field_it_described() {
        let claimed: BTreeSet<String> = SECTIONS
            .iter()
            .flat_map(|s| {
                let fields = s.fields.iter().flat_map(|f| [f.label_key(), f.help_key()]);
                [s.title_key(), s.blurb_key()].into_iter().chain(fields)
            })
            .collect();

        // `en` is the source of truth build.rs checks the others against, so
        // an orphan anywhere is an orphan here.
        let ftl = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../session-core/locales/en/settings.ftl"
        ));
        let orphans: Vec<&str> = ftl
            .lines()
            .filter_map(|line| line.split_once('=').map(|(k, _)| k.trim()))
            .filter(|k| k.starts_with("settings-s-") || k.starts_with("settings-f-"))
            .filter(|k| !claimed.contains(*k))
            .collect();
        assert!(
            orphans.is_empty(),
            "locale strings for fields that no longer exist: {orphans:#?}"
        );
    }
}
