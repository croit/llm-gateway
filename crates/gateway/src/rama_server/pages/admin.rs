// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/*` pages. Currently just `/admin/models` for the
//! per-model sampling defaults — temperature, top_p, top_k,
//! min_p, repeat_penalty, frequency_penalty, presence_penalty,
//! max_tokens, stop tokens, etc. Each model gets a key=value TOML
//! textarea; the gateway parses it at save-time to reject
//! obviously-broken submissions and at request-time to merge
//! missing keys into the outgoing body. Client values always win.
//!
//! All routes are gated on the `admin` role via
//! [`super::require_admin_or_403`] — non-admins see a 403 page and
//! never the form. The sidebar entry is also conditional on that
//! role, so non-admins don't even know the page exists.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};

use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_admin_or_403};
use session_core::chrome::{
    Flash, FlashKind, NavSections, Theme, is_datastar_request, read_body_to_bytes, sse_response,
    sse_toast,
};
use session_core::icons;

use crate::rama_server::state::RamaState;
use crate::server::db::model_defaults as db;
use crate::server::model_defaults as merge;
use crate::server::upstreams::PoolKind;

/// GET /admin/models — one card per chat model, each with the
/// stored TOML as a textarea + a Save button. Models with no row
/// yet render an empty textarea (operator picks defaults from
/// scratch).
pub async fn models_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let mut models = state.upstreams.models_for_kind(PoolKind::Chat);
    models.sort();
    let mut rows: Vec<ModelRow> = Vec::with_capacity(models.len());
    for name in &models {
        let row = match db::get(&state.db, name).await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err, model = %name, "model_defaults: get failed");
                None
            }
        };
        rows.push(ModelRow {
            name: name.clone(),
            toml: row
                .as_ref()
                .map(|r| r.defaults_toml.clone())
                .unwrap_or_default(),
            reasoning_style: row
                .as_ref()
                .and_then(|r| r.reasoning_style.clone())
                .unwrap_or_default(),
            budget_standard: row.as_ref().and_then(|r| r.thinking_budget_standard),
            budget_deep: row.as_ref().and_then(|r| r.thinking_budget_deep),
            budget_max: row.as_ref().and_then(|r| r.thinking_budget_max),
            effort_standard: row
                .as_ref()
                .and_then(|r| r.reasoning_effort_standard.clone())
                .unwrap_or_default(),
            effort_deep: row
                .as_ref()
                .and_then(|r| r.reasoning_effort_deep.clone())
                .unwrap_or_default(),
            effort_max: row
                .as_ref()
                .and_then(|r| r.reasoning_effort_max.clone())
                .unwrap_or_default(),
        });
    }

    let body = render_models_body(&rows);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        nav,
        NavItem::Admin,
        "Model defaults — LLM Gateway",
        &user.email,
        is_admin(&state, &user),
        session.impersonator_id.is_some(),
        body,
        "/admin/models",
        &chat,
    )
}

/// POST /admin/models — save the per-model defaults. Form body
/// carries both the `model_name` (as a hidden input — putting it
/// in the URL path doesn't survive rama's path lowercasing +
/// case-sensitive HuggingFace IDs) and the `defaults_toml`. An
/// empty `defaults_toml` clears the stored row.
pub async fn models_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: SaveForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return toast(FlashKind::Error, format!("malformed form: {err}"));
        }
    };
    if form.model_name.is_empty() {
        return toast(FlashKind::Error, "missing model_name field");
    }
    let trimmed = form.defaults_toml.trim();
    if trimmed.is_empty() {
        if let Err(err) = db::delete(&state.db, &form.model_name).await {
            return toast(FlashKind::Error, format!("db delete: {err}"));
        }
        return toast(
            FlashKind::Success,
            format!("cleared defaults for `{}`", form.model_name),
        );
    }
    // Parse before persisting so we never store TOML that
    // `apply_defaults` would later reject — keeps the round-trip
    // honest (whatever you save is exactly what the merge will use).
    if let Err(err) = merge::parse_defaults(&form.defaults_toml) {
        return toast(FlashKind::Error, format!("invalid TOML: {err}"));
    }
    if let Err(err) = db::upsert(&state.db, &form.model_name, &form.defaults_toml).await {
        return toast(FlashKind::Error, format!("db upsert: {err}"));
    }
    toast(
        FlashKind::Success,
        format!("saved defaults for `{}`", form.model_name),
    )
}

/// POST /admin/models/reasoning — save a model's reasoning style (how its
/// reasoning budget is expressed on the wire). Kept separate from the TOML
/// save so clearing the sampling defaults (which deletes the row) doesn't also
/// reset the reasoning style, and vice versa. An empty / "auto" value clears
/// the explicit choice and falls back to name-based auto-detection.
pub async fn models_reasoning_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: ReasoningForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => return toast(FlashKind::Error, format!("malformed form: {err}")),
    };
    if form.model_name.is_empty() {
        return toast(FlashKind::Error, "missing model_name field");
    }
    // Empty / "auto" → clear the explicit choice (NULL), otherwise store the
    // canonical value. Validate against the known styles so a bad submission
    // can't poison the row.
    let style = match form.reasoning_style.trim() {
        "" | "auto" => None,
        s @ ("none" | "qwen" | "openai" | "glm" | "anthropic") => Some(s),
        other => {
            return toast(
                FlashKind::Error,
                format!("unknown reasoning style `{other}`"),
            );
        }
    };
    if let Err(err) = db::set_reasoning_style(&state.db, &form.model_name, style).await {
        return toast(FlashKind::Error, format!("db: {err}"));
    }
    toast(
        FlashKind::Success,
        format!("saved reasoning style for `{}`", form.model_name),
    )
}

/// POST /admin/models/reasoning-budget — save a model's per-effort reasoning
/// overrides (token budgets for Qwen/Anthropic, `reasoning_effort` levels for
/// OpenAI/GLM). Like the reasoning-style save, this touches only its own
/// columns so it composes with the TOML save and the style save. Empty fields
/// clear that level back to the built-in default.
pub async fn models_reasoning_budget_save(
    State(state): State<Arc<RamaState>>,
    req: Request,
) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: ReasoningBudgetForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => return toast(FlashKind::Error, format!("malformed form: {err}")),
    };
    if form.model_name.is_empty() {
        return toast(FlashKind::Error, "missing model_name field");
    }
    // Parse + validate each field; an empty string clears the level.
    let budget = |s: &str| -> Result<Option<i64>, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(None);
        }
        match s.parse::<i64>() {
            Ok(n) if n >= 1 => Ok(Some(n)),
            _ => Err(format!("budget `{s}` must be a positive integer")),
        }
    };
    let effort = |s: &str| -> Result<Option<String>, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(None);
        }
        // Validate against the full intensity scale (the GLM superset); the
        // dropdown only offers per-style-valid values anyway.
        if crate::server::reasoning::ReasoningStyle::Glm
            .effort_levels()
            .contains(&s)
        {
            Ok(Some(s.to_string()))
        } else {
            Err(format!("unknown reasoning effort `{s}`"))
        }
    };
    let build = || -> Result<db::ReasoningOverrideCols, String> {
        Ok(db::ReasoningOverrideCols {
            budget_standard: budget(&form.budget_standard)?,
            budget_deep: budget(&form.budget_deep)?,
            budget_max: budget(&form.budget_max)?,
            effort_standard: effort(&form.effort_standard)?,
            effort_deep: effort(&form.effort_deep)?,
            effort_max: effort(&form.effort_max)?,
        })
    };
    let cols = match build() {
        Ok(c) => c,
        Err(e) => return toast(FlashKind::Error, e),
    };
    if let Err(err) = db::set_reasoning_overrides(&state.db, &form.model_name, &cols).await {
        return toast(FlashKind::Error, format!("db: {err}"));
    }
    toast(
        FlashKind::Success,
        format!("saved reasoning budget for `{}`", form.model_name),
    )
}

#[derive(serde::Deserialize)]
struct SaveForm {
    model_name: String,
    defaults_toml: String,
}

#[derive(serde::Deserialize)]
struct ReasoningForm {
    model_name: String,
    reasoning_style: String,
}

/// Per-effort reasoning overrides form. All fields optional / empty = "clear
/// this level" (fall back to the built-in default). For token-budget styles the
/// `budget_*` fields are filled; for effort-level styles the `effort_*` fields.
/// The form only renders the relevant set, but we accept and store all six so a
/// later style switch can clear stale values.
#[derive(Default, serde::Deserialize)]
struct ReasoningBudgetForm {
    model_name: String,
    #[serde(default)]
    budget_standard: String,
    #[serde(default)]
    budget_deep: String,
    #[serde(default)]
    budget_max: String,
    #[serde(default)]
    effort_standard: String,
    #[serde(default)]
    effort_deep: String,
    #[serde(default)]
    effort_max: String,
}

struct ModelRow {
    name: String,
    toml: String,
    /// Stored reasoning style, or empty string for "auto" (name-detected).
    reasoning_style: String,
    /// Per-effort token budgets (token-budget styles); `None` = built-in default.
    budget_standard: Option<i64>,
    budget_deep: Option<i64>,
    budget_max: Option<i64>,
    /// Per-effort `reasoning_effort` levels (effort-level styles); empty = default.
    effort_standard: String,
    effort_deep: String,
    effort_max: String,
}

fn render_models_body(rows: &[ModelRow]) -> Html {
    let cards: Vec<Html> = rows.iter().map(render_model_card).collect();
    html! {
        section(class: "max-w-5xl mx-auto p-4 sm:p-6 flex flex-col gap-4") {
            header(class: "flex flex-col gap-1") {
                h1(class: "text-2xl font-bold") { "Model defaults" }
                p(class: "text-base-content/70 text-sm") {
                    "Server-wide default sampling parameters for this model, in TOML. These \
                     apply to "
                    strong { "every" }
                    " request for this model, from any user or token — unless the caller sets \
                     the same key in their own request, which "
                    strong { "always wins" }
                    ". Think of it as the floor everyone gets when they don't specify their own \
                     values. Empty = no defaults, the backend's built-in behaviour applies."
                }
            }
            if rows.is_empty() {
                div(class: "alert") {
                    (icons::info(18))
                    span {
                        "No chat models advertised yet. Once an upstream backend is reachable, \
                         it'll appear here."
                    }
                }
            } else {
                div(class: "flex flex-col gap-4") {
                    for c in cards.iter() {
                        (c.clone())
                    }
                }
            }
        }
    }
    .to_html()
}

fn render_model_card(row: &ModelRow) -> Html {
    let action = "/admin/models";
    let placeholder = "# Common keys (vLLM/OpenAI):\n\
                       # temperature      = 0.7\n\
                       # top_p            = 0.95\n\
                       # top_k            = 40\n\
                       # min_p            = 0.05\n\
                       # repeat_penalty   = 1.1\n\
                       # frequency_penalty= 0.0\n\
                       # presence_penalty = 0.0\n\
                       # max_tokens       = 2048\n\
                       # stop             = [\"<|im_end|>\"]\n";
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                header(class: "flex items-center justify-between gap-3 flex-wrap") {
                    h2(class: "card-title text-base font-mono break-all") { (row.name.clone()) }
                    (render_reasoning_select(row))
                }
                (render_reasoning_budget(row))
                form(
                    method: "post",
                    action: (action),
                    "data-on:submit__prevent":
                        (format!("@post('{action}', {{contentType: 'form'}})")),
                    class: "flex flex-col gap-2 m-0"
                ) {
                    input(type: "hidden", name: "model_name", value: (row.name.clone()));
                    label(class: "label sr-only", "for": (format!("toml-{}", row.name))) {
                        "TOML defaults"
                    }
                    textarea(
                        id: (format!("toml-{}", row.name)),
                        name: "defaults_toml",
                        class: "textarea textarea-bordered font-mono text-sm w-full leading-relaxed",
                        rows: "16",
                        spellcheck: "false",
                        placeholder: (placeholder)
                    ) { (row.toml.clone()) }
                    div(class: "flex justify-end") {
                        button(type: "submit", class: "btn btn-primary btn-sm") {
                            (icons::check(14))
                            span { "Save" }
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

/// The per-model "reasoning style" picker: a tiny form whose `<select>`
/// auto-saves on change. Tells `apply_effort` how this model expects its
/// reasoning budget on the wire; "Auto" leaves it to name detection.
fn render_reasoning_select(row: &ModelRow) -> Html {
    let action = "/admin/models/reasoning";
    let options: &[(&str, &str)] = &[
        ("", "Reasoning: Auto"),
        ("none", "Reasoning: none"),
        ("qwen", "Reasoning: Qwen (vLLM)"),
        ("openai", "Reasoning: OpenAI"),
        ("glm", "Reasoning: GLM / z.AI"),
        ("anthropic", "Reasoning: Anthropic"),
    ];
    let current = row.reasoning_style.as_str();
    let option_html: Vec<Html> = options
        .iter()
        .map(|(value, label)| {
            if *value == current {
                html! { option(value: (*value), selected: "selected") { (*label) } }.to_html()
            } else {
                html! { option(value: (*value)) { (*label) } }.to_html()
            }
        })
        .collect();
    html! {
        form(
            method: "post",
            action: (action),
            class: "m-0"
        ) {
            input(type: "hidden", name: "model_name", value: (row.name.clone()));
            select(
                name: "reasoning_style",
                "aria-label": "Reasoning style",
                "data-on:change": (format!("@post('{action}', {{contentType: 'form'}})")),
                class: "select select-bordered select-xs"
            ) {
                for o in option_html.iter() {
                    (o.clone())
                }
            }
        }
    }
    .to_html()
}

/// Adaptive per-effort reasoning controls, shown below the style picker. Token-
/// budget styles (Qwen, Anthropic) get integer token fields; effort-level styles
/// (OpenAI, GLM) get `reasoning_effort` dropdowns; styles without reasoning
/// render nothing. The effective style is resolved the same way the request path
/// does (explicit choice, else name detection), so the right controls appear
/// even when the style is left on "Auto".
fn render_reasoning_budget(row: &ModelRow) -> Html {
    use crate::server::reasoning::ReasoningStyle;
    let explicit = (!row.reasoning_style.is_empty()).then_some(row.reasoning_style.as_str());
    let style = ReasoningStyle::resolve(explicit, &row.name);
    let action = "/admin/models/reasoning-budget";

    let (controls, hint) = if style.uses_token_budget() {
        let num = |name: &str, label: &str, val: &Option<i64>| {
            let v = val.map(|n| n.to_string()).unwrap_or_default();
            html! {
                label(class: "form-control") {
                    span(class: "label-text text-xs") { (label) }
                    input(
                        type: "number", name: (name), value: (v), min: "1",
                        placeholder: "default",
                        class: "input input-bordered input-xs w-28"
                    );
                }
            }
            .to_html()
        };
        let controls = html! {
            (num("budget_standard", "Standard", &row.budget_standard))
            (num("budget_deep", "Deep", &row.budget_deep))
            (num("budget_max", "Max", &row.budget_max))
        }
        .to_html();
        (
            controls,
            "Max thinking tokens per effort level. Blank = backend default \
             (uncapped). Fast disables thinking.",
        )
    } else if style.uses_effort_level() {
        let levels = style.effort_levels();
        let sel = |name: &str, label: &str, current: &str| {
            let mut opts: Vec<Html> = Vec::new();
            opts.push(if current.is_empty() {
                html! { option(value: "", selected: "selected") { "(default)" } }.to_html()
            } else {
                html! { option(value: "") { "(default)" } }.to_html()
            });
            for lvl in levels {
                opts.push(if *lvl == current {
                    html! { option(value: (*lvl), selected: "selected") { (*lvl) } }.to_html()
                } else {
                    html! { option(value: (*lvl)) { (*lvl) } }.to_html()
                });
            }
            html! {
                label(class: "form-control") {
                    span(class: "label-text text-xs") { (label) }
                    select(name: (name), class: "select select-bordered select-xs") {
                        for o in opts.iter() { (o.clone()) }
                    }
                }
            }
            .to_html()
        };
        let controls = html! {
            (sel("effort_standard", "Standard", &row.effort_standard))
            (sel("effort_deep", "Deep", &row.effort_deep))
            (sel("effort_max", "Max", &row.effort_max))
        }
        .to_html();
        (
            controls,
            "Reasoning effort per level. Blank = built-in default. Fast disables thinking.",
        )
    } else {
        // No reasoning support → no controls.
        return html! { (String::new()) }.to_html();
    };

    html! {
        form(
            method: "post",
            action: (action),
            "data-on:submit__prevent":
                (format!("@post('{action}', {{contentType: 'form'}})")),
            class: "flex flex-col gap-2 m-0 border-t border-base-300 pt-3"
        ) {
            input(type: "hidden", name: "model_name", value: (row.name.clone()));
            span(class: "text-xs text-base-content/60") { (hint) }
            div(class: "flex flex-wrap items-end gap-3") {
                (controls)
                button(type: "submit", class: "btn btn-ghost btn-xs ml-auto self-end") {
                    (icons::check(12))
                    span { "Save reasoning budget" }
                }
            }
        }
    }
    .to_html()
}

fn toast(kind: FlashKind, message: impl Into<String>) -> Response {
    sse_response(&[sse_toast(&Flash {
        kind,
        message: message.into(),
    })])
}
