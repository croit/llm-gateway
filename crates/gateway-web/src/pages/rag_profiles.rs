// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/rag/profiles` — the extraction-profile editor.
//!
//! A profile decides what gets pulled out of every document in a collection:
//! the fields that make "the most recent invoice from X" or "how much did we
//! spend" answerable at all. Two ship seeded (`invoice`,
//! `project_document`), which covers an invoice archive and a project folder
//! and nothing else — what a *vendor* or a *project* means differs per
//! customer, and a corpus of contracts or lab reports wants fields nobody
//! shipping this could guess.
//!
//! Fields are authored as JSON rather than through a row-builder UI. That is
//! a deliberate trade: the schema is small, operators editing it are the same
//! people who write `gateway.toml`, and a hand-rolled repeater would be a lot
//! of markup for a form that is edited once per corpus. The JSON is validated
//! on save and the error names the problem.
//!
//! **Editing bumps the profile's version**, which is part of the extraction
//! cache key. Without that, every already-processed document would keep
//! serving fields extracted under the old prompt — an edit that appears to do
//! nothing, which is the worst outcome for someone trying to fix a bad
//! extraction. The page says so, with the number of documents affected.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::{Path, State};
use rama::http::{Request, Response};
use serde::Deserialize;

use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_admin_or_403, toast};
use session_core::chrome::{
    Flash, FlashKind, NavSections, Theme, is_datastar_request, read_body_to_bytes, sse_patch,
    sse_response, sse_toast,
};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

use gateway_core::server::db::rag_documents as docs_db;
use gateway_runtime::rama_server::state::RamaState;

#[derive(Deserialize)]
struct ProfileForm {
    name: String,
    #[serde(default)]
    description: Option<String>,
    prompt: String,
    fields_json: String,
}

/// Validate the submitted form into something storable.
///
/// Every failure names what to fix: an operator who mistyped a field type
/// should not have to guess which of six fields was wrong.
fn validate(lang: Lang, form: &ProfileForm) -> Result<docs_db::ProfileInput, String> {
    let name = form.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(t(lang, "rag-profile-toast-name-length"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(t(lang, "rag-profile-toast-name-charset"));
    }
    let prompt = form.prompt.trim();
    if prompt.is_empty() {
        return Err(t(lang, "rag-profile-toast-prompt-required"));
    }
    let fields: Vec<docs_db::ProfileField> = serde_json::from_str(form.fields_json.trim())
        .map_err(|e| {
            t_args(
                lang,
                "rag-profile-toast-fields-invalid",
                &i18n::args([("err", e.to_string().into())]),
            )
        })?;
    if fields.is_empty() {
        return Err(t(lang, "rag-profile-toast-fields-empty"));
    }
    let mut seen = std::collections::HashSet::new();
    for f in &fields {
        if f.key.trim().is_empty() {
            return Err(t(lang, "rag-profile-toast-field-key-required"));
        }
        // Keys become query arguments and column keys; a duplicate would
        // silently shadow the other in the EAV table's primary key.
        if !seen.insert(f.key.clone()) {
            return Err(t_args(
                lang,
                "rag-profile-toast-field-duplicate",
                &i18n::args([("key", f.key.clone().into())]),
            ));
        }
        if f.field_type == docs_db::FieldType::Enum && f.values.is_empty() {
            return Err(t_args(
                lang,
                "rag-profile-toast-enum-values",
                &i18n::args([("key", f.key.clone().into())]),
            ));
        }
    }
    Ok(docs_db::ProfileInput {
        name: name.to_string(),
        description: form
            .description
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        prompt: prompt.to_string(),
        fields,
    })
}

pub async fn profiles_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let profiles = docs_db::list_profiles(&state.db).await.unwrap_or_default();
    let body = render_body(lang, &profiles);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    let title = t(lang, "rag-profile-page-title");
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
    nav_or_html_page(&pctx, NavItem::Rag, &title, body, "/rag/profiles", &chat)
}

pub async fn profile_create(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: ProfileForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "rag-toast-malformed-form",
                    &i18n::args([("err", err.to_string().into())]),
                ),
            );
        }
    };
    let input = match validate(lang, &form) {
        Ok(i) => i,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    if let Err(err) = docs_db::create_profile(&state.db, &input).await {
        let s = err.to_string();
        tracing::warn!(error = %err, "create extraction profile");
        return toast(
            FlashKind::Error,
            if s.contains("UNIQUE") || s.contains("constraint") {
                t_args(
                    lang,
                    "rag-profile-toast-name-exists",
                    &i18n::args([("name", input.name.clone().into())]),
                )
            } else {
                t(lang, "rag-profile-toast-save-failed")
            },
        );
    }
    patch_list(
        &state,
        lang,
        t_args(
            lang,
            "rag-profile-toast-created",
            &i18n::args([("name", input.name.into())]),
        ),
    )
    .await
}

pub async fn profile_update(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let form: ProfileForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return toast(
                FlashKind::Error,
                t_args(
                    lang,
                    "rag-toast-malformed-form",
                    &i18n::args([("err", err.to_string().into())]),
                ),
            );
        }
    };
    let input = match validate(lang, &form) {
        Ok(i) => i,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    if let Err(err) = docs_db::update_profile(&state.db, id, &input).await {
        tracing::warn!(error = %err, %id, "update extraction profile");
        return toast(FlashKind::Error, t(lang, "rag-profile-toast-save-failed"));
    }
    // The version bump invalidates every cached extraction under this
    // profile, so the collections using it must re-index to pick up the new
    // shape. Saying so beats an operator wondering why nothing changed.
    let users = docs_db::collections_using_profile(&state.db, id)
        .await
        .unwrap_or_default();
    let msg = if users.is_empty() {
        t_args(
            lang,
            "rag-profile-toast-saved",
            &i18n::args([("name", input.name.into())]),
        )
    } else {
        t_args(
            lang,
            "rag-profile-toast-saved-reindex",
            &i18n::args([
                ("name", input.name.into()),
                ("collections", users.join(", ").into()),
            ]),
        )
    };
    patch_list(&state, lang, msg).await
}

pub async fn profile_delete(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    // Refused rather than cascaded: a collection whose profile vanished
    // indexes without fields, and the operator gets a puzzle instead of an
    // error.
    let users = docs_db::collections_using_profile(&state.db, id)
        .await
        .unwrap_or_default();
    if !users.is_empty() {
        return toast(
            FlashKind::Error,
            t_args(
                lang,
                "rag-profile-toast-in-use",
                &i18n::args([("collections", users.join(", ").into())]),
            ),
        );
    }
    match docs_db::delete_profile(&state.db, id).await {
        Ok(true) => patch_list(&state, lang, t(lang, "rag-profile-toast-deleted")).await,
        Ok(false) => toast(FlashKind::Error, t(lang, "rag-profile-toast-builtin")),
        Err(err) => {
            tracing::warn!(error = %err, %id, "delete extraction profile");
            toast(FlashKind::Error, t(lang, "rag-profile-toast-save-failed"))
        }
    }
}

/// Re-render the whole list and toast. The list is small and edits are rare,
/// so a surgical per-row patch would be more machinery than it saves.
async fn patch_list(state: &RamaState, lang: Lang, message: String) -> Response {
    let profiles = docs_db::list_profiles(&state.db).await.unwrap_or_default();
    sse_response(&[
        sse_patch(
            Some("#rag-profile-list"),
            Some("outer"),
            &render_list(lang, &profiles).to_string(),
        ),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message,
        }),
    ])
}

/// Pretty-print a profile's fields for the editor textarea.
fn fields_json(profile: &docs_db::Profile) -> String {
    serde_json::to_string_pretty(&profile.fields).unwrap_or_else(|_| "[]".into())
}

/// A worked example, so a first-time operator has something to edit rather
/// than a blank box and a schema to guess at.
const EXAMPLE_FIELDS: &str = r#"[
  {
    "key": "counterparty",
    "label": "Counterparty",
    "type": "text",
    "description": "The other organisation this contract is with.",
    "filterable": true,
    "sortable": true
  },
  {
    "key": "effective_date",
    "label": "Effective date",
    "type": "date",
    "description": "The date the agreement takes effect.",
    "filterable": true,
    "sortable": true
  },
  {
    "key": "status",
    "label": "Status",
    "type": "enum",
    "values": ["draft", "signed", "expired"],
    "description": "Where the document says it stands.",
    "filterable": true,
    "sortable": false
  }
]"#;

fn render_row(lang: Lang, p: &docs_db::Profile) -> Html {
    let dom_id = format!("rag-profile-{}", p.id);
    let update_action = format!("/rag/profiles/{}/update", p.id);
    let delete_action = format!("/rag/profiles/{}/delete", p.id);
    let update_directive = format!("@post('{update_action}', {{contentType: 'form'}})");
    let delete_directive = format!("@post('{delete_action}', {{contentType: 'form'}})");
    let description = p.description.clone().unwrap_or_default();
    let fields = fields_json(p);
    let version = t_args(
        lang,
        "rag-profile-version",
        &i18n::args([("version", p.version.into())]),
    );
    let summary = t_args(
        lang,
        "rag-profile-summary",
        &i18n::args([("count", (p.fields.len() as i64).into())]),
    );
    let builtin = p.builtin;
    html! {
        li(id: (dom_id), class: "py-4") {
            details(class: "group") {
                summary(class: "cursor-pointer flex items-center gap-2 flex-wrap") {
                    span(class: "font-mono font-semibold") { (p.name.clone()) }
                    if builtin {
                        span(class: "badge badge-ghost badge-sm") { (t(lang, "rag-profile-builtin")) }
                    }
                    span(class: "badge badge-outline badge-sm") { (version) }
                    span(class: "text-sm opacity-70") { (summary) }
                }
                form(
                    action: (update_action.clone()),
                    method: "post",
                    class: "mt-3 flex flex-col gap-3",
                    "data-on:submit__prevent": (update_directive)
                ) {
                    div(class: "grid grid-cols-1 md:grid-cols-2 gap-3") {
                        label(class: "flex flex-col gap-1 w-full") {
                            div(class: "label") { span(class: "label-text") { (t(lang, "rag-profile-label-name")) } }
                            input(
                                name: "name",
                                type: "text",
                                value: (p.name.clone()),
                                required: "required",
                                class: "input input-bordered w-full font-mono"
                            );
                        }
                        label(class: "flex flex-col gap-1 w-full") {
                            div(class: "label") { span(class: "label-text") { (t(lang, "rag-profile-label-description")) } }
                            input(
                                name: "description",
                                type: "text",
                                value: (description),
                                class: "input input-bordered w-full"
                            );
                        }
                    }
                    label(class: "flex flex-col gap-1 w-full") {
                        div(class: "label") { span(class: "label-text") { (t(lang, "rag-profile-label-prompt")) } }
                        textarea(
                            name: "prompt",
                            rows: "4",
                            class: "textarea textarea-bordered w-full"
                        ) { (p.prompt.clone()) }
                    }
                    label(class: "flex flex-col gap-1 w-full") {
                        div(class: "label") { span(class: "label-text") { (t(lang, "rag-profile-label-fields")) } }
                        textarea(
                            name: "fields_json",
                            rows: "12",
                            class: "textarea textarea-bordered w-full font-mono text-xs"
                        ) { (fields) }
                    }
                    div(class: "alert alert-warning text-sm") {
                        (icons::alert(16))
                        span { (t(lang, "rag-profile-edit-warning")) }
                    }
                    div(class: "flex justify-end gap-2") {
                        if !builtin {
                            button(
                                type: "button",
                                class: "btn btn-sm btn-outline btn-error",
                                "data-on:click": (delete_directive)
                            ) {
                                (t(lang, "rag-profile-button-delete"))
                            }
                        }
                        button(type: "submit", class: "btn btn-sm btn-primary") {
                            (t(lang, "rag-profile-button-save"))
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

fn render_list(lang: Lang, profiles: &[docs_db::Profile]) -> Html {
    let rows: Vec<Html> = profiles.iter().map(|p| render_row(lang, p)).collect();
    let empty = profiles.is_empty();
    html! {
        ul(id: "rag-profile-list", class: "divide-y divide-base-300 m-0 p-0 list-none") {
            if empty {
                li(class: "py-4 opacity-70") { (t(lang, "rag-profile-empty")) }
            }
            for row in rows.iter() {
                (row.clone())
            }
        }
    }
    .to_html()
}

fn render_body(lang: Lang, profiles: &[docs_db::Profile]) -> Html {
    let list = render_list(lang, profiles);
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
            div(class: "flex items-center gap-2 mb-2") {
                (icons::folder(20))
                h1(class: "text-2xl font-bold m-0") { (t(lang, "rag-profile-heading")) }
            }
            p(class: "text-base-content/60 text-sm mb-6") { (t(lang, "rag-profile-description")) }

            form(
                id: "rag-profile-create-form",
                action: "/rag/profiles",
                method: "post",
                class: "card border border-base-300 mb-6",
                "data-on:submit__prevent": "@post('/rag/profiles', {contentType: 'form'})"
            ) {
                div(class: "card-body gap-3") {
                    h2(class: "card-title text-base") { (t(lang, "rag-profile-create-heading")) }
                    div(class: "grid grid-cols-1 md:grid-cols-2 gap-3") {
                        label(class: "flex flex-col gap-1 w-full") {
                            div(class: "label") { span(class: "label-text") { (t(lang, "rag-profile-label-name")) } }
                            input(
                                name: "name",
                                type: "text",
                                required: "required",
                                placeholder: "contract",
                                class: "input input-bordered w-full font-mono"
                            );
                        }
                        label(class: "flex flex-col gap-1 w-full") {
                            div(class: "label") { span(class: "label-text") { (t(lang, "rag-profile-label-description")) } }
                            input(
                                name: "description",
                                type: "text",
                                class: "input input-bordered w-full"
                            );
                        }
                    }
                    label(class: "flex flex-col gap-1 w-full") {
                        div(class: "label") { span(class: "label-text") { (t(lang, "rag-profile-label-prompt")) } }
                        textarea(
                            name: "prompt",
                            rows: "3",
                            required: "required",
                            placeholder: (t(lang, "rag-profile-prompt-placeholder")),
                            class: "textarea textarea-bordered w-full"
                        ) {}
                    }
                    label(class: "flex flex-col gap-1 w-full") {
                        div(class: "label") { span(class: "label-text") { (t(lang, "rag-profile-label-fields")) } }
                        textarea(
                            name: "fields_json",
                            rows: "14",
                            class: "textarea textarea-bordered w-full font-mono text-xs"
                        ) { (EXAMPLE_FIELDS) }
                        p(class: "text-xs opacity-70") { (t(lang, "rag-profile-fields-help")) }
                    }
                    div(class: "card-actions justify-end") {
                        button(type: "submit", class: "btn btn-primary") {
                            (t(lang, "rag-profile-button-create"))
                        }
                    }
                }
            }

            section(class: "card border border-base-300") {
                div(class: "card-body") {
                    h2(class: "card-title") { (t(lang, "rag-profile-list-heading")) }
                    (list)
                }
            }
        }
    }
    .to_html()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;

    fn form(fields: &str) -> ProfileForm {
        ProfileForm {
            name: "contract".into(),
            description: Some("Contracts".into()),
            prompt: "Extract contract fields.".into(),
            fields_json: fields.into(),
        }
    }

    fn profile() -> docs_db::Profile {
        docs_db::Profile {
            id: 1,
            name: "invoice".into(),
            description: Some("Invoices".into()),
            prompt: "Extract invoice fields.".into(),
            fields: vec![docs_db::ProfileField {
                key: "vendor".into(),
                label: "Vendor".into(),
                field_type: docs_db::FieldType::Text,
                description: "Who billed us.".into(),
                values: vec![],
                filterable: true,
                sortable: true,
            }],
            version: 3,
            builtin: true,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        }
    }

    #[test]
    fn a_valid_field_list_parses() {
        let input =
            validate(Lang::En, &form(EXAMPLE_FIELDS)).expect("the shipped example is valid");
        assert_eq!(input.fields.len(), 3);
        assert_eq!(input.fields[1].field_type, docs_db::FieldType::Date);
    }

    #[test]
    fn malformed_json_is_reported_rather_than_saved_empty() {
        let err = validate(Lang::En, &form("{not json")).expect_err("must not save");
        assert!(!err.is_empty());
    }

    #[test]
    fn an_empty_field_list_is_rejected() {
        assert!(validate(Lang::En, &form("[]")).is_err());
    }

    #[test]
    fn a_duplicate_key_is_rejected_by_name() {
        // Two fields with one key would collide in the EAV primary key and
        // one would silently win.
        let dup = r#"[{"key":"a","label":"A","type":"text"},
                      {"key":"a","label":"B","type":"text"}]"#;
        let err = validate(Lang::En, &form(dup)).expect_err("must not save");
        assert!(err.contains('a'), "{err}");
    }

    #[test]
    fn an_enum_without_values_is_rejected() {
        let bad = r#"[{"key":"status","label":"Status","type":"enum"}]"#;
        let err = validate(Lang::En, &form(bad)).expect_err("must not save");
        assert!(err.to_lowercase().contains("status"), "{err}");
    }

    #[test]
    fn a_name_that_is_not_a_safe_identifier_is_rejected() {
        let mut f = form(EXAMPLE_FIELDS);
        f.name = "my profile!".into();
        assert!(validate(Lang::En, &f).is_err());
    }

    #[test]
    fn an_empty_prompt_is_rejected() {
        let mut f = form(EXAMPLE_FIELDS);
        f.prompt = "   ".into();
        assert!(validate(Lang::En, &f).is_err());
    }

    #[test]
    fn the_row_wires_its_buttons_to_the_real_endpoints() {
        let html = render_row(Lang::En, &profile()).to_string();
        assert!(html.contains("/rag/profiles/1/update"), "{html}");
        assert!(
            !html.contains("/rag/profiles/1/delete"),
            "a built-in profile offers no delete: a collection pointing at a vanished \
             profile indexes without fields"
        );
    }

    #[test]
    fn a_custom_profile_can_be_deleted() {
        let mut p = profile();
        p.builtin = false;
        let html = render_row(Lang::En, &p).to_string();
        assert!(html.contains("/rag/profiles/1/delete"), "{html}");
    }

    #[test]
    fn the_editor_round_trips_the_stored_fields() {
        let html = render_row(Lang::En, &profile()).to_string();
        assert!(html.contains("vendor"));
        assert!(html.contains("Extract invoice fields."));
    }

    #[test]
    fn the_create_form_ships_a_worked_example_not_a_blank_box() {
        let html = render_body(Lang::En, &[]).to_string();
        assert!(html.contains("counterparty"), "{html}");
        assert!(html.contains("/rag/profiles"));
    }
}
