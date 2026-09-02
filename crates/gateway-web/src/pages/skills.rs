// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/skills` — admin viewer + manager for the gateway's Agent Skills.
//!
//! Skills are operator config discovered from `[skills] dir`, but unlike
//! Typst templates they're managed live: an admin can **upload** a `.skill`
//! archive and **delete** an installed skill, and the change takes effect
//! immediately (the [`SkillStore`](gateway_features::server::skills::SkillStore) re-scans
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
use session_core::i18n::{self, Lang, t, t_args};
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
    let Some(store) = state.skills() else {
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

use gateway_core::server::db::skill_grants;
use gateway_core::server::db::users::User;
use gateway_runtime::rama_server::state::RamaState;

/// One loaded skill, flattened for rendering.
struct SkillView {
    name: String,
    description: String,
    /// Bundle-relative file paths (excludes `SKILL.md`), sorted.
    files: Vec<String>,
    /// Role ids granted this skill in the static `[[roles]].skills` config
    /// (incl. via `"*"`). Read-only in the UI — managed in `gateway.toml`.
    config_roles: Vec<String>,
    /// Role ids granted this skill via the UI overlay (`skill_role_grants`).
    /// These are what the grant dialog edits.
    granted_roles: Vec<String>,
    /// `SKILL.md` body (frontmatter stripped) rendered from GFM to HTML.
    body_html: String,
}

/// GET /admin/skills — master-detail viewer. `?skill=<name>` selects which
/// skill's detail to show; defaults to the first loaded skill.
pub async fn skills_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
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
        lang,
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
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let Some(store) = state.skills() else {
        return render_page(
            &state,
            datastar,
            theme,
            lang,
            nav,
            &user,
            session.impersonator_id.is_some(),
            None,
            Some(&t(lang, "skills-error-not-configured")),
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
                lang,
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
                lang,
                nav,
                &user,
                session.impersonator_id.is_some(),
                None,
                Some(&t(lang, "skills-error-no-file")),
            )
            .await;
        }
        Err(msg) => {
            return render_page(
                &state,
                datastar,
                theme,
                lang,
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
            let msg = t_args(
                lang,
                "skills-error-install-failed",
                &i18n::args([("error", err.to_string().into())]),
            );
            render_page(
                &state,
                datastar,
                theme,
                lang,
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
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let Some(store) = state.skills() else {
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
                lang,
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
            let msg = t_args(
                lang,
                "skills-error-bad-delete-request",
                &i18n::args([("error", err.to_string().into())]),
            );
            return render_page(
                &state,
                datastar,
                theme,
                lang,
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
        Ok(_) => {
            // Drop the skill's overlay grants too, so re-uploading a skill of
            // the same name later starts with a clean slate rather than
            // silently inheriting the old access.
            if let Err(e) = skill_grants::delete_skill(&state.db, &form.name).await {
                tracing::warn!(skill = %form.name, error = %e, "clearing grants on skill delete");
            }
            reload_skill_overlay(&state).await;
            see_other("/admin/skills")
        }
        Err(err) => {
            let msg = t_args(
                lang,
                "skills-error-delete-failed",
                &i18n::args([("error", err.to_string().into())]),
            );
            render_page(
                &state,
                datastar,
                theme,
                lang,
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

/// POST /admin/skills/grants — set which roles may use a skill (the editable
/// overlay on top of the static `[[roles]].skills` config). Body is a plain
/// form: `skill=<name>` plus one `role=<id>` per checked, editable role.
/// Persists to `skill_role_grants`, refreshes the live resolver overlay, and
/// redirects back to the skill. Admin-gated like the rest of the page.
pub async fn skills_grants_save(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    // Skills must be configured to grant them — mirrors the other mutations.
    if state.skills().is_none() {
        return see_other("/admin/skills");
    }
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(_) => return see_other("/admin/skills"),
    };
    let (skill, submitted) = parse_grant_form(&body);
    let Some(skill) = skill.filter(|s| !s.is_empty()) else {
        return see_other("/admin/skills");
    };

    // Only persist roles that (a) actually exist in config and (b) aren't
    // already granted by config — the config-granted checkboxes render
    // disabled, so they shouldn't post, but we filter defensively so a
    // hand-crafted request can't write a redundant or bogus row.
    let config = state.config();
    let config_granted: Vec<&str> = config
        .roles
        .iter()
        .filter(|r| r.skills.iter().any(|g| g == "*" || g == &skill))
        .map(|r| r.id.as_str())
        .collect();
    let roles: Vec<String> = submitted
        .into_iter()
        .filter(|role| {
            state.config().roles.iter().any(|r| &r.id == role)
                && !config_granted.iter().any(|c| c == role)
        })
        .collect();

    if let Err(e) = skill_grants::set_for_skill(&state.db, &skill, &roles).await {
        tracing::warn!(skill = %skill, error = %e, "saving skill grants");
    }
    reload_skill_overlay(&state).await;
    see_other(&format!("/admin/skills?skill={skill}"))
}

/// Parse the grant form body: the single `skill` field and every `role` value
/// (repeated keys → a list, which `serde_urlencoded` can't express). Empty
/// `role` values are dropped. Reuses [`percent_decode`] for `%XX`/`+`.
fn parse_grant_form(body: &[u8]) -> (Option<String>, Vec<String>) {
    let text = String::from_utf8_lossy(body);
    let mut skill = None;
    let mut roles = Vec::new();
    for pair in text.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        match k {
            "skill" => skill = Some(percent_decode(v)),
            "role" => {
                let v = percent_decode(v);
                if !v.is_empty() {
                    roles.push(v);
                }
            }
            _ => {}
        }
    }
    (skill, roles)
}

/// Re-seed the resolver's skill-grant overlay from the DB. Called after any
/// mutation (a grant edit, or a delete that cleaned up grants) so the live
/// authorization view matches what's persisted. A DB hiccup leaves the
/// previous overlay in place rather than blanking grants.
async fn reload_skill_overlay(state: &RamaState) {
    match skill_grants::all(&state.db).await {
        Ok(grants) => state.rbac.set_skill_grant_overlay(grants),
        Err(e) => tracing::warn!(error = %e, "reloading skill-grant overlay"),
    }
}

/// Pull the first `skill` multipart field's bytes out of the request body.
/// Returns `Ok(None)` when no (non-empty) file part was sent. Shared with the
/// per-user skills page ([`super::skills_user`]), which accepts the same
/// `.skill` upload shape.
pub(super) async fn read_upload_field(
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
/// Shared with the per-user skills page.
pub(super) fn selected_skill_param(req: &Request) -> Option<String> {
    let query = req.uri().query()?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "skill").then(|| percent_decode(v))
    })
}

pub(super) fn percent_decode(s: &str) -> String {
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
    lang: Lang,
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
    let dir = state.config().skills.as_ref().map(|c| display_dir(&c.dir));
    let push_url = match views.get(selected) {
        Some(v) => format!("/admin/skills?skill={}", v.name),
        None => "/admin/skills".to_string(),
    };
    let roles = all_role_ids(state);
    let dir_inaccessible = state.skills_dir_inaccessible();
    let body = render_body(
        lang,
        &views,
        selected,
        dir.as_deref(),
        dir_inaccessible,
        &roles,
        error,
    );
    let chat = fetch_sidebar_chat(state, &user.id, None).await;
    let title = t(lang, "skills-page-title");
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
        nav_or_html_page(&pctx, NavItem::Skills, &title, body, &push_url, &chat)
    }
}

/// Flatten the loaded registry into render rows. Each skill is paired with the
/// role ids that grant it, split by source: `config_roles` from the static
/// `[[roles]].skills` config (`"*"` or an exact name match — the same rule
/// [`gateway_core::server::rbac::Resolver::allowed_skills`] enforces) and
/// `granted_roles` from the live UI overlay. Also renders its `SKILL.md` body.
fn skill_views(state: &RamaState) -> Vec<SkillView> {
    let Some(store) = state.skills() else {
        return Vec::new();
    };
    let registry = store.current();
    registry
        .iter()
        .map(|skill| {
            let config_roles = state
                .config()
                .roles
                .iter()
                .filter(|r| r.skills.iter().any(|g| g == "*" || g == &skill.name))
                .map(|r| r.id.clone())
                .collect();
            let granted_roles = state.rbac.overlay_roles_for_skill(&skill.name);
            let body_html = match skill.body() {
                Ok(body) => session_core::render::render_markdown(&body),
                Err(err) => format!("<p><em>Could not read SKILL.md: {err}</em></p>"),
            };
            SkillView {
                name: skill.name.clone(),
                description: skill.description.clone(),
                files: skill.files(),
                config_roles,
                granted_roles,
                body_html,
            }
        })
        .collect()
}

/// Every role id defined in the gateway config, in declaration order — the
/// universe of checkboxes the grant dialog offers.
fn all_role_ids(state: &RamaState) -> Vec<String> {
    state.config().roles.iter().map(|r| r.id.clone()).collect()
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
    lang: Lang,
    skills: &[SkillView],
    selected: usize,
    dir: Option<&str>,
    // The skills dir is configured but not accessible (missing/permission
    // denied) — surface a prominent "no directory access" warning.
    dir_inaccessible: bool,
    all_roles: &[String],
    error: Option<&str>,
) -> Html {
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
            div(class: "flex items-center gap-2") {
                (icons::sliders(20))
                h1(class: "text-2xl font-bold m-0") { (t(lang, "skills-heading")) }
            }
            p(class: "text-base-content/60 text-sm mt-1 mb-4") {
                (t(lang, "skills-intro-part1")) " "
                code(class: "font-mono text-xs") { "read_skill" }
                " " (t(lang, "skills-intro-part2")) " "
                code(class: "font-mono text-xs") { ".skill" }
                " " (t(lang, "skills-intro-part3"))
            }

            if dir_inaccessible {
                div(class: "alert alert-warning text-sm mb-4") {
                    (icons::alert(16))
                    span {
                        (t(lang, "skills-error-no-dir-access"))
                        if let Some(dir) = dir {
                            " " span(class: "font-mono break-all") { (dir) }
                        }
                    }
                }
            }

            if let Some(error) = error {
                div(class: "alert alert-error text-sm mb-4") { (error) }
            }

            div(class: "flex gap-6 items-start") {
                (render_rail(lang, skills, selected, dir))
                if skills.is_empty() {
                    section(class: "flex-1 min-w-0 text-base-content/60 text-sm pt-2") {
                        if dir_inaccessible {
                            (t(lang, "skills-error-no-dir-access"))
                        } else if dir.is_some() {
                            (t(lang, "skills-empty-loaded"))
                        } else {
                            (t(lang, "skills-empty-not-configured"))
                        }
                    }
                } else {
                    (render_detail(lang, &skills[selected], all_roles))
                }
            }
        }
    }
    .to_html()
}

/// Left rail: upload control, every loaded skill, the selected one expanded
/// to its file tree.
fn render_rail(lang: Lang, skills: &[SkillView], selected: usize, dir: Option<&str>) -> Html {
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
                        (t(lang, "skills-upload-heading"))
                    }
                    input(
                        type: "file",
                        name: "skill",
                        accept: ".skill,.zip",
                        required: "required",
                        class: "file-input file-input-sm file-input-bordered w-full"
                    );
                    button(type: "submit", class: "btn btn-sm btn-primary w-full") {
                        (t(lang, "skills-upload-button"))
                    }
                }
            }

            div(class: "card border border-base-300") {
                div(class: "card-body p-2") {
                    div(class: "px-2 py-1 text-xs uppercase tracking-wide text-base-content/50") {
                        (t(lang, "skills-loaded-heading"))
                    }
                    if skills.is_empty() {
                        div(class: "px-2 py-1 text-sm text-base-content/50") { (t(lang, "skills-none-yet")) }
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
                            (t(lang, "skills-source-prefix")) " " (dir)
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

/// DOM id of the per-detail grant dialog. Only the selected skill's detail is
/// rendered, so a single fixed id is unique on the page; the open/close
/// buttons reference it.
const GRANT_DIALOG_ID: &str = "skill-grant-dialog";

/// Right pane: metadata header (+ delete), the editable "Granted to" control,
/// and the rendered SKILL.md.
fn render_detail(lang: Lang, s: &SkillView, all_roles: &[String]) -> Html {
    let body_html = s.body_html.as_str();
    let no_grants = s.config_roles.is_empty() && s.granted_roles.is_empty();
    let open_dialog = format!("document.getElementById('{GRANT_DIALOG_ID}').showModal()");
    let files_count = t_args(
        lang,
        "skills-files-count",
        &i18n::args([("count", s.files.len().to_string().into())]),
    );
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
                        title: (t(lang, "skills-download-title"))
                    ) {
                        (icons::download(14)) (t(lang, "skills-download-button"))
                    }
                    form(method: "post", action: "/admin/skills/delete") {
                        input(type: "hidden", name: "name", value: (&s.name));
                        button(
                            type: "submit",
                            class: "btn btn-sm btn-ghost text-error",
                            title: (t(lang, "skills-delete-title"))
                        ) {
                            (icons::trash(14)) (t(lang, "skills-delete-button"))
                        }
                    }
                }
            }

            div(class: "flex flex-wrap items-start gap-x-8 gap-y-2 mt-2") {
                div {
                    div(class: "text-xs uppercase tracking-wide text-base-content/50 mb-1") {
                        (t(lang, "skills-granted-to-heading"))
                    }
                    div(class: "flex flex-wrap items-center gap-1") {
                        // Config grants — read-only here (managed in gateway.toml).
                        for role in s.config_roles.iter() {
                            span(
                                class: "badge badge-sm badge-outline",
                                title: (t(lang, "skills-granted-config-title"))
                            ) { (role) }
                        }
                        // UI overlay grants — what the dialog edits.
                        for role in s.granted_roles.iter() {
                            span(class: "badge badge-sm badge-secondary") { (role) }
                        }
                        if no_grants {
                            button(
                                type: "button",
                                class: "badge badge-sm badge-warning badge-outline cursor-pointer",
                                title: (t(lang, "skills-choose-access-title")),
                                "data-on:click": (open_dialog.clone())
                            ) {
                                (t(lang, "skills-no-grants-warning"))
                            }
                        } else {
                            button(
                                type: "button",
                                class: "btn btn-xs btn-ghost gap-1",
                                title: (t(lang, "skills-edit-access-title")),
                                "data-on:click": (open_dialog.clone())
                            ) {
                                (icons::sliders(12)) (t(lang, "skills-edit-access-button"))
                            }
                        }
                    }
                }
                div {
                    div(class: "text-xs uppercase tracking-wide text-base-content/50 mb-1") {
                        (t(lang, "skills-files-heading"))
                    }
                    div(class: "text-sm text-base-content/80") {
                        (files_count)
                    }
                }
            }

            div(class: "mt-4") {
                div(class: "text-xs uppercase tracking-wide text-base-content/50 mb-1") {
                    (t(lang, "skills-description-heading"))
                }
                p(class: "text-sm text-base-content/80 m-0") { (&s.description) }
            }

            div(class: "card border border-base-300 mt-5") {
                div(class: "card-body prose max-w-none") {
                    // Rendered SKILL.md body — raw HTML from the GFM renderer.
                    #(body_html)
                }
            }

            (render_grant_dialog(lang, s, all_roles))
        }
    }
    .to_html()
}

/// The "who can use this skill" modal. A native `<dialog>` (the browser draws
/// the backdrop + centring) holding a plain POST form — submitting redirects
/// back, so it works the same way as the upload/delete forms. Each configured
/// role is a checkbox; roles granted by config show checked-and-disabled (they
/// can't be edited here), the rest reflect the live overlay grant.
fn render_grant_dialog(lang: Lang, s: &SkillView, all_roles: &[String]) -> Html {
    let close = format!("document.getElementById('{GRANT_DIALOG_ID}').close()");
    html! {
        dialog(
            id: (GRANT_DIALOG_ID),
            class: "rounded-xl border border-base-300",
            // Inline style: the native <dialog> needs explicit sizing and a
            // surface colour, and these knobs aren't worth a utility class.
            style: "padding:0; width:92vw; max-width:30rem; \
                    background:var(--color-base-100); color:var(--color-base-content);"
        ) {
            form(method: "post", action: "/admin/skills/grants") {
                input(type: "hidden", name: "skill", value: (&s.name));
                div(class: "p-4 flex flex-col gap-3") {
                    div {
                        h3(class: "text-base font-semibold m-0") { (t(lang, "skills-grant-dialog-heading")) }
                        p(class: "text-xs text-base-content/60 mt-1 mb-0") {
                            (t(lang, "skills-grant-dialog-desc-part1")) " "
                            span(class: "font-mono") { (&s.name) }
                            (t(lang, "skills-grant-dialog-desc-part2"))
                        }
                    }
                    div(class: "flex flex-col") {
                        if all_roles.is_empty() {
                            p(class: "text-sm text-base-content/60 m-0") {
                                (t(lang, "skills-grant-dialog-no-roles-part1")) " "
                                code(class: "font-mono text-xs") { "[[roles]]" }
                                " " (t(lang, "skills-grant-dialog-no-roles-part2"))
                            }
                        }
                        for role in all_roles.iter() {
                            (render_role_row(
                                lang,
                                role,
                                s.config_roles.iter().any(|r| r == role),
                                s.granted_roles.iter().any(|r| r == role),
                            ))
                        }
                    }
                    div(class: "flex items-center justify-end gap-2 pt-1") {
                        button(
                            type: "button",
                            class: "btn btn-sm btn-ghost",
                            "data-on:click": (close)
                        ) { (t(lang, "skills-cancel-button")) }
                        button(type: "submit", class: "btn btn-sm btn-primary") { (t(lang, "skills-save-access-button")) }
                    }
                }
            }
        }
    }
    .to_html()
}

/// One role checkbox in the grant dialog. Config-granted roles render
/// checked-and-disabled with a "config" marker (so they're visible but can't be
/// toggled — they're authoritative and live in TOML); editable roles carry
/// `name="role"` so they post back, pre-checked when the overlay already grants
/// them.
fn render_role_row(lang: Lang, role: &str, from_config: bool, granted: bool) -> Html {
    html! {
        label(class: "flex items-center gap-2 py-1.5 cursor-pointer") {
            if from_config {
                input(
                    type: "checkbox",
                    class: "checkbox checkbox-sm",
                    checked: "checked",
                    disabled: "disabled"
                );
            } else {
                (role_checkbox(role, granted))
            }
            span(class: "text-sm font-mono") { (role) }
            if from_config {
                span(class: "badge badge-sm badge-outline ml-auto") { (t(lang, "skills-from-config-badge")) }
            }
        }
    }
    .to_html()
}

/// The editable checkbox half of [`render_role_row`] — split out because the
/// `checked` attribute is present-or-absent, not a boolean value.
fn role_checkbox(role: &str, checked: bool) -> Html {
    if checked {
        html! {
            input(
                type: "checkbox",
                name: "role",
                value: (role),
                class: "checkbox checkbox-sm",
                checked: "checked"
            );
        }
        .to_html()
    } else {
        html! {
            input(
                type: "checkbox",
                name: "role",
                value: (role),
                class: "checkbox checkbox-sm"
            );
        }
        .to_html()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configured-but-inaccessible skills dir must surface the "no directory
    /// access" warning (the admin-facing half of hiding the feature). It shows
    /// even though the dir path is `Some(...)` — that's the "configured but
    /// broken" state, distinct from "not configured".
    #[test]
    fn inaccessible_dir_shows_the_no_access_warning() {
        let html = render_body(Lang::En, &[], 0, Some("data/skills"), true, &[], None).to_string();
        assert!(
            html.contains("No access to the skills directory"),
            "should surface the no-directory-access message"
        );
        // A healthy configured dir shows the ordinary empty state instead.
        let ok = render_body(Lang::En, &[], 0, Some("data/skills"), false, &[], None).to_string();
        assert!(!ok.contains("No access to the skills directory"));
    }

    fn view(config_roles: &[&str], granted_roles: &[&str]) -> SkillView {
        SkillView {
            name: "brand".into(),
            description: "house style".into(),
            files: vec![],
            config_roles: config_roles.iter().map(|s| (*s).to_string()).collect(),
            granted_roles: granted_roles.iter().map(|s| (*s).to_string()).collect(),
            body_html: String::new(),
        }
    }

    // ---- parse_grant_form ----

    #[test]
    fn parse_grant_form_collects_skill_and_repeated_roles() {
        let (skill, roles) = parse_grant_form(b"skill=brand&role=eng&role=qa");
        assert_eq!(skill.as_deref(), Some("brand"));
        assert_eq!(roles, vec!["eng".to_string(), "qa".to_string()]);
    }

    #[test]
    fn parse_grant_form_percent_decodes_and_drops_empty_roles() {
        // `+` → space, `%2D` → `-`, and an empty `role=` is ignored (the form
        // sends no value for unchecked boxes, but a stray one mustn't become "").
        let (skill, roles) = parse_grant_form(b"skill=my+skill&role=eng%2Dteam&role=");
        assert_eq!(skill.as_deref(), Some("my skill"));
        assert_eq!(roles, vec!["eng-team".to_string()]);
    }

    #[test]
    fn parse_grant_form_no_roles_clears() {
        let (skill, roles) = parse_grant_form(b"skill=brand");
        assert_eq!(skill.as_deref(), Some("brand"));
        assert!(roles.is_empty());
    }

    // ---- render wiring ----

    /// The open button, the dialog, and the close button must all reference
    /// the same element id, and the form must POST to the route the router
    /// registers (`/admin/skills/grants`). This pins the UI-directive↔endpoint
    /// contract so a rename can't silently break the dialog.
    #[test]
    fn detail_wires_dialog_open_to_the_grant_form_endpoint() {
        let html =
            render_detail(Lang::En, &view(&[], &[]), &["eng".into(), "admin".into()]).to_string();
        // The dialog exists with the canonical id.
        assert!(html.contains(&format!("id=\"{GRANT_DIALOG_ID}\"")));
        // The id appears three times — on the dialog, in the open directive,
        // and in the close directive — so all three reference the same element.
        // (Attribute values are HTML-escaped, so we count the id rather than
        // matching the full `getElementById('…')` expression verbatim.)
        assert_eq!(
            html.matches(GRANT_DIALOG_ID).count(),
            3,
            "dialog id should be wired to exactly the open + close directives"
        );
        assert!(html.contains("showModal()"));
        assert!(html.contains("close()"));
        // The form posts to the route the router registers.
        assert!(html.contains("action=\"/admin/skills/grants\""));
        // The skill name rides along as a hidden field.
        assert!(html.contains("name=\"skill\""));
    }

    #[test]
    fn detail_with_no_grants_shows_the_clickable_warning() {
        let html = render_detail(Lang::En, &view(&[], &[]), &["eng".into()]).to_string();
        assert!(html.contains("no role grants this"));
        // Both checkboxes for editable roles carry name="role".
        assert!(html.contains("name=\"role\""));
    }

    #[test]
    fn detail_marks_config_roles_readonly_and_overlay_roles_editable() {
        // `admin` is granted in config (read-only), `eng` via the overlay
        // (editable, pre-checked), `qa` ungranted (editable, unchecked).
        let html = render_detail(
            Lang::En,
            &view(&["admin"], &["eng"]),
            &["admin", "eng", "qa"]
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
        )
        .to_string();
        // Config role shows the read-only marker and a disabled checkbox.
        assert!(html.contains("from config"));
        assert!(html.contains("disabled"));
        // The "Edit access" affordance replaces the warning once something is granted.
        assert!(html.contains("Edit access"));
        assert!(!html.contains("no role grants this"));
        // The granted overlay role renders a pre-checked editable box.
        assert!(html.contains("value=\"eng\""));
        assert!(html.contains("value=\"qa\""));
    }
}
