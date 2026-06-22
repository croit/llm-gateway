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
        let (stored, reasoning_style) = match db::get(&state.db, name).await {
            Ok(Some(r)) => (r.defaults_toml, r.reasoning_style),
            Ok(None) => (String::new(), None),
            Err(err) => {
                tracing::warn!(error = %err, model = %name, "model_defaults: get failed");
                (String::new(), None)
            }
        };
        rows.push(ModelRow {
            name: name.clone(),
            toml: stored,
            reasoning_style: reasoning_style.unwrap_or_default(),
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

struct ModelRow {
    name: String,
    toml: String,
    /// Stored reasoning style, or empty string for "auto" (name-detected).
    reasoning_style: String,
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

fn toast(kind: FlashKind, message: impl Into<String>) -> Response {
    sse_response(&[sse_toast(&Flash {
        kind,
        message: message.into(),
    })])
}
