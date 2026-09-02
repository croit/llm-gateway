// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/skills` — the per-user, **private** Agent Skills page.
//!
//! The user-owned counterpart to the admin-only `/admin/skills`: every
//! signed-in user manages their *own* skills here, invisible to other users.
//! Ownership is the grant — a private skill is loadable in that user's chats
//! with no RBAC role, and overlays the global operator skills (private shadows
//! global on a name collision; see [`gateway_features::server::skills::combined_registry`]).
//!
//! Two authoring paths, both landing in the same per-user store
//! ([`gateway_features::server::skills::UserSkillStore`]):
//!
//!   - **Upload** a `.skill` archive (same shape the admin page accepts), or
//!   - **write `SKILL.md` inline** in a textarea and save it.
//!
//! It's a master-detail viewer like the admin page — a left rail lists the
//! user's skills, the detail pane renders the selected `SKILL.md` (view mode)
//! or a raw editor (edit/new mode). Mutations are plain form POSTs that
//! redirect back. Signed-in-user gated (not admin), unlike `/admin/skills`.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};

use super::skills::{percent_decode, read_upload_field, selected_skill_param};
use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_session_or_redirect};
use gateway_core::server::db::users::User;
use gateway_features::server::skills::{self, UserSkillStore};
use gateway_runtime::rama_server::state::RamaState;
use session_core::chrome::{
    NavSections, Theme, is_datastar_request, read_body_to_bytes, see_other,
};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

/// The `SKILL.md` a fresh "New skill" editor starts from — a minimal valid
/// bundle the user fills in. The `name` here is what the skill is saved as, so
/// the placeholder is a valid slug.
const NEW_SKILL_TEMPLATE: &str = "---\nname: my-skill\ntitle: My Skill\ndescription: \
One line describing when the assistant should use this skill.\n---\n\n# Instructions\n\n\
Write what the assistant should do when this skill is loaded.\n";

/// Which pane the detail column shows.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Mode {
    /// Rendered SKILL.md of the selected skill.
    View,
    /// Raw editor for the selected skill.
    Edit,
    /// Raw editor for a brand-new skill.
    New,
}

/// GET /skills — master-detail viewer. `?skill=<name>` selects; `?edit=1`
/// opens the editor for the selected skill; `?new=1` opens a blank editor.
pub async fn user_skills_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = require_session!(state, req);
    let selected = selected_skill_param(&req);
    let mode = if has_flag(&req, "new") {
        Mode::New
    } else if has_flag(&req, "edit") {
        Mode::Edit
    } else {
        Mode::View
    };
    render_page(
        &state,
        datastar,
        theme,
        lang,
        nav,
        &user,
        session.impersonator_id.is_some(),
        selected.as_deref(),
        mode,
        None,
        None,
    )
    .await
}

/// POST /skills/upload — accept a `.skill` archive (multipart field `skill`),
/// install it as one of this user's private skills, and redirect to it.
pub async fn user_skills_upload(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = require_session!(state, req);
    let impersonating = session.impersonator_id.is_some();
    let Some(store) = state.user_skills() else {
        return err_page(
            &state,
            datastar,
            theme,
            lang,
            nav,
            &user,
            impersonating,
            &t(lang, "my-skills-error-not-configured"),
        )
        .await;
    };

    let content_type = req
        .headers()
        .get(rama::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => {
            return err_page(
                &state,
                datastar,
                theme,
                lang,
                nav,
                &user,
                impersonating,
                &msg,
            )
            .await;
        }
    };
    let bytes = match read_upload_field(&content_type, body).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            return err_page(
                &state,
                datastar,
                theme,
                lang,
                nav,
                &user,
                impersonating,
                &t(lang, "my-skills-error-no-file"),
            )
            .await;
        }
        Err(msg) => {
            return err_page(
                &state,
                datastar,
                theme,
                lang,
                nav,
                &user,
                impersonating,
                &msg,
            )
            .await;
        }
    };

    match store.install_archive(&user.id, &bytes) {
        Ok(name) => see_other(&format!("/skills?skill={name}")),
        Err(err) => {
            let msg = t_args(
                lang,
                "my-skills-error-install-failed",
                &i18n::args([("error", err.to_string().into())]),
            );
            err_page(
                &state,
                datastar,
                theme,
                lang,
                nav,
                &user,
                impersonating,
                &msg,
            )
            .await
        }
    }
}

/// POST /skills/save — create or edit a private skill from raw `SKILL.md` text
/// (the inline editor). `name` is the target slug (empty for a new skill, in
/// which case it's derived from the content's frontmatter); `content` is the
/// full `SKILL.md`.
pub async fn user_skills_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = require_session!(state, req);
    let impersonating = session.impersonator_id.is_some();
    let Some(store) = state.user_skills() else {
        return err_page(
            &state,
            datastar,
            theme,
            lang,
            nav,
            &user,
            impersonating,
            &t(lang, "my-skills-error-not-configured"),
        )
        .await;
    };

    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => {
            return err_page(
                &state,
                datastar,
                theme,
                lang,
                nav,
                &user,
                impersonating,
                &msg,
            )
            .await;
        }
    };
    let form: SaveForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            return err_page(
                &state,
                datastar,
                theme,
                lang,
                nav,
                &user,
                impersonating,
                &err.to_string(),
            )
            .await;
        }
    };

    // A new skill (empty `name`) takes its slug from the frontmatter, so the
    // user names it once; an edit carries the original slug so a changed
    // frontmatter `name` is rejected (rather than silently orphaning a rename).
    let is_new = form.name.trim().is_empty();
    let target = if is_new {
        match skills::manifest_name(&form.content) {
            Some(n) => n,
            None => {
                return editor_error(
                    &state,
                    datastar,
                    theme,
                    lang,
                    nav,
                    &user,
                    impersonating,
                    Mode::New,
                    "",
                    &form.content,
                    &t(lang, "my-skills-error-no-name"),
                )
                .await;
            }
        }
    } else {
        form.name.clone()
    };

    match store.save_manifest(&user.id, &target, &form.content) {
        Ok(name) => see_other(&format!("/skills?skill={name}")),
        Err(err) => {
            let msg = t_args(
                lang,
                "my-skills-error-save-failed",
                &i18n::args([("error", err.to_string().into())]),
            );
            let mode = if is_new { Mode::New } else { Mode::Edit };
            editor_error(
                &state,
                datastar,
                theme,
                lang,
                nav,
                &user,
                impersonating,
                mode,
                &target,
                &form.content,
                &msg,
            )
            .await
        }
    }
}

#[derive(serde::Deserialize)]
struct SaveForm {
    #[serde(default)]
    name: String,
    content: String,
}

/// POST /skills/delete — remove one of this user's private skills by `name`
/// (form field, not path — rama lowercases path segments). Redirects back.
pub async fn user_skills_delete(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (session, user) = require_session!(state, req);
    let _ = session;
    let Some(store) = state.user_skills() else {
        return see_other("/skills");
    };
    let (_, body) = req.into_parts();
    let Ok(body) = read_body_to_bytes(body).await else {
        return see_other("/skills");
    };
    let Ok(form) = serde_urlencoded::from_bytes::<DeleteForm>(&body) else {
        return see_other("/skills");
    };
    if let Err(err) = store.remove(&user.id, &form.name) {
        tracing::warn!(skill = %form.name, error = %err, "deleting private skill");
    }
    see_other("/skills")
}

#[derive(serde::Deserialize)]
struct DeleteForm {
    name: String,
}

/// GET /skills/download?skill=<name> — re-package one of this user's private
/// skills as a `.skill` archive.
pub async fn user_skills_download(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (_, user) = require_session!(state, req);
    let Some(name) = selected_skill_param(&req) else {
        return see_other("/skills");
    };
    let Some(store) = state.user_skills() else {
        return see_other("/skills");
    };
    let registry = store.registry_for(&user.id);
    let Some(skill) = registry.get(&name) else {
        return see_other("/skills");
    };
    match skill.to_archive() {
        Ok(bytes) => Response::builder()
            .status(rama::http::StatusCode::OK)
            .header(rama::http::header::CONTENT_TYPE, "application/zip")
            .header(rama::http::header::CONTENT_LENGTH, bytes.len())
            .header(
                rama::http::header::CONTENT_DISPOSITION,
                // `name` is validated to `[A-Za-z0-9._-]` on save, so it's
                // safe to interpolate into the filename unescaped.
                format!("attachment; filename=\"{name}.skill\""),
            )
            .header(rama::http::header::CACHE_CONTROL, "no-store")
            .body(bytes.into())
            .unwrap_or_else(|_| see_other("/skills")),
        Err(err) => {
            tracing::warn!(skill = %name, error = %err, "packaging private skill for download");
            see_other("/skills")
        }
    }
}

/// One of the user's private skills, flattened for rendering.
struct SkillView {
    name: String,
    title: String,
    description: String,
    files: Vec<String>,
    /// `SKILL.md` body (frontmatter stripped) rendered from GFM to HTML.
    body_html: String,
    /// The raw `SKILL.md` (frontmatter included) — the editor's starting text.
    manifest: String,
}

/// Flatten the user's private registry into render rows.
fn skill_views(store: &UserSkillStore, user_id: &str) -> Vec<SkillView> {
    let registry = store.registry_for(user_id);
    registry
        .iter()
        .map(|skill| {
            let body_html = match skill.body() {
                Ok(body) => session_core::render::render_markdown(&body),
                Err(err) => format!("<p><em>Could not read SKILL.md: {err}</em></p>"),
            };
            SkillView {
                name: skill.name.clone(),
                title: skill.title.clone(),
                description: skill.description.clone(),
                files: skill.files(),
                body_html,
                manifest: skill.manifest_text().unwrap_or_default(),
            }
        })
        .collect()
}

/// `?<key>=1` (or a bare `?<key>`) is present in the query.
fn has_flag(req: &Request, key: &str) -> bool {
    let Some(query) = req.uri().query() else {
        return false;
    };
    query.split('&').any(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        percent_decode(k) == key && (v.is_empty() || v == "1")
    })
}

/// Render helper for a mutation error that should drop the user back into the
/// editor with their in-progress text preserved (rather than a fresh page).
#[allow(clippy::too_many_arguments)]
async fn editor_error(
    state: &RamaState,
    datastar: bool,
    theme: Theme,
    lang: Lang,
    nav: NavSections,
    user: &User,
    impersonating: bool,
    mode: Mode,
    name: &str,
    content: &str,
    error: &str,
) -> Response {
    render_page(
        state,
        datastar,
        theme,
        lang,
        nav,
        user,
        impersonating,
        Some(name),
        mode,
        Some(content.to_string()),
        Some(error),
    )
    .await
}

/// Render helper for a plain page-level error (view mode).
#[allow(clippy::too_many_arguments)]
async fn err_page(
    state: &RamaState,
    datastar: bool,
    theme: Theme,
    lang: Lang,
    nav: NavSections,
    user: &User,
    impersonating: bool,
    error: &str,
) -> Response {
    render_page(
        state,
        datastar,
        theme,
        lang,
        nav,
        user,
        impersonating,
        None,
        Mode::View,
        None,
        Some(error),
    )
    .await
}

/// Shared renderer for the GET path and every post-mutation path.
#[allow(clippy::too_many_arguments)]
async fn render_page(
    state: &RamaState,
    datastar: bool,
    theme: Theme,
    lang: Lang,
    nav: NavSections,
    user: &User,
    impersonating: bool,
    selected_name: Option<&str>,
    mode: Mode,
    editor_prefill: Option<String>,
    error: Option<&str>,
) -> Response {
    let views = match state.user_skills() {
        Some(store) => skill_views(&store, &user.id),
        None => Vec::new(),
    };
    let configured = state.user_skills().is_some();
    let selected = views
        .iter()
        .position(|v| Some(v.name.as_str()) == selected_name);
    let push_url = match (mode, selected.and_then(|i| views.get(i))) {
        (Mode::New, _) => "/skills?new=1".to_string(),
        (Mode::Edit, Some(v)) => format!("/skills?skill={}&edit=1", v.name),
        (_, Some(v)) => format!("/skills?skill={}", v.name),
        _ => "/skills".to_string(),
    };
    let body = render_body(
        lang,
        &views,
        selected,
        configured,
        mode,
        editor_prefill,
        error,
    );
    let chat = fetch_sidebar_chat(state, &user.id, None).await;
    let title = t(lang, "my-skills-page-title");
    {
        let pctx = super::PageCtx {
            theme,
            lang,
            nav,
            datastar,
            user_email: user.email.clone(),
            is_admin: is_admin(state, user),
            skills_enabled: state.user_skills_enabled(),
            impersonating,
        };
        nav_or_html_page(&pctx, NavItem::MySkills, &title, body, &push_url, &chat)
    }
}

fn render_body(
    lang: Lang,
    skills: &[SkillView],
    selected: Option<usize>,
    configured: bool,
    mode: Mode,
    editor_prefill: Option<String>,
    error: Option<&str>,
) -> Html {
    // Build the two columns first: the `html!` block becomes an `Fn` closure,
    // so a non-`Copy` value (`editor_prefill`) can't be moved into a call
    // inside it — bind the rendered panes here and interpolate them instead.
    let rail = render_rail(lang, skills, selected, mode, configured);
    let detail = render_detail_pane(lang, skills, selected, configured, mode, editor_prefill);
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
            div(class: "flex items-center gap-2") {
                (icons::sparkles(20))
                h1(class: "text-2xl font-bold m-0") { (t(lang, "my-skills-heading")) }
            }
            p(class: "text-base-content/60 text-sm mt-1 mb-4") {
                (t(lang, "my-skills-intro"))
            }

            if let Some(error) = error {
                div(class: "alert alert-error text-sm mb-4") { (error) }
            }

            div(class: "flex gap-6 items-start") {
                (rail)
                (detail)
            }
        }
    }
    .to_html()
}

/// Left rail: "New skill", the upload control, and the user's skill list.
fn render_rail(
    lang: Lang,
    skills: &[SkillView],
    selected: Option<usize>,
    mode: Mode,
    configured: bool,
) -> Html {
    html! {
        aside(class: "w-60 shrink-0 sticky top-6 flex flex-col gap-3") {
            if configured {
                a(
                    href: "/skills?new=1",
                    "data-on:click__prevent": "@get('/skills?new=1')",
                    class: (rail_new_class(mode == Mode::New))
                ) {
                    (icons::sparkles(14)) " " (t(lang, "my-skills-new-button"))
                }
                form(
                    method: "post",
                    action: "/skills/upload",
                    enctype: "multipart/form-data",
                    class: "card border border-base-300"
                ) {
                    div(class: "card-body p-3 gap-2") {
                        div(class: "text-xs uppercase tracking-wide text-base-content/50") {
                            (t(lang, "my-skills-upload-heading"))
                        }
                        input(
                            type: "file",
                            name: "skill",
                            accept: ".skill,.zip",
                            required: "required",
                            class: "file-input file-input-sm file-input-bordered w-full"
                        );
                        button(type: "submit", class: "btn btn-sm btn-primary w-full") {
                            (t(lang, "my-skills-upload-button"))
                        }
                    }
                }
            }

            div(class: "card border border-base-300") {
                div(class: "card-body p-2") {
                    div(class: "px-2 py-1 text-xs uppercase tracking-wide text-base-content/50") {
                        (t(lang, "my-skills-loaded-heading"))
                    }
                    if skills.is_empty() {
                        div(class: "px-2 py-1 text-sm text-base-content/50") {
                            (t(lang, "my-skills-none-yet"))
                        }
                    }
                    ul(class: "flex flex-col") {
                        for (i, s) in skills.iter().enumerate() {
                            li {
                                a(
                                    href: (format!("/skills?skill={}", s.name)),
                                    "data-on:click__prevent": (format!("@get('/skills?skill={}')", s.name)),
                                    class: (rail_link_class(Some(i) == selected && mode != Mode::New))
                                ) {
                                    (&s.title)
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

fn rail_new_class(active: bool) -> String {
    let base = "btn btn-sm w-full gap-1";
    if active {
        format!("{base} btn-primary")
    } else {
        format!("{base} btn-outline")
    }
}

fn rail_link_class(active: bool) -> String {
    let base = "block px-2 py-1.5 text-sm rounded transition-colors cursor-pointer";
    if active {
        format!("{base} bg-base-300 text-base-content font-semibold")
    } else {
        format!("{base} text-base-content/70 hover:bg-base-200")
    }
}

/// The right column, dispatched by mode.
fn render_detail_pane(
    lang: Lang,
    skills: &[SkillView],
    selected: Option<usize>,
    configured: bool,
    mode: Mode,
    editor_prefill: Option<String>,
) -> Html {
    if !configured {
        return html! {
            section(class: "flex-1 min-w-0 text-base-content/60 text-sm pt-2") {
                (t(lang, "my-skills-empty-not-configured"))
            }
        }
        .to_html();
    }
    match mode {
        Mode::New => render_editor(
            lang,
            "",
            &editor_prefill.unwrap_or_else(|| NEW_SKILL_TEMPLATE.to_string()),
            true,
        ),
        Mode::Edit => {
            let sel = selected.and_then(|i| skills.get(i));
            match sel {
                Some(s) => {
                    let content = editor_prefill.unwrap_or_else(|| s.manifest.clone());
                    render_editor(lang, &s.name, &content, false)
                }
                None => render_editor(
                    lang,
                    "",
                    &editor_prefill.unwrap_or_else(|| NEW_SKILL_TEMPLATE.to_string()),
                    true,
                ),
            }
        }
        Mode::View => match selected.and_then(|i| skills.get(i)) {
            Some(s) => render_detail(lang, s),
            None if skills.is_empty() => html! {
                section(class: "flex-1 min-w-0 text-base-content/60 text-sm pt-2") {
                    (t(lang, "my-skills-empty-loaded"))
                }
            }
            .to_html(),
            None => render_detail(lang, &skills[0]),
        },
    }
}

/// View mode: metadata header (edit / download / delete) + rendered SKILL.md.
fn render_detail(lang: Lang, s: &SkillView) -> Html {
    let body_html = s.body_html.as_str();
    let files_count = t_args(
        lang,
        "my-skills-files-count",
        &i18n::args([("count", s.files.len().to_string().into())]),
    );
    html! {
        section(class: "flex-1 min-w-0") {
            div(class: "flex items-start justify-between gap-3") {
                h2(class: "text-xl font-semibold m-0") { (&s.title) }
                div(class: "flex items-center gap-1 shrink-0") {
                    a(
                        href: (format!("/skills?skill={}&edit=1", s.name)),
                        "data-on:click__prevent": (format!("@get('/skills?skill={}&edit=1')", s.name)),
                        class: "btn btn-sm btn-ghost"
                    ) {
                        (icons::sliders(14)) (t(lang, "my-skills-edit-button"))
                    }
                    a(
                        href: (format!("/skills/download?skill={}", s.name)),
                        class: "btn btn-sm btn-ghost",
                        title: (t(lang, "my-skills-download-title"))
                    ) {
                        (icons::download(14)) (t(lang, "my-skills-download-button"))
                    }
                    form(method: "post", action: "/skills/delete") {
                        input(type: "hidden", name: "name", value: (&s.name));
                        button(
                            type: "submit",
                            class: "btn btn-sm btn-ghost text-error",
                            title: (t(lang, "my-skills-delete-title"))
                        ) {
                            (icons::trash(14)) (t(lang, "my-skills-delete-button"))
                        }
                    }
                }
            }
            div(class: "flex flex-wrap items-center gap-x-6 gap-y-1 mt-1 text-xs text-base-content/50") {
                span(class: "font-mono") { (&s.name) }
                span { (files_count) }
            }
            div(class: "mt-4") {
                div(class: "text-xs uppercase tracking-wide text-base-content/50 mb-1") {
                    (t(lang, "my-skills-description-heading"))
                }
                p(class: "text-sm text-base-content/80 m-0") { (&s.description) }
            }
            div(class: "card border border-base-300 mt-5") {
                div(class: "card-body prose max-w-none") {
                    #(body_html)
                }
            }
        }
    }
    .to_html()
}

/// Edit / New mode: a raw `SKILL.md` textarea posting to `/skills/save`. For an
/// edit, the original slug rides along as a hidden `name` so a changed
/// frontmatter `name` is caught; for a new skill the field is empty and the
/// slug is taken from the frontmatter.
fn render_editor(lang: Lang, name: &str, content: &str, is_new: bool) -> Html {
    let heading = if is_new {
        t(lang, "my-skills-new-heading")
    } else {
        t(lang, "my-skills-edit-heading")
    };
    let cancel_href = if is_new {
        "/skills".to_string()
    } else {
        format!("/skills?skill={name}")
    };
    html! {
        section(class: "flex-1 min-w-0") {
            h2(class: "text-xl font-semibold m-0") { (heading) }
            p(class: "text-sm text-base-content/60 mt-1 mb-3") { (t(lang, "my-skills-editor-hint")) }
            form(method: "post", action: "/skills/save", class: "flex flex-col gap-3") {
                input(type: "hidden", name: "name", value: (name));
                textarea(
                    name: "content",
                    rows: "24",
                    spellcheck: "false",
                    required: "required",
                    class: "textarea textarea-bordered w-full font-mono text-sm leading-relaxed"
                ) { (content) }
                div(class: "flex items-center justify-end gap-2") {
                    a(href: (cancel_href), class: "btn btn-sm btn-ghost") {
                        (t(lang, "my-skills-cancel-button"))
                    }
                    button(type: "submit", class: "btn btn-sm btn-primary") {
                        (t(lang, "my-skills-save-button"))
                    }
                }
            }
        }
    }
    .to_html()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(name: &str) -> SkillView {
        SkillView {
            name: name.into(),
            title: "My Skill".into(),
            description: "does a thing".into(),
            files: vec![],
            body_html: "<p>body</p>".into(),
            manifest: "---\nname: mine\ndescription: d\n---\nbody".into(),
        }
    }

    /// The view pane's edit/download/delete controls must target the routes the
    /// router registers — pins the UI-directive↔endpoint contract so a rename
    /// can't silently break the buttons.
    #[test]
    fn detail_wires_controls_to_registered_endpoints() {
        let html = render_detail(Lang::En, &view("mine")).to_string();
        assert!(html.contains("action=\"/skills/delete\""));
        assert!(html.contains("/skills/download?skill=mine"));
        // `&` is HTML-escaped to `&amp;` inside attribute values.
        assert!(html.contains("/skills?skill=mine&amp;edit=1"));
        assert!(html.contains("name=\"name\""));
    }

    /// The editor form must POST to `/skills/save` with the two fields the save
    /// handler deserializes (`name`, `content`), and prefill the textarea.
    #[test]
    fn editor_posts_to_save_with_name_and_content() {
        let html = render_editor(Lang::En, "mine", "SKILL BODY HERE", false).to_string();
        assert!(html.contains("action=\"/skills/save\""));
        assert!(html.contains("name=\"name\""));
        assert!(html.contains("value=\"mine\""));
        assert!(html.contains("name=\"content\""));
        assert!(html.contains("SKILL BODY HERE"));
    }

    /// A new-skill editor carries an empty `name` (slug derived from
    /// frontmatter) and the starter template.
    #[test]
    fn new_editor_has_empty_name_and_template() {
        let html = render_editor(Lang::En, "", NEW_SKILL_TEMPLATE, true).to_string();
        assert!(html.contains("action=\"/skills/save\""));
        // Empty hidden name.
        assert!(html.contains("name=\"name\""));
        assert!(html.contains("name: my-skill"));
    }

    #[test]
    fn has_flag_detects_new_and_edit() {
        let mk = |q: &str| {
            Request::builder()
                .uri(format!("http://x/skills?{q}"))
                .body(rama::http::Body::empty())
                .unwrap()
        };
        assert!(has_flag(&mk("new=1"), "new"));
        assert!(has_flag(&mk("skill=a&edit=1"), "edit"));
        assert!(has_flag(&mk("new"), "new"));
        assert!(!has_flag(&mk("skill=a"), "edit"));
        assert!(!has_flag(&mk("edit=0"), "edit"));
    }
}
