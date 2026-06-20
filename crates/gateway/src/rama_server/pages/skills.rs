// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/skills` — admin viewer + manager for the gateway's Agent Skills.
//!
//! Skills are operator config discovered from `[skills] dir`, but unlike
//! Typst templates they're managed live: an admin can **upload** a `.skill`
//! archive and **delete** an installed skill, and the change takes effect
//! immediately (the [`SkillStore`](crate::server::skills::SkillStore) re-scans
//! and atomically swaps the registry — no gateway restart).
//!
//! It's a master-detail viewer: a left rail lists the loaded skills with the
//! selected bundle's file tree (and the upload control); the detail pane
//! renders that skill's `SKILL.md` (the chat's GFM renderer) plus who it's
//! granted to and a delete button. Selecting a skill is a `?skill=<name>`
//! Datastar nav (a plain link too, so it works without JS). Mutations are
//! plain form POSTs that redirect back. Admin-gated, like `/rag`.

use std::collections::BTreeMap;
use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};

use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_admin_or_403};
use session_core::chrome::{
    NavSections, Theme, is_datastar_request, read_body_to_bytes, see_other,
};
use session_core::icons;

/// GET /admin/skills/download?skill=<name> — re-package an installed skill as
/// a `.skill` archive so an admin can retrieve what was uploaded (or grab a
/// skill that was dropped in on disk). Admin-gated like the rest of the page.
pub async fn skills_download(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let Some(name) = selected_skill_param(&req) else {
        return see_other("/admin/skills");
    };
    let Some(store) = state.skills.clone() else {
        return see_other("/admin/skills");
    };
    let registry = store.current();
    let Some(skill) = registry.get(&name) else {
        return see_other("/admin/skills");
    };
    match skill.to_archive() {
        Ok(bytes) => Response::builder()
            .status(rama::http::StatusCode::OK)
            .header(rama::http::header::CONTENT_TYPE, "application/zip")
            .header(rama::http::header::CONTENT_LENGTH, bytes.len())
            .header(
                rama::http::header::CONTENT_DISPOSITION,
                // `name` is validated to `[A-Za-z0-9._-]` on install, so it's
                // safe to interpolate into the filename unescaped.
                format!("attachment; filename=\"{name}.skill\""),
            )
            .header(rama::http::header::CACHE_CONTROL, "no-store")
            .body(bytes.into())
            .unwrap_or_else(|_| see_other("/admin/skills")),
        Err(err) => {
            tracing::warn!(skill = %name, error = %err, "packaging skill for download");
            see_other("/admin/skills")
        }
    }
}

use crate::rama_server::state::RamaState;
use crate::server::db::users::User;

/// One loaded skill, flattened for rendering.
struct SkillView {
    name: String,
    description: String,
    /// Bundle-relative file paths (excludes `SKILL.md`), sorted.
    files: Vec<String>,
    /// Role ids whose `skills` grant covers this skill.
    roles: Vec<String>,
    /// `SKILL.md` body (frontmatter stripped) rendered from GFM to HTML.
    body_html: String,
}

/// GET /admin/skills — master-detail viewer. `?skill=<name>` selects which
/// skill's detail to show; defaults to the first loaded skill.
pub async fn skills_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let selected = selected_skill_param(&req);
    render_page(
        &state,
        datastar,
        theme,
        nav,
        &user,
        session.impersonator_id.is_some(),
        selected.as_deref(),
        None,
    )
    .await
}

/// POST /admin/skills/upload — accept a `.skill` archive (multipart field
/// `skill`), install it live, and redirect to the new skill. On a bad upload
/// re-renders the page with an inline error.
pub async fn skills_upload(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let Some(store) = state.skills.clone() else {
        return render_page(
            &state,
            datastar,
            theme,
            nav,
            &user,
            session.impersonator_id.is_some(),
            None,
            Some("Skills aren't configured ([skills] dir is unset)."),
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
            return render_page(
                &state,
                datastar,
                theme,
                nav,
                &user,
                session.impersonator_id.is_some(),
                None,
                Some(&msg),
            )
            .await;
        }
    };
    let bytes = match read_upload_field(&content_type, body).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            return render_page(
                &state,
                datastar,
                theme,
                nav,
                &user,
                session.impersonator_id.is_some(),
                None,
                Some("No file was uploaded — pick a .skill archive."),
            )
            .await;
        }
        Err(msg) => {
            return render_page(
                &state,
                datastar,
                theme,
                nav,
                &user,
                session.impersonator_id.is_some(),
                None,
                Some(&msg),
            )
            .await;
        }
    };

    match store.install_archive(&bytes) {
        Ok(name) => see_other(&format!("/admin/skills?skill={name}")),
        Err(err) => {
            let msg = format!("Could not install skill: {err}");
            render_page(
                &state,
                datastar,
                theme,
                nav,
                &user,
                session.impersonator_id.is_some(),
                None,
                Some(&msg),
            )
            .await
        }
    }
}

/// POST /admin/skills/delete — remove an installed skill by `name` (in the
/// form body, not the path, since rama lowercases path segments). Redirects
/// back to the list.
pub async fn skills_delete(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let Some(store) = state.skills.clone() else {
        return see_other("/admin/skills");
    };
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => {
            return render_page(
                &state,
                datastar,
                theme,
                nav,
                &user,
                session.impersonator_id.is_some(),
                None,
                Some(&msg),
            )
            .await;
        }
    };
    let form: DeleteForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => {
            let msg = format!("Bad delete request: {err}");
            return render_page(
                &state,
                datastar,
                theme,
                nav,
                &user,
                session.impersonator_id.is_some(),
                None,
                Some(&msg),
            )
            .await;
        }
    };
    match store.remove(&form.name) {
        Ok(_) => see_other("/admin/skills"),
        Err(err) => {
            let msg = format!("Could not delete skill: {err}");
            render_page(
                &state,
                datastar,
                theme,
                nav,
                &user,
                session.impersonator_id.is_some(),
                None,
                Some(&msg),
            )
            .await
        }
    }
}

#[derive(serde::Deserialize)]
struct DeleteForm {
    name: String,
}

/// Pull the first `skill` multipart field's bytes out of the request body.
/// Returns `Ok(None)` when no (non-empty) file part was sent.
async fn read_upload_field(
    content_type: &str,
    body: rama::bytes::Bytes,
) -> Result<Option<Vec<u8>>, String> {
    let boundary = multer::parse_boundary(content_type)
        .map_err(|err| format!("expected a multipart/form-data upload (form enctype): {err}"))?;
    let stream =
        rama::futures::stream::once(async move { Ok::<_, std::convert::Infallible>(body) });
    let mut mp = multer::Multipart::new(stream, boundary);
    while let Some(field) = mp.next_field().await.map_err(|e| e.to_string())? {
        if field.name() == Some("skill") {
            let bytes = field.bytes().await.map_err(|e| e.to_string())?.to_vec();
            if bytes.is_empty() {
                return Ok(None);
            }
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

/// Pull `?skill=<name>` out of the request URI, percent-decoding `%XX`/`+`.
fn selected_skill_param(req: &Request) -> Option<String> {
    let query = req.uri().query()?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "skill").then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let replaced = s.replace('+', " ");
    let bytes = replaced.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Shared renderer for the GET path and the post-mutation error paths.
#[allow(clippy::too_many_arguments)]
async fn render_page(
    state: &RamaState,
    datastar: bool,
    theme: Theme,
    nav: NavSections,
    user: &User,
    impersonating: bool,
    selected_name: Option<&str>,
    error: Option<&str>,
) -> Response {
    let views = skill_views(state);
    let selected = views
        .iter()
        .position(|v| Some(v.name.as_str()) == selected_name)
        .unwrap_or(0);
    let dir = state.config.skills.as_ref().map(|c| display_dir(&c.dir));
    let push_url = match views.get(selected) {
        Some(v) => format!("/admin/skills?skill={}", v.name),
        None => "/admin/skills".to_string(),
    };
    let body = render_body(&views, selected, dir.as_deref(), error);
    let chat = fetch_sidebar_chat(state, &user.id, None).await;
    nav_or_html_page(
        datastar,
        theme,
        nav,
        NavItem::Skills,
        "Skills — LLM Gateway",
        &user.email,
        is_admin(state, user),
        impersonating,
        body,
        &push_url,
        &chat,
    )
}

/// Flatten the loaded registry into render rows, pairing each skill with the
/// role ids whose `skills` grant covers it (`"*"` or an exact name match) —
/// the same rule [`crate::server::rbac::Resolver::allowed_skills`] enforces —
/// and rendering its `SKILL.md` body to HTML.
fn skill_views(state: &RamaState) -> Vec<SkillView> {
    let Some(store) = state.skills.as_ref() else {
        return Vec::new();
    };
    let registry = store.current();
    registry
        .iter()
        .map(|skill| {
            let roles = state
                .config
                .roles
                .iter()
                .filter(|r| r.skills.iter().any(|g| g == "*" || g == &skill.name))
                .map(|r| r.id.clone())
                .collect();
            let body_html = match skill.body() {
                Ok(body) => session_core::render::render_markdown(&body),
                Err(err) => format!("<p><em>Could not read SKILL.md: {err}</em></p>"),
            };
            SkillView {
                name: skill.name.clone(),
                description: skill.description.clone(),
                files: skill.files(),
                roles,
                body_html,
            }
        })
        .collect()
}

/// Show the skills directory relative to the gateway's working directory when
/// it lives under it (dev shows `data/skills`), else the configured path.
fn display_dir(dir: &std::path::Path) -> String {
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(rel) = dir.strip_prefix(&cwd)
    {
        return rel.display().to_string();
    }
    dir.display().to_string()
}

fn render_body(
    skills: &[SkillView],
    selected: usize,
    dir: Option<&str>,
    error: Option<&str>,
) -> Html {
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
            div(class: "flex items-center gap-2") {
                (icons::sliders(20))
                h1(class: "text-2xl font-bold m-0") { "Skills" }
            }
            p(class: "text-base-content/60 text-sm mt-1 mb-4") {
                "Operator-installed guidance the chat model loads on demand via the "
                code(class: "font-mono text-xs") { "read_skill" }
                " tool. Upload a "
                code(class: "font-mono text-xs") { ".skill" }
                " archive below — it's available immediately, no restart."
            }

            if let Some(error) = error {
                div(class: "alert alert-error text-sm mb-4") { (error) }
            }

            div(class: "flex gap-6 items-start") {
                (render_rail(skills, selected, dir))
                if skills.is_empty() {
                    section(class: "flex-1 min-w-0 text-base-content/60 text-sm pt-2") {
                        if dir.is_some() {
                            "No skills loaded yet. Upload a .skill archive to add one."
                        } else {
                            "Skills aren't configured. Set [skills] dir in the gateway config \
                             and restart to enable them."
                        }
                    }
                } else {
                    (render_detail(&skills[selected]))
                }
            }
        }
    }
    .to_html()
}

/// Left rail: upload control, every loaded skill, the selected one expanded
/// to its file tree.
fn render_rail(skills: &[SkillView], selected: usize, dir: Option<&str>) -> Html {
    html! {
        aside(class: "w-60 shrink-0 sticky top-6 flex flex-col gap-3") {
            form(
                method: "post",
                action: "/admin/skills/upload",
                enctype: "multipart/form-data",
                class: "card border border-base-300"
            ) {
                div(class: "card-body p-3 gap-2") {
                    div(class: "text-xs uppercase tracking-wide text-base-content/50") {
                        "Add a skill"
                    }
                    input(
                        type: "file",
                        name: "skill",
                        accept: ".skill,.zip",
                        required: "required",
                        class: "file-input file-input-sm file-input-bordered w-full"
                    );
                    button(type: "submit", class: "btn btn-sm btn-primary w-full") {
                        "Upload .skill"
                    }
                }
            }

            div(class: "card border border-base-300") {
                div(class: "card-body p-2") {
                    div(class: "px-2 py-1 text-xs uppercase tracking-wide text-base-content/50") {
                        "Loaded skills"
                    }
                    if skills.is_empty() {
                        div(class: "px-2 py-1 text-sm text-base-content/50") { "None yet" }
                    }
                    ul(class: "flex flex-col") {
                        for (i, s) in skills.iter().enumerate() {
                            li {
                                a(
                                    href: (format!("/admin/skills?skill={}", s.name)),
                                    "data-on:click__prevent": (format!("@get('/admin/skills?skill={}')", s.name)),
                                    class: (rail_link_class(i == selected))
                                ) {
                                    (&s.name)
                                }
                                if i == selected {
                                    (render_file_tree(s))
                                }
                            }
                        }
                    }
                    if let Some(dir) = dir {
                        div(class: "px-2 pt-2 mt-1 border-t border-base-300 text-xs text-base-content/40 break-all") {
                            "Source: " (dir)
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

fn rail_link_class(active: bool) -> String {
    let base = "block px-2 py-1.5 text-sm font-mono rounded transition-colors cursor-pointer";
    if active {
        format!("{base} bg-base-300 text-base-content font-semibold")
    } else {
        format!("{base} text-base-content/70 hover:bg-base-200")
    }
}

/// The bundle's files as a small tree: `SKILL.md` first, then each
/// subdirectory with its files, then any root-level extras.
fn render_file_tree(s: &SkillView) -> Html {
    let mut dirs: BTreeMap<Option<&str>, Vec<&str>> = BTreeMap::new();
    for path in &s.files {
        match path.split_once('/') {
            Some((dir, rest)) => dirs.entry(Some(dir)).or_default().push(rest),
            None => dirs.entry(None).or_default().push(path),
        }
    }
    html! {
        div(class: "pl-3 pr-1 pb-1 text-xs text-base-content/60") {
            div(class: "flex items-center gap-1 py-0.5") {
                (icons::folder(12)) span(class: "font-mono") { "SKILL.md" }
            }
            for (dir, files) in dirs.iter() {
                if let Some(dir) = dir {
                    div(class: "flex items-center gap-1 py-0.5 text-base-content/50") {
                        (icons::folder(12)) span(class: "font-mono") { (dir) "/" }
                    }
                    for f in files.iter() {
                        div(class: "pl-4 py-0.5 font-mono truncate") { (f) }
                    }
                } else {
                    for f in files.iter() {
                        div(class: "py-0.5 font-mono truncate") { (f) }
                    }
                }
            }
        }
    }
    .to_html()
}

/// Right pane: metadata header (+ delete) and the rendered SKILL.md.
fn render_detail(s: &SkillView) -> Html {
    let body_html = s.body_html.as_str();
    html! {
        section(class: "flex-1 min-w-0") {
            div(class: "flex items-start justify-between gap-3") {
                h2(class: "text-xl font-mono font-semibold m-0") { (&s.name) }
                div(class: "flex items-center gap-1 shrink-0") {
                    // Plain GET download (no datastar) so the browser saves the
                    // returned .skill archive instead of swallowing it into an
                    // SPA-nav patch.
                    a(
                        href: (format!("/admin/skills/download?skill={}", s.name)),
                        class: "btn btn-sm btn-ghost",
                        title: "Download this skill as a .skill archive"
                    ) {
                        (icons::download(14)) "Download"
                    }
                    form(method: "post", action: "/admin/skills/delete") {
                        input(type: "hidden", name: "name", value: (&s.name));
                        button(
                            type: "submit",
                            class: "btn btn-sm btn-ghost text-error",
                            title: "Remove this skill"
                        ) {
                            (icons::trash(14)) "Delete"
                        }
                    }
                }
            }

            div(class: "flex flex-wrap items-start gap-x-8 gap-y-2 mt-2") {
                div {
                    div(class: "text-xs uppercase tracking-wide text-base-content/50 mb-1") {
                        "Granted to"
                    }
                    div(class: "flex flex-wrap gap-1") {
                        for role in s.roles.iter() {
                            span(class: "badge badge-sm badge-outline") { (role) }
                        }
                        if s.roles.is_empty() {
                            span(class: "badge badge-sm badge-warning badge-outline") {
                                "no role grants this"
                            }
                        }
                    }
                }
                div {
                    div(class: "text-xs uppercase tracking-wide text-base-content/50 mb-1") {
                        "Files"
                    }
                    div(class: "text-sm text-base-content/80") {
                        (format!("{} bundled", s.files.len()))
                    }
                }
            }

            div(class: "mt-4") {
                div(class: "text-xs uppercase tracking-wide text-base-content/50 mb-1") {
                    "Description"
                }
                p(class: "text-sm text-base-content/80 m-0") { (&s.description) }
            }

            div(class: "card border border-base-300 mt-5") {
                div(class: "card-body prose max-w-none") {
                    // Rendered SKILL.md body — raw HTML from the GFM renderer.
                    #(body_html)
                }
            }
        }
    }
    .to_html()
}
