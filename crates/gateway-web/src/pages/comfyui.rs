// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/admin/comfyui` — operator viewer + reload trigger for the headless
//! ComfyUI workflow catalog.
//!
//! Lists the currently-loaded workflows with their parameter surfaces, the
//! `[comfyui]` operator config (base URL + content_dir), and a "Reload
//! catalog" button. Mutations are a plain form POST that hits
//! `POST /api/v0/comfyui/reload` and re-renders the page with the new
//! snapshot's `ReloadReport` (loaded ids, skipped sources, reasons).
//! Admin-gated like `/admin/skills`.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Request, Response};
use session_core::chrome::{Theme, is_datastar_request, see_other};
use session_core::i18n::Lang;
use session_core::icons;

use super::{NavItem, fetch_sidebar_chat, is_admin, nav_or_html_page, require_admin_or_403};
use gateway_core::server::db::users::User;
use gateway_features::server::comfyui::{ComfyuiStore, ReloadReport, WorkflowManifest};
use gateway_runtime::rama_server::state::RamaState;

/// GET /admin/comfyui — render the current catalog snapshot.
pub async fn comfyui_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = session_core::chrome::NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = match require_admin_or_403(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let flash: Option<String> = req
        .headers()
        .get("x-comfyui-flash")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    render_page(
        &state,
        datastar,
        theme,
        lang,
        nav,
        &user,
        session.impersonator_id.is_some(),
        flash.as_deref(),
    )
    .await
}

/// POST /admin/comfyui/reload — re-scan `[comfyui] content_dir` and redirect
/// back to the index, surfacing the reload report through a header the index
/// renders as an inline banner. Bounces anonymous / non-admin callers back
/// to `/auth/login` / 403 — same gate as the index.
pub async fn comfyui_reload(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_admin_or_403(&state, &req).await {
        return resp;
    }
    let Some(handle) = state.comfyui() else {
        return see_other("/admin/comfyui");
    };
    // The rescan is synchronous disk I/O + parsing — run it off the async
    // worker thread.
    let store = handle.store.clone();
    let report: ReloadReport = match tokio::task::spawn_blocking(move || store.reload()).await {
        Ok(report) => report,
        Err(err) => {
            tracing::warn!(error = %err, "comfyui reload task panicked");
            return see_other("/admin/comfyui");
        }
    };
    tracing::info!(
        loaded = report.total,
        skipped = report.skipped.len(),
        "comfyui catalog reloaded via admin UI",
    );
    // Encode a short flash into the redirect via a synthetic header the
    // index reads on next render. Keeps the page stateless (no session
    // flash storage) while still surfacing the outcome.
    let flash = format_flash(&report);
    Response::builder()
        .status(rama::http::StatusCode::SEE_OTHER)
        .header(rama::http::header::LOCATION, "/admin/comfyui")
        .header("x-comfyui-flash", flash)
        .body(rama::http::Body::empty())
        .unwrap_or_else(|_| see_other("/admin/comfyui"))
}

fn format_flash(report: &ReloadReport) -> String {
    if report.skipped.is_empty() {
        format!(
            "Catalog reloaded — {} workflow{} loaded.",
            report.total,
            if report.total == 1 { "" } else { "s" }
        )
    } else {
        let reasons: Vec<String> = report
            .skipped
            .iter()
            .map(|s| format!("{} ({})", s.reason, short_source(&s.source)))
            .collect();
        format!(
            "Catalog reloaded — {} workflow{} loaded, {} skipped: {}",
            report.total,
            if report.total == 1 { "" } else { "s" },
            report.skipped.len(),
            reasons.join("; ")
        )
    }
}

fn short_source(source: &str) -> String {
    std::path::Path::new(source)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(source)
        .to_string()
}

#[allow(clippy::too_many_arguments)]
async fn render_page(
    state: &RamaState,
    datastar: bool,
    theme: Theme,
    lang: Lang,
    nav: session_core::chrome::NavSections,
    user: &User,
    impersonating: bool,
    flash: Option<&str>,
) -> Response {
    let jobs = gateway_features::server::comfyui::jobs::recent(&state.db, 20)
        .await
        .unwrap_or_default();
    let body = render_body(lang, state.comfyui().as_deref(), flash, &jobs);
    let chat = fetch_sidebar_chat(state, &user.id, None).await;
    let title = "ComfyUI — Workflow catalog";
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
        nav_or_html_page(
            &pctx,
            NavItem::Comfyui,
            title,
            body,
            "/admin/comfyui",
            &chat,
        )
    }
}

fn render_body(
    _lang: Lang,
    handle: Option<&gateway_runtime::server::comfyui_tool::ComfyuiHandle>,
    flash: Option<&str>,
    jobs: &[gateway_features::server::comfyui::jobs::ComfyuiJob],
) -> Html {
    let header: Html = html! {
        div(class: "flex items-center justify-between mb-6") {
            div {
                h1(class: "text-2xl font-semibold") { "ComfyUI workflow catalog" }
                p(class: "text-base-content/60 mt-1") {
                    "Headless ComfyUI worker. The gateway exposes each loaded \
                     workflow as a comfyui_<id> tool the model can call. Users \
                     never see ComfyUI itself."
                }
            }
            (reload_button(handle.is_some()))
        }
    }
    .to_html();
    let flash_card: Option<Html> = flash.map(|msg| {
        let is_error = msg.to_ascii_lowercase().contains("skip");
        let alert_cls = if is_error {
            "alert-warning"
        } else {
            "alert-success"
        };
        html! {
            div(class: format!("alert {alert_cls} mb-6")) {
                (icons::sparkles(20))
                span { (msg) }
            }
        }
        .to_html()
    });
    let body: Html = match handle {
        None => html! {
            div(class: "card border border-base-300 mb-6") {
                div(class: "card-body") {
                    h2(class: "card-title text-base") { "Not configured" }
                    p(class: "text-base-content/70") {
                        "Enable ComfyUI at /admin/settings — set its base URL and workflow \
                         directory — then restart the gateway to load the workflow catalog."
                    }
                }
            }
        }
        .to_html(),
        Some(h) => {
            let store: &ComfyuiStore = &h.store;
            let snapshot = store.current();
            let workflows = snapshot.workflows();
            let config_card: Html = html! {
                div(class: "card border border-base-300 mb-6") {
                    div(class: "card-body") {
                        h2(class: "card-title text-base") { "Operator configuration" }
                        div(class: "grid grid-cols-1 md:grid-cols-2 gap-4 text-sm") {
                            div {
                                div(class: "text-base-content/60") { "Worker base URL" }
                                div(class: "font-mono break-all") { (h.client.base_url()) }
                            }
                            div {
                                div(class: "text-base-content/60") { "Content directory" }
                                div(class: "font-mono break-all") { (store.dir().display().to_string()) }
                            }
                            div {
                                div(class: "text-base-content/60") { "Workflow timeout" }
                                div { (format!("{} s", h.runner_timeout.as_secs())) }
                            }
                            div {
                                div(class: "text-base-content/60") { "Queue poll interval" }
                                div { (format!("{} ms", h.runner_poll_interval.as_millis())) }
                            }
                        }
                        p(class: "text-base-content/60 mt-4 text-xs") {
                            "The content directory is operator-managed and not \
                             part of the public repository. Edit manifests there \
                             and click Reload above to apply changes — no \
                             restart needed."
                        }
                    }
                }
            }
            .to_html();
            let workflows_card: Html = if workflows.is_empty() {
                html! {
                    div(class: "card border border-base-300") {
                        div(class: "card-body") {
                            h2(class: "card-title text-base") { "Loaded workflows" }
                            p(class: "text-base-content/70") {
                                "No workflows loaded. Drop one subdirectory per \
                                 workflow into the content directory (each holding \
                                 manifest.toml + workflow.json) and click Reload \
                                 above. See docs/comfyui.md for the manifest format."
                            }
                        }
                    }
                }
                .to_html()
            } else {
                let rows: Vec<Html> = workflows.iter().map(|m| render_workflow_row(m)).collect();
                html! {
                    div(class: "card border border-base-300") {
                        div(class: "card-body") {
                            h2(class: "card-title text-base") {
                                "Loaded workflows"
                                span(class: "badge badge-outline ml-2") {
                                    (format!("{}", workflows.len()))
                                }
                            }
                            div(class: "flex flex-col divide-y divide-base-300") {
                                for row in rows.iter() {
                                    (row)
                                }
                            }
                        }
                    }
                }
                .to_html()
            };
            html! { (config_card) (workflows_card) (jobs_card(jobs)) }.to_html()
        }
    };
    html! {
        div(class: "p-6 max-w-5xl mx-auto") {
            (header)
            (flash_card)
            (body)
        }
    }
    .to_html()
}

fn reload_button(enabled: bool) -> Html {
    if !enabled {
        return html! {
            span(class: "btn btn-disabled btn-sm") {
                (icons::sparkles(16))
                "Reload catalog"
            }
        }
        .to_html();
    }
    html! {
        form(action: "/admin/comfyui/reload", method: "post") {
            button(type: "submit", class: "btn btn-primary btn-sm") {
                (icons::sparkles(16))
                "Reload catalog"
            }
        }
    }
    .to_html()
}

fn render_workflow_row(m: &WorkflowManifest) -> Html {
    let tool_id = format!("comfyui_{}", m.id);
    let kind_label = m.output_kind.to_string();
    let params: Vec<Html> = m
        .params
        .iter()
        .map(|p| {
            let req_badge: Html = if p.required {
                html! { span(class: "badge badge-xs badge-primary ml-1") { "required" } }.to_html()
            } else {
                plait::Html::new_unchecked(String::new())
            };
            html! {
                li(class: "text-sm") {
                    span(class: "font-mono text-base-content/80") { (p.key) }
                    (req_badge)
                    " — "
                    span(class: "text-base-content/70") { (p.description) }
                }
            }
            .to_html()
        })
        .collect();
    let params_section: Html = if params.is_empty() {
        plait::Html::new_unchecked(String::new())
    } else {
        html! {
            div(class: "mt-3") {
                div(class: "text-xs uppercase tracking-wide text-base-content/60 mb-1") {
                    "Parameters"
                }
                ul(class: "space-y-1") {
                    for p in params.iter() {
                        (p)
                    }
                }
            }
        }
        .to_html()
    };
    html! {
        div(class: "py-4") {
            div(class: "flex flex-wrap items-baseline gap-2") {
                span(class: "font-mono text-sm font-semibold") { (tool_id) }
                span(class: "badge badge-outline badge-sm") { (kind_label) }
                span(class: "text-base-content/60 text-sm") {
                    "node " (m.output_node_id)
                }
            }
            p(class: "mt-1 text-base-content/80") { (m.description) }
            div(class: "mt-2 text-xs text-base-content/60") {
                "Title: " (m.title) " · prefix: " (m.output_filename_prefix)
            }
            (params_section)
        }
    }
    .to_html()
}

fn jobs_card(jobs: &[gateway_features::server::comfyui::jobs::ComfyuiJob]) -> Html {
    if jobs.is_empty() {
        return plait::Html::new_unchecked(String::new());
    }
    let pending_count = jobs.iter().filter(|j| j.status == "pending").count();
    let rows: Vec<Html> = jobs
        .iter()
        .map(|j| {
            let badge_cls = match j.status.as_str() {
                "pending" => "badge-warning",
                "completed" => "badge-success",
                "failed" | "timeout" => "badge-error",
                _ => "badge-ghost",
            };
            let icon = match j.status.as_str() {
                "pending" => "⏳",
                "completed" => "✅",
                "failed" | "timeout" => "⚠️",
                _ => "•",
            };
            let prompt_short: String = if j.prompt_id.len() > 12 {
                format!("{}…", &j.prompt_id[..12])
            } else {
                j.prompt_id.clone()
            };
            let detail: String = match j.status.as_str() {
                "completed" => j.output_filename.clone().unwrap_or_default(),
                "failed" | "timeout" => j.error_message.clone().unwrap_or_default(),
                _ => String::new(),
            };
            let detail_html: Html = if detail.is_empty() {
                plait::Html::new_unchecked(String::new())
            } else {
                html! {
                    div(class: "text-xs text-base-content/60 font-mono break-all mt-1") {
                        (detail)
                    }
                }
                .to_html()
            };
            let completed_str = j.completed_at.as_deref().unwrap_or("");
            let completed_html: Html = if completed_str.is_empty() {
                plait::Html::new_unchecked(String::new())
            } else {
                html! { " · completed: " (completed_str) }.to_html()
            };
            html! {
                div(class: "py-3 flex items-start gap-3") {
                    span(class: "text-lg") { (icon) }
                    div(class: "flex-1 min-w-0") {
                        div(class: "flex flex-wrap items-baseline gap-2") {
                            span(class: "font-mono text-sm font-semibold") { "#" (j.id) }
                            span(class: format!("badge badge-sm {badge_cls}")) { (j.status) }
                            span(class: "text-base-content/70 text-sm") { (j.workflow_id) }
                        }
                        div(class: "text-xs text-base-content/50 mt-1") {
                            "prompt: " (prompt_short) " · created: " (j.created_at)
                            (completed_html)
                        }
                        (detail_html)
                    }
                }
            }
            .to_html()
        })
        .collect();
    let pending_badge: Html = if pending_count > 0 {
        html! {
            span(class: "badge badge-warning badge-sm ml-1") {
                (format!("{} pending", pending_count))
            }
        }
        .to_html()
    } else {
        plait::Html::new_unchecked(String::new())
    };
    html! {
        div(class: "card border border-base-300 mb-6") {
            div(class: "card-body") {
                h2(class: "card-title text-base") {
                    "Recent jobs"
                    span(class: "badge badge-outline ml-2") { (format!("{}", jobs.len())) }
                    (pending_badge)
                }
                div(class: "flex flex-col divide-y divide-base-300") {
                    for row in rows.iter() {
                        (row)
                    }
                }
            }
        }
    }
    .to_html()
}
