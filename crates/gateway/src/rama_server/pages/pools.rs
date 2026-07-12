// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/pools` — CRUD editor for the DB-backed upstream pool topology.
//!
//! A pool groups one or more [backends](super::backends) under a `kind`
//! (chat / transcription / embedding / image / speech) and a picker
//! `strategy`, plus per-pool compliance flags, an offline fallback model,
//! advertised models, and (for speech) a language→voice map. Edits are
//! written to the DB via [`crate::server::db::upstreams_config`] but only
//! take effect on the runtime registry when the admin clicks "Apply changes"
//! (POST `/admin/upstreams/reload`).
//!
//! Also hosts the global unknown-model fallback editor (one model per kind),
//! backed by [`upstreams_config::set_fallback`].
//!
//! Gated on the `admin` role via [`super::require_admin_or_403`], same as the
//! other operator pages.

use std::collections::HashMap;
use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};

use super::{
    NavItem, checkbox_on, fetch_sidebar_chat, field, fields_all, is_admin, nav_or_html_page,
    parse_csv, read_form, require_admin_or_403, toast,
};
use session_core::chrome::{FlashKind, NavSections, Theme, is_datastar_request};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

use crate::rama_server::state::RamaState;
use crate::server::db::upstreams_config::{self, PoolRow, VoiceRow};

/// Pool kinds, matching the `snake_case` the `db_bridge` parser recognises.
const KINDS: &[&str] = &["chat", "transcription", "embedding", "image", "speech"];
/// Picker strategies, matching the `db_bridge` parser.
const STRATEGIES: &[&str] = &["least_inflight", "round_robin"];
/// Kinds that support an unknown-model fallback (speech deliberately has none —
/// a mistyped voice/model just surfaces the backend's own error).
const FALLBACK_KINDS: &[&str] = &["chat", "transcription", "embedding", "image"];

/// GET /admin/pools — the pool editor: an "Apply changes" reload button, the
/// per-kind unknown-model fallback editor, an "Add pool" form, and one
/// edit/delete form per stored pool.
pub async fn pools_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    // One snapshot yields pools + backend names + fallbacks together.
    let snapshot = upstreams_config::load_snapshot(&state.db)
        .await
        .unwrap_or_default();
    let mut pools = snapshot.pools.clone();
    pools.sort_by(|a, b| a.sort_order.cmp(&b.sort_order).then(a.name.cmp(&b.name)));
    let mut backend_names: Vec<String> = snapshot.backends.keys().cloned().collect();
    backend_names.sort();
    let all_models = state.upstreams.all_models();

    let body = render_pools_body(
        lang,
        &pools,
        &backend_names,
        &snapshot.fallbacks,
        &all_models,
    );
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        lang,
        nav,
        NavItem::Pools,
        &t(lang, "pools-page-title"),
        &user.email,
        is_admin(&state, &user),
        session.impersonator_id.is_some(),
        body,
        "/admin/pools",
        &chat,
    )
}

fn render_pools_body(
    lang: Lang,
    pools: &[PoolRow],
    backend_names: &[String],
    fallbacks: &HashMap<String, String>,
    all_models: &[String],
) -> Html {
    let reload = "@post('/admin/upstreams/reload')";
    let cards: Vec<Html> = pools
        .iter()
        .map(|p| render_pool_form(lang, Some(p), p.sort_order, backend_names))
        .collect();
    let next_order = pools.len() as i64;
    html! {
        section(class: "max-w-5xl mx-auto p-4 sm:p-6 flex flex-col gap-4") {
            header(class: "flex items-center justify-between gap-3 flex-wrap") {
                div(class: "flex flex-col gap-1") {
                    h1(class: "text-2xl font-bold") { (t(lang, "pools-heading")) }
                    p(class: "text-base-content/70 text-sm") { (t(lang, "pools-description")) }
                }
                button(class: "btn btn-warning btn-sm", "data-on:click": (reload)) {
                    (icons::check(14))
                    span { (t(lang, "backends-apply-changes")) }
                }
            }
            (render_fallbacks_card(lang, fallbacks, all_models))
            (render_pool_form(lang, None, next_order, backend_names))
            if !cards.is_empty() {
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

/// The global unknown-model fallback editor: one auto-saving `<select>` per
/// fallback-capable kind, populated from the advertised model set. Selecting
/// "(none)" clears the fallback for that kind.
fn render_fallbacks_card(
    lang: Lang,
    fallbacks: &HashMap<String, String>,
    all_models: &[String],
) -> Html {
    let selects: Vec<Html> = FALLBACK_KINDS
        .iter()
        .map(|kind| {
            fallback_select(
                lang,
                kind,
                fallbacks.get(*kind).map(String::as_str),
                all_models,
            )
        })
        .collect();
    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                h2(class: "card-title text-base") { (t(lang, "pools-fallbacks-heading")) }
                p(class: "text-base-content/70 text-sm") { (t(lang, "pools-fallbacks-description")) }
                div(class: "grid grid-cols-1 sm:grid-cols-2 gap-3") {
                    for s in selects.iter() {
                        (s.clone())
                    }
                }
            }
        }
    }
    .to_html()
}

fn fallback_select(lang: Lang, kind: &str, current: Option<&str>, all_models: &[String]) -> Html {
    let action = "/admin/pools/fallback";
    let post = format!("@post('{action}', {{contentType: 'form'}})");
    let mut opts: Vec<Html> = vec![super::select_option(
        "",
        &t(lang, "admin-cap-no-fallback"),
        false,
    )];
    for m in all_models {
        opts.push(super::select_option(m, m, current == Some(m.as_str())));
    }
    html! {
        form(method: "post", action: (action), class: "m-0") {
            input(type: "hidden", name: "kind", value: (kind.to_string()));
            label(class: "form-control gap-1") {
                span(class: "text-xs opacity-70 font-mono") { (kind.to_string()) }
                select(name: "model", class: "select select-bordered select-sm", "data-on:change": (post)) {
                    for o in opts.iter() { (o.clone()) }
                }
            }
        }
    }
    .to_html()
}

/// Editor form for one pool — empty (`existing = None`) for "Add pool",
/// pre-filled otherwise. `name` is the primary key, so it is read-only when
/// editing (rename = delete + re-add). Submits to `/admin/pools/save`; an
/// existing row also renders a delete form.
fn render_pool_form(
    lang: Lang,
    existing: Option<&PoolRow>,
    sort_order: i64,
    backend_names: &[String],
) -> Html {
    let action = "/admin/pools/save";
    let post = format!("@post('{action}', {{contentType: 'form'}})");
    let is_edit = existing.is_some();
    let name = existing.map(|p| p.name.clone()).unwrap_or_default();
    let kind = existing
        .map(|p| p.kind.clone())
        .unwrap_or_else(|| "chat".into());
    let strategy = existing
        .map(|p| p.strategy.clone())
        .unwrap_or_else(|| "least_inflight".into());
    let fallback_offline = existing
        .and_then(|p| p.fallback_offline.clone())
        .unwrap_or_default();
    // Compliance flags default to "compliant" (checked) for a new pool; the
    // stored bool otherwise. Unchecking a flag surfaces the chat warning.
    let gdpr = existing.map(|p| p.compliance_gdpr).unwrap_or(true);
    let nda = existing.map(|p| p.compliance_nda).unwrap_or(true);
    let enforce = existing.map(|p| p.enforce_limits).unwrap_or(true);
    let models = existing.map(|p| p.models.join(", ")).unwrap_or_default();
    let voices = existing.map(|p| voice_lines(&p.voices)).unwrap_or_default();
    let assigned: Vec<String> = existing.map(|p| p.backends.clone()).unwrap_or_default();
    let sort_order_str = sort_order.to_string();
    let title = if is_edit {
        name.clone()
    } else {
        t(lang, "pools-add-heading")
    };

    let kind_opts = options_for(KINDS, &kind);
    let strategy_opts = options_for(STRATEGIES, &strategy);
    // `readonly`/`checked` are presence-based (see `super::select_option`), so
    // the name input and every checkbox go through standalone helpers.
    let name_field = super::pk_name_input(&name, "chat-eu", is_edit);
    let backend_boxes: Vec<Html> = backend_names
        .iter()
        .map(|bn| super::bool_checkbox("backends", bn, bn, assigned.iter().any(|a| a == bn), true))
        .collect();
    let gdpr_box = super::bool_checkbox(
        "compliance_gdpr",
        "on",
        &t(lang, "pools-field-gdpr"),
        gdpr,
        false,
    );
    let nda_box = super::bool_checkbox(
        "compliance_nda",
        "on",
        &t(lang, "pools-field-nda"),
        nda,
        false,
    );
    let enforce_box = super::bool_checkbox(
        "enforce_limits",
        "on",
        &t(lang, "pools-field-enforce-limits"),
        enforce,
        false,
    );
    let delete_form = is_edit.then(|| {
        super::render_delete_form("/admin/pools/delete", &name, &t(lang, "pools-delete-pool"))
    });

    html! {
        article(class: "card border border-base-300 bg-base-100") {
            div(class: "card-body gap-3") {
                h3(class: "card-title text-base font-mono break-all") { (title) }
                form(
                    method: "post",
                    action: (action),
                    "data-on:submit__prevent": (post.clone()),
                    class: "flex flex-col gap-3 m-0"
                ) {
                    input(type: "hidden", name: "sort_order", value: (sort_order_str));
                    div(class: "grid grid-cols-1 sm:grid-cols-3 gap-3") {
                        label(class: "form-control gap-1") {
                            span(class: "text-xs opacity-70") { (t(lang, "pools-field-name")) }
                            (name_field)
                        }
                        label(class: "form-control gap-1") {
                            span(class: "text-xs opacity-70") { (t(lang, "pools-field-kind")) }
                            select(name: "kind", class: "select select-bordered select-sm") {
                                for o in kind_opts.iter() { (o.clone()) }
                            }
                        }
                        label(class: "form-control gap-1") {
                            span(class: "text-xs opacity-70") { (t(lang, "pools-field-strategy")) }
                            select(name: "strategy", class: "select select-bordered select-sm") {
                                for o in strategy_opts.iter() { (o.clone()) }
                            }
                        }
                    }
                    label(class: "form-control gap-1") {
                        span(class: "text-xs opacity-70") { (t(lang, "pools-field-fallback-offline")) }
                        input(
                            type: "text", name: "fallback_offline", value: (fallback_offline),
                            class: "input input-bordered input-sm font-mono",
                            placeholder: (t(lang, "pools-field-fallback-offline-placeholder"))
                        );
                    }
                    label(class: "form-control gap-1") {
                        span(class: "text-xs opacity-70") { (t(lang, "pools-field-models")) }
                        input(
                            type: "text", name: "models", value: (models),
                            class: "input input-bordered input-sm font-mono",
                            placeholder: "qwen-32b, glm-4.6"
                        );
                    }
                    label(class: "form-control gap-1") {
                        span(class: "text-xs opacity-70") { (t(lang, "pools-field-voices")) }
                        textarea(
                            name: "voices",
                            class: "textarea textarea-bordered textarea-sm font-mono",
                            rows: "2",
                            placeholder: "de=de-voice\nen=en-voice"
                        ) { (voices) }
                    }
                    fieldset(class: "flex flex-col gap-1") {
                        span(class: "text-xs opacity-70") { (t(lang, "pools-field-backends")) }
                        if backend_boxes.is_empty() {
                            span(class: "text-xs text-base-content/50 italic") { (t(lang, "pools-no-backends")) }
                        } else {
                            div(class: "flex flex-wrap gap-x-4 gap-y-1") {
                                for b in backend_boxes.iter() { (b.clone()) }
                            }
                        }
                    }
                    div(class: "flex flex-wrap gap-4") {
                        (gdpr_box)
                        (nda_box)
                        (enforce_box)
                    }
                    div(class: "flex justify-end") {
                        button(type: "submit", class: "btn btn-primary btn-sm") {
                            (icons::check(14))
                            span { (t(lang, if is_edit { "pools-save-pool" } else { "pools-add-pool" })) }
                        }
                    }
                }
                if let Some(df) = delete_form.as_ref() {
                    (df.clone())
                }
            }
        }
    }
    .to_html()
}

/// Build `<option>`s for a `<select>`, marking `current` as selected. The
/// option label is the raw snake_case value (matching the topology view on
/// `/admin/backends`).
fn options_for(values: &[&str], current: &str) -> Vec<Html> {
    values
        .iter()
        .map(|v| super::select_option(v, v, *v == current))
        .collect()
}

/// Serialise a pool's voices back into the textarea format (`lang=voice` per
/// line; a bare `voice` for the empty-key default).
fn voice_lines(voices: &[VoiceRow]) -> String {
    voices
        .iter()
        .map(|v| {
            if v.lang_code.is_empty() {
                v.voice_id.clone()
            } else {
                format!("{}={}", v.lang_code, v.voice_id)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the voices textarea: one `lang=voice` per line; a line with no `=` is
/// the empty-key default voice.
fn parse_voices(v: &str) -> Vec<VoiceRow> {
    v.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            match line.split_once('=') {
                Some((lang, voice)) => Some(VoiceRow {
                    lang_code: lang.trim().to_string(),
                    voice_id: voice.trim().to_string(),
                }),
                None => Some(VoiceRow {
                    lang_code: String::new(),
                    voice_id: line.to_string(),
                }),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Handlers (POST) — write the DB topology; the registry picks changes up on
// POST /admin/upstreams/reload ("Apply changes").
// ---------------------------------------------------------------------------

/// POST /admin/pools/save — insert or update a pool from the editor form.
pub async fn pools_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let pairs: Vec<(String, String)> = match read_form(body).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let name = field(&pairs, "name").trim();
    if name.is_empty() {
        return toast(FlashKind::Error, t(lang, "pools-error-name-required"));
    }
    let kind = field(&pairs, "kind").trim();
    if !KINDS.contains(&kind) {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "pools-error-invalid-kind",
                &i18n::args([("kind", kind.to_string().into())]),
            ),
        );
    }
    let strategy = {
        let s = field(&pairs, "strategy").trim();
        if STRATEGIES.contains(&s) {
            s
        } else {
            "least_inflight"
        }
    };
    let fallback_offline = {
        let v = field(&pairs, "fallback_offline").trim();
        (!v.is_empty()).then(|| v.to_string())
    };
    let sort_order: i64 = field(&pairs, "sort_order").trim().parse().unwrap_or(0);

    let row = PoolRow {
        name: name.to_string(),
        kind: kind.to_string(),
        strategy: strategy.to_string(),
        fallback_offline,
        compliance_gdpr: checkbox_on(field(&pairs, "compliance_gdpr")),
        compliance_nda: checkbox_on(field(&pairs, "compliance_nda")),
        enforce_limits: checkbox_on(field(&pairs, "enforce_limits")),
        sort_order,
        backends: fields_all(&pairs, "backends"),
        models: parse_csv(field(&pairs, "models")),
        voices: parse_voices(field(&pairs, "voices")),
        created_at: jiff::Timestamp::now(),
        updated_at: jiff::Timestamp::now(),
    };
    match upstreams_config::upsert_pool(&state.db, &row).await {
        Ok(()) => toast(
            FlashKind::Success,
            t_args(
                lang,
                "pools-saved",
                &i18n::args([("name", name.to_string().into())]),
            ),
        ),
        Err(e) => toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-upsert-error",
                &i18n::args([("err", e.to_string().into())]),
            ),
        ),
    }
}

/// POST /admin/pools/delete — remove a pool and its dependent rows (backends
/// stay; deleting a pool never deletes a backend).
pub async fn pools_delete(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let pairs: Vec<(String, String)> = match read_form(body).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let name = field(&pairs, "name").trim();
    match upstreams_config::delete_pool(&state.db, name).await {
        Ok(()) => toast(
            FlashKind::Success,
            t_args(
                lang,
                "pools-deleted",
                &i18n::args([("name", name.to_string().into())]),
            ),
        ),
        Err(e) => toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-error",
                &i18n::args([("err", e.to_string().into())]),
            ),
        ),
    }
}

/// POST /admin/pools/fallback — set or clear the unknown-model fallback for a
/// kind. An empty `model` clears it.
pub async fn pools_fallback_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let pairs: Vec<(String, String)> = match read_form(body).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let kind = field(&pairs, "kind").trim();
    if !FALLBACK_KINDS.contains(&kind) {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "pools-error-invalid-kind",
                &i18n::args([("kind", kind.to_string().into())]),
            ),
        );
    }
    let model = {
        let m = field(&pairs, "model").trim();
        (!m.is_empty()).then_some(m)
    };
    match upstreams_config::set_fallback(&state.db, kind, model).await {
        Ok(()) => {
            let msg = match model {
                Some(m) => t_args(
                    lang,
                    "pools-fallback-saved",
                    &i18n::args([
                        ("kind", kind.to_string().into()),
                        ("model", m.to_string().into()),
                    ]),
                ),
                None => t_args(
                    lang,
                    "pools-fallback-cleared",
                    &i18n::args([("kind", kind.to_string().into())]),
                ),
            };
            toast(FlashKind::Success, msg)
        }
        Err(e) => toast(
            FlashKind::Error,
            t_args(
                lang,
                "admin-db-error",
                &i18n::args([("err", e.to_string().into())]),
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;

    fn sample_pool() -> PoolRow {
        PoolRow {
            name: "chat-eu".into(),
            kind: "chat".into(),
            strategy: "round_robin".into(),
            fallback_offline: Some("glm-4.6".into()),
            compliance_gdpr: false,
            compliance_nda: true,
            enforce_limits: true,
            sort_order: 3,
            backends: vec!["gpu-01".into()],
            models: vec!["qwen-32b".into()],
            voices: vec![
                VoiceRow {
                    lang_code: "de".into(),
                    voice_id: "de-voice".into(),
                },
                VoiceRow {
                    lang_code: String::new(),
                    voice_id: "default-voice".into(),
                },
            ],
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        }
    }

    /// Voices round-trip through parse → serialise: `lang=voice`, and a bare
    /// line becomes the empty-key default voice.
    #[test]
    fn voices_round_trip() {
        let parsed = parse_voices("de=de-voice\ndefault-voice\nen = en-voice ");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].lang_code, "de");
        assert_eq!(parsed[0].voice_id, "de-voice");
        assert!(parsed[1].lang_code.is_empty());
        assert_eq!(parsed[1].voice_id, "default-voice");
        assert_eq!(parsed[2].lang_code, "en");
        assert_eq!(parsed[2].voice_id, "en-voice");
        assert_eq!(
            voice_lines(&parsed),
            "de=de-voice\ndefault-voice\nen=en-voice"
        );
    }

    /// The "Add pool" form posts to the save route via datastar and has no
    /// delete affordance.
    #[test]
    fn add_form_wires_to_save_endpoint() {
        let html = render_pool_form(Lang::En, None, 0, &["gpu-01".into()]).to_string();
        assert!(
            html.contains(r#"action="/admin/pools/save""#),
            "form must post to the save route: {html}"
        );
        assert!(
            html.contains("@post(") && html.contains("/admin/pools/save"),
            "submit must datastar-post to the save route: {html}"
        );
        assert!(
            !html.contains("/admin/pools/delete"),
            "add form must not render a delete form: {html}"
        );
    }

    /// An edit form marks the stored kind/strategy selected, checks assigned
    /// backends, reflects an unchecked compliance flag, locks the name, and
    /// offers a delete.
    #[test]
    fn edit_form_reflects_stored_state() {
        let p = sample_pool();
        let html = render_pool_form(
            Lang::En,
            Some(&p),
            p.sort_order,
            &["gpu-01".into(), "gpu-02".into()],
        )
        .to_string();
        assert!(
            html.contains(r#"value="chat" selected="selected""#),
            "stored kind must be selected: {html}"
        );
        assert!(
            html.contains(r#"value="round_robin" selected="selected""#),
            "stored strategy must be selected: {html}"
        );
        // gpu-01 is assigned → its checkbox is checked; gpu-02 is not.
        assert!(
            html.contains(r#"name="backends" value="gpu-01" checked="checked""#),
            "assigned backend must be checked: {html}"
        );
        assert!(
            html.contains(r#"name="backends" value="gpu-02" class="checkbox checkbox-sm""#),
            "unassigned backend must be unchecked: {html}"
        );
        // gdpr=false → the GDPR checkbox has no `checked`; nda=true → checked.
        assert!(
            html.contains(r#"name="compliance_nda" value="on" checked="checked""#),
            "nda true → checked: {html}"
        );
        assert!(
            html.contains(r#"name="compliance_gdpr" value="on" class="checkbox checkbox-sm""#),
            "gdpr false → not checked: {html}"
        );
        assert!(
            html.contains("readonly=\"readonly\""),
            "name must be read-only on edit: {html}"
        );
        assert!(
            html.contains(r#"action="/admin/pools/delete""#),
            "edit form must offer a delete: {html}"
        );
    }

    /// The per-kind fallback select auto-saves to the fallback route and marks
    /// the current model selected.
    #[test]
    fn fallback_select_wires_and_marks_current() {
        let models = vec!["qwen-32b".to_string(), "glm-4.6".to_string()];
        let html = fallback_select(Lang::En, "chat", Some("glm-4.6"), &models).to_string();
        assert!(
            html.contains(r#"action="/admin/pools/fallback""#),
            "must post to the fallback route: {html}"
        );
        assert!(
            html.contains(r#"name="kind" value="chat""#),
            "kind discriminator must be present: {html}"
        );
        assert!(
            html.contains(r#"value="glm-4.6" selected="selected""#),
            "current model must be selected: {html}"
        );
    }
}
