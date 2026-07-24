// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Scheduled actions page — per-user prompts that run on a cron schedule.
//!
//! The centrepiece is a friendly schedule *builder*: a Repeat selector
//! (Hourly / Daily / Weekly / Monthly / Advanced) whose sub-panels feed a
//! single 5-field cron string assembled server-side. Non-technical users
//! never see cron unless they pick Advanced; everyone gets a live
//! human-readable summary plus the next three run times, computed by the
//! server (`POST /scheduled/preview`) so the preview can never drift from
//! what the scheduler will actually do.
//!
//! CRUD handlers return `text/event-stream` (datastar patches) like the
//! tokens/memory pages; the edit surface is a full sub-page reached via
//! SPA navigation. Auth is the plain session gate — every signed-in user
//! manages their own schedules (scoped by `user_id` in the data layer).

use std::sync::Arc;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::{Path, State};
use rama::http::{Request, Response};
use serde::Deserialize;

use super::{
    NavItem, fetch_sidebar_chat, internal_error_html, is_admin, nav_or_html_page,
    require_session_or_redirect, toast,
};
use session_core::chrome::{
    Flash, FlashKind, NavSections, Theme, is_datastar_request, sse_patch, sse_response, sse_script,
    sse_toast,
};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

use crate::rama_server::state::RamaState;
use crate::server::scheduled::{self, EditAction, NewAction, ScheduledAction, cron::Cron};

// ---------------------------------------------------------------------------
// Form types

/// The schedule-builder fields, shared by create / update / preview.
/// Weekday checkboxes are seven discrete `wd{n}` booleans (cron numbering,
/// Sunday = 0) rather than a repeated key, since `serde_urlencoded` can't
/// decode a repeated field into a `Vec`.
#[derive(Deserialize, Default)]
struct ScheduleFields {
    mode: String,
    minute: Option<String>,
    hour: Option<String>,
    dom: Option<String>,
    advanced: Option<String>,
    wd0: Option<String>,
    wd1: Option<String>,
    wd2: Option<String>,
    wd3: Option<String>,
    wd4: Option<String>,
    wd5: Option<String>,
    wd6: Option<String>,
}

impl ScheduleFields {
    fn weekdays(&self) -> Vec<u8> {
        [
            &self.wd0, &self.wd1, &self.wd2, &self.wd3, &self.wd4, &self.wd5, &self.wd6,
        ]
        .iter()
        .enumerate()
        .filter_map(|(i, v)| v.as_ref().map(|_| i as u8))
        .collect()
    }

    /// Assemble the 5-field cron string this builder describes, or a
    /// user-facing error. The friendly modes are valid by construction;
    /// Advanced is whatever the user typed, validated by the caller.
    fn assemble_cron(&self, lang: Lang) -> Result<String, String> {
        let minute = parse_in_range(
            self.minute.as_deref(),
            0,
            59,
            "scheduled-field-minute",
            lang,
        )?;
        let hour = parse_in_range(self.hour.as_deref(), 0, 23, "scheduled-field-hour", lang)?;
        match self.mode.as_str() {
            "hourly" => Ok(format!("{minute} * * * *")),
            "daily" => Ok(format!("{minute} {hour} * * *")),
            "weekly" => {
                let days = self.weekdays();
                if days.is_empty() {
                    return Err(t(lang, "scheduled-err-pick-weekday"));
                }
                let days = days
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                Ok(format!("{minute} {hour} * * {days}"))
            }
            "monthly" => {
                let dom = parse_in_range(
                    self.dom.as_deref(),
                    1,
                    31,
                    "scheduled-field-day-of-month",
                    lang,
                )?;
                Ok(format!("{minute} {hour} {dom} * *"))
            }
            "advanced" => {
                let raw = self.advanced.as_deref().unwrap_or("").trim().to_string();
                if raw.is_empty() {
                    return Err(t(lang, "scheduled-err-enter-cron"));
                }
                Ok(raw)
            }
            other => Err(t_args(
                lang,
                "scheduled-err-unknown-schedule-type",
                &i18n::args([("kind", other.to_string().into())]),
            )),
        }
    }
}

/// Full create payload: the schedule builder plus the action's identity.
#[derive(Deserialize)]
struct CreateForm {
    name: String,
    prompt: String,
    model: String,
    timezone: String,
    tools: Option<String>,
    /// Present (checkbox ticked) = reuse the previous run's chat session.
    reuse: Option<String>,
    /// Recent rounds of history to replay when reusing. Absent/invalid falls
    /// back to the default in [`reuse_rounds_or_default`].
    reuse_rounds: Option<String>,
    #[serde(flatten)]
    schedule: ScheduleFields,
}

/// Default replay window when reuse is on but no (valid) count was submitted.
const DEFAULT_REUSE_ROUNDS: i64 = 5;

/// Clamp the form's reuse-rounds field to a sane 1..=50 window, defaulting
/// when absent or unparseable. The cap keeps a reused conversation's replayed
/// history bounded regardless of what the form posts.
fn reuse_rounds_or_default(raw: Option<&str>) -> i64 {
    raw.and_then(|s| s.trim().parse::<i64>().ok())
        .map(|n| n.clamp(1, 50))
        .unwrap_or(DEFAULT_REUSE_ROUNDS)
}

/// Preview payload: just enough to compute the summary + next runs.
#[derive(Deserialize)]
struct PreviewForm {
    timezone: String,
    #[serde(flatten)]
    schedule: ScheduleFields,
}

fn parse_in_range(
    v: Option<&str>,
    min: u8,
    max: u8,
    field_key: &str,
    lang: Lang,
) -> Result<u8, String> {
    let v = v.unwrap_or("").trim();
    let field = t(lang, field_key);
    if v.is_empty() {
        return Err(t_args(
            lang,
            "scheduled-err-enter-field",
            &i18n::args([("field", field.into())]),
        ));
    }
    let n: u8 = v.parse().map_err(|_| {
        t_args(
            lang,
            "scheduled-err-invalid-field",
            &i18n::args([
                ("field", field.clone().into()),
                ("value", v.to_string().into()),
            ]),
        )
    })?;
    if n < min || n > max {
        return Err(t_args(
            lang,
            "scheduled-err-field-range",
            &i18n::args([
                ("field", field.into()),
                ("min", min.to_string().into()),
                ("max", max.to_string().into()),
            ]),
        ));
    }
    Ok(n)
}

/// Resolve a timezone name to a `jiff` zone, falling back to UTC for an
/// unknown name (validated on the form before we get here).
fn resolve_tz(name: &str) -> TimeZone {
    TimeZone::get(name).unwrap_or(TimeZone::UTC)
}

// ---------------------------------------------------------------------------
// Handlers

/// GET /scheduled — the management page: a builder form + the user's list.
pub async fn scheduled_index(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = require_session!(state, req);
    let actions = match scheduled::list_for_user(&state.db, &user.id).await {
        Ok(a) => a,
        Err(err) => {
            tracing::warn!(error = %err, "listing scheduled actions");
            return internal_error_html(&user.email, "could not list scheduled actions");
        }
    };
    let models = list_models(&state).await;
    let default_tz = user.timezone.clone().unwrap_or_else(|| "UTC".to_string());
    let body = render_index_body(&actions, &models, &default_tz, lang);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    {
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
            NavItem::Scheduled,
            &t(lang, "scheduled-page-title"),
            body,
            "/scheduled",
            &chat,
        )
    }
}

/// POST /scheduled — create from the builder form.
pub async fn scheduled_create(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (_, user) = require_session!(state, req);
    let (_, body) = req.into_parts();
    let form: CreateForm = match super::read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };

    let prepared = match prepare(
        &form.name,
        &form.prompt,
        &form.model,
        &form.timezone,
        &form.schedule,
        lang,
    ) {
        Ok(p) => p,
        Err(msg) => return toast(FlashKind::Error, msg),
    };

    let new = NewAction {
        user_id: user.id.clone(),
        name: prepared.name,
        prompt: prepared.prompt,
        model: prepared.model,
        cron: prepared.cron,
        timezone: prepared.timezone,
        tools_enabled: form.tools.is_some(),
        reuse_conversation: form.reuse.is_some(),
        reuse_rounds: reuse_rounds_or_default(form.reuse_rounds.as_deref()),
        next_run_at: prepared.next_run_at,
    };
    let created = match scheduled::create(&state.db, new).await {
        Ok(a) => a,
        Err(err) => {
            tracing::warn!(error = %err, "creating scheduled action");
            return toast(FlashKind::Error, t(lang, "scheduled-toast-save-failed"));
        }
    };

    let row_html = render_action_row(&created, lang).to_string();
    // Reset the form, then re-sync the builder: `form.reset()` restores the
    // default-checked mode radio but doesn't touch datastar's `$mode` signal,
    // so without this the panel + preview would stay on the just-submitted
    // mode. Dispatching `change` on the now-checked radio re-runs its
    // handler ($mode = … + preview refresh), snapping everything back to the
    // default. The reuse checkbox carries the same wrinkle — `f.reset()`
    // unchecks it but leaves `$reuse` true, stranding the "send last N
    // rounds" field visible — so re-dispatch its `change` too.
    let reset_script = "const f = document.getElementById('sched-create-form'); \
         f.reset(); \
         f.querySelector('input[name=mode]:checked')\
         ?.dispatchEvent(new Event('change', {bubbles: true})); \
         f.querySelector('input[name=reuse]')\
         ?.dispatchEvent(new Event('change', {bubbles: true}));";
    sse_response(&[
        sse_patch(Some("#sched-list"), Some("append"), &row_html),
        sse_script(reset_script),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: t(lang, "scheduled-toast-created"),
        }),
    ])
}

/// GET /scheduled/{id}/edit — the full-page edit form for one action.
pub async fn scheduled_edit_form(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    let nav = NavSections::from_headers(req.headers());
    let datastar = is_datastar_request(req.headers());
    let (session, user) = require_session!(state, req);
    let action = match scheduled::get(&state.db, &user.id, &id).await {
        Ok(Some(a)) => a,
        Ok(None) => return super::forbidden_html(&user.email, "no such scheduled action"),
        Err(err) => {
            tracing::warn!(error = %err, "loading scheduled action");
            return internal_error_html(&user.email, "could not load the scheduled action");
        }
    };
    let models = list_models(&state).await;
    let body = render_edit_body(&action, &models, lang);
    let chat = fetch_sidebar_chat(&state, &user.id, None).await;
    {
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
            NavItem::Scheduled,
            &t(lang, "scheduled-edit-page-title"),
            body,
            &format!("/scheduled/{id}/edit"),
            &chat,
        )
    }
}

/// POST /scheduled/{id} — apply an edit. On success navigates back to the
/// list; on a validation error fires a toast and leaves the form standing.
pub async fn scheduled_update(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (_, user) = require_session!(state, req);
    let (_, body) = req.into_parts();
    let form: CreateForm = match super::read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let prepared = match prepare(
        &form.name,
        &form.prompt,
        &form.model,
        &form.timezone,
        &form.schedule,
        lang,
    ) {
        Ok(p) => p,
        Err(msg) => return toast(FlashKind::Error, msg),
    };
    let edit = EditAction {
        name: prepared.name,
        prompt: prepared.prompt,
        model: prepared.model,
        cron: prepared.cron,
        timezone: prepared.timezone,
        tools_enabled: form.tools.is_some(),
        reuse_conversation: form.reuse.is_some(),
        reuse_rounds: reuse_rounds_or_default(form.reuse_rounds.as_deref()),
        // Keep a paused action paused: only recompute the next fire when
        // it's currently enabled. We re-read to decide.
        next_run_at: prepared.next_run_at,
    };
    // Preserve the paused state's NULL next_run_at.
    let edit = match scheduled::get(&state.db, &user.id, &id).await {
        Ok(Some(a)) if !a.enabled => EditAction {
            next_run_at: None,
            ..edit
        },
        _ => edit,
    };
    match scheduled::update(&state.db, &user.id, &id, edit).await {
        Ok(true) => sse_response(&[
            sse_toast(&Flash {
                kind: FlashKind::Success,
                message: t(lang, "scheduled-toast-updated"),
            }),
            sse_script("window.location.assign('/scheduled')"),
        ]),
        Ok(false) => toast(FlashKind::Error, t(lang, "scheduled-toast-not-found")),
        Err(err) => {
            tracing::warn!(error = %err, "updating scheduled action");
            toast(FlashKind::Error, t(lang, "scheduled-toast-update-failed"))
        }
    }
}

/// POST /scheduled/{id}/toggle — pause or resume. Resuming recomputes the
/// next fire time; pausing clears it so the worker skips the action.
pub async fn scheduled_toggle(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (_, user) = require_session!(state, req);
    let action = match scheduled::get(&state.db, &user.id, &id).await {
        Ok(Some(a)) => a,
        Ok(None) => return toast(FlashKind::Error, t(lang, "scheduled-toast-not-found")),
        Err(err) => {
            tracing::warn!(error = %err, "toggling scheduled action");
            return toast(FlashKind::Error, t(lang, "scheduled-toast-update-failed"));
        }
    };
    let resume = !action.enabled;
    let next = if resume {
        Cron::parse(&action.cron)
            .ok()
            .and_then(|c| c.next_after(Timestamp::now(), &resolve_tz(&action.timezone)))
    } else {
        None
    };
    if let Err(err) = scheduled::set_enabled(&state.db, &user.id, &id, resume, next).await {
        tracing::warn!(error = %err, "toggling scheduled action");
        return toast(FlashKind::Error, t(lang, "scheduled-toast-update-failed"));
    }
    // Re-render the row fresh from the DB so the badge + next-run line and
    // the pause/resume button reflect the new state.
    match scheduled::get(&state.db, &user.id, &id).await {
        Ok(Some(updated)) => {
            let selector = format!("#sched-row-{id}");
            let row_html = render_action_row(&updated, lang).to_string();
            sse_response(&[
                sse_patch(Some(&selector), Some("outer"), &row_html),
                sse_toast(&Flash {
                    kind: FlashKind::Success,
                    message: if resume {
                        t(lang, "scheduled-toast-resumed")
                    } else {
                        t(lang, "scheduled-toast-paused")
                    },
                }),
            ])
        }
        _ => toast(FlashKind::Error, t(lang, "scheduled-toast-refresh-failed")),
    }
}

/// POST /scheduled/{id}/delete — remove an action.
pub async fn scheduled_delete(
    State(state): State<Arc<RamaState>>,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    let lang = Lang::from_headers(req.headers());
    let (_, user) = require_session!(state, req);
    match scheduled::delete(&state.db, &user.id, &id).await {
        Ok(true) => {
            let selector = format!("#sched-row-{id}");
            sse_response(&[
                sse_patch(Some(&selector), Some("remove"), ""),
                sse_toast(&Flash {
                    kind: FlashKind::Success,
                    message: t(lang, "scheduled-toast-deleted"),
                }),
            ])
        }
        Ok(false) => toast(FlashKind::Info, t(lang, "scheduled-toast-already-gone")),
        Err(err) => {
            tracing::warn!(error = %err, "deleting scheduled action");
            toast(FlashKind::Error, t(lang, "scheduled-toast-delete-failed"))
        }
    }
}

/// POST /scheduled/preview — the live summary + next-runs for the current
/// builder state. Returns an SSE patch of `#schedule-preview`.
pub async fn scheduled_preview(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let lang = Lang::from_headers(req.headers());
    if require_session_or_redirect(&state, &req).await.is_err() {
        // Don't redirect a fetch; just send an empty patch.
        return sse_response(&[sse_patch(Some("#schedule-preview"), Some("inner"), "")]);
    }
    let (_, body) = req.into_parts();
    let form: PreviewForm = match super::read_form(body).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let html = match form.schedule.assemble_cron(lang) {
        Ok(cron) => render_preview(&cron, &form.timezone, lang),
        Err(msg) => render_preview_error(&msg),
    };
    sse_response(&[sse_patch(
        Some("#schedule-preview"),
        Some("inner"),
        &html.to_string(),
    )])
}

// ---------------------------------------------------------------------------
// Shared prepare + validation

struct Prepared {
    name: String,
    prompt: String,
    model: String,
    timezone: String,
    cron: String,
    next_run_at: Option<Timestamp>,
}

/// Validate the common fields and assemble + validate the cron, computing
/// the first fire time. The single gate both create and update run through.
fn prepare(
    name: &str,
    prompt: &str,
    model: &str,
    timezone: &str,
    schedule: &ScheduleFields,
    lang: Lang,
) -> Result<Prepared, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(t(lang, "scheduled-err-name-length"));
    }
    let prompt = prompt.trim();
    if prompt.is_empty() || prompt.len() > 8000 {
        return Err(t(lang, "scheduled-err-prompt-length"));
    }
    let model = model.trim();
    if model.is_empty() {
        return Err(t(lang, "scheduled-err-pick-model"));
    }
    let timezone = timezone.trim();
    if TimeZone::get(timezone).is_err() {
        return Err(t_args(
            lang,
            "scheduled-err-unknown-timezone",
            &i18n::args([("tz", timezone.to_string().into())]),
        ));
    }
    let cron = schedule.assemble_cron(lang)?;
    let parsed = Cron::parse(&cron).map_err(|e| e.to_string())?;
    let next_run_at = parsed.next_after(Timestamp::now(), &resolve_tz(timezone));
    Ok(Prepared {
        name: name.to_string(),
        prompt: prompt.to_string(),
        model: model.to_string(),
        timezone: timezone.to_string(),
        cron,
        next_run_at,
    })
}

// ---------------------------------------------------------------------------
// Models (id + compliance flags), mirrored from the chat page

struct ModelOption {
    id: String,
    gdpr: bool,
    nda: bool,
}

async fn list_models(state: &RamaState) -> Vec<ModelOption> {
    state
        .upstreams
        .models_with_compliance_for_kind(crate::server::upstreams::PoolKind::Chat)
        .into_iter()
        .map(|(id, c)| ModelOption {
            id,
            gdpr: c.gdpr,
            nda: c.nda,
        })
        .collect()
}

fn model_label(m: &ModelOption, lang: Lang) -> String {
    match (m.gdpr, m.nda) {
        (true, true) => m.id.clone(),
        (false, true) => t_args(
            lang,
            "scheduled-model-non-gdpr",
            &i18n::args([("model", m.id.clone().into())]),
        ),
        (true, false) => t_args(
            lang,
            "scheduled-model-nda-restricted",
            &i18n::args([("model", m.id.clone().into())]),
        ),
        (false, false) => t_args(
            lang,
            "scheduled-model-non-gdpr-nda-restricted",
            &i18n::args([("model", m.id.clone().into())]),
        ),
    }
}

/// datastar signal store the compliance banners read — same shape the chat
/// page uses (`selectedModel`, `gdprFlagged`, `ndaFlagged`).
fn compliance_signals(models: &[ModelOption], selected: &str) -> String {
    let gdpr: Vec<&str> = models
        .iter()
        .filter(|m| !m.gdpr)
        .map(|m| m.id.as_str())
        .collect();
    let nda: Vec<&str> = models
        .iter()
        .filter(|m| !m.nda)
        .map(|m| m.id.as_str())
        .collect();
    format!(
        "{{selectedModel: {}, gdprFlagged: {}, ndaFlagged: {}}}",
        serde_json::to_string(selected).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(&gdpr).unwrap_or_else(|_| "[]".into()),
        serde_json::to_string(&nda).unwrap_or_else(|_| "[]".into()),
    )
}

// ---------------------------------------------------------------------------
// Builder decomposition (for the edit page)

/// Initial state for the schedule builder. Created from defaults (new
/// action) or decomposed from an existing cron string (edit).
struct BuilderInit {
    mode: String,
    minute: u8,
    hour: u8,
    dom: u8,
    weekdays: Vec<u8>,
    advanced: String,
    timezone: String,
    /// Whether a cron string failed to map onto a friendly preset, so the
    /// builder opens on Advanced with the raw expression.
    advanced_raw: String,
}

impl BuilderInit {
    fn defaults(timezone: &str) -> Self {
        BuilderInit {
            mode: "daily".to_string(),
            minute: 0,
            hour: 9,
            dom: 1,
            weekdays: vec![1], // Monday
            advanced: "0 9 * * *".to_string(),
            timezone: timezone.to_string(),
            advanced_raw: "0 9 * * *".to_string(),
        }
    }

    /// Best-effort decompose: recognise the friendly shapes the builder
    /// emits and preselect that mode; anything else opens on Advanced with
    /// the raw expression preserved.
    fn from_cron(cron: &str, timezone: &str) -> Self {
        let mut init = BuilderInit::defaults(timezone);
        init.advanced = cron.to_string();
        init.advanced_raw = cron.to_string();
        init.mode = "advanced".to_string();

        let f: Vec<&str> = cron.split_whitespace().collect();
        if f.len() != 5 {
            return init;
        }
        let (m, h, dom, mon, dow) = (f[0], f[1], f[2], f[3], f[4]);
        let as_u8 = |s: &str| s.parse::<u8>().ok();
        if mon != "*" {
            return init;
        }
        match (m, h, dom, dow) {
            // Hourly: fixed minute, every hour.
            (m, "*", "*", "*") if as_u8(m).is_some() => {
                init.mode = "hourly".to_string();
                init.minute = as_u8(m).unwrap();
            }
            // Daily: fixed minute + hour.
            (m, h, "*", "*") if as_u8(m).is_some() && as_u8(h).is_some() => {
                init.mode = "daily".to_string();
                init.minute = as_u8(m).unwrap();
                init.hour = as_u8(h).unwrap();
            }
            // Weekly: fixed time on a list of weekdays.
            (m, h, "*", dow) if as_u8(m).is_some() && as_u8(h).is_some() => {
                let days: Option<Vec<u8>> = dow
                    .split(',')
                    .map(|d| as_u8(d).map(|n| if n == 7 { 0 } else { n }))
                    .collect();
                if let Some(mut days) = days
                    && days.iter().all(|d| *d <= 6)
                {
                    days.sort_unstable();
                    days.dedup();
                    init.mode = "weekly".to_string();
                    init.minute = as_u8(m).unwrap();
                    init.hour = as_u8(h).unwrap();
                    init.weekdays = days;
                }
            }
            // Monthly: fixed time on a day-of-month.
            (m, h, dom, "*")
                if as_u8(m).is_some() && as_u8(h).is_some() && as_u8(dom).is_some() =>
            {
                init.mode = "monthly".to_string();
                init.minute = as_u8(m).unwrap();
                init.hour = as_u8(h).unwrap();
                init.dom = as_u8(dom).unwrap();
            }
            _ => {}
        }
        init
    }
}

// ---------------------------------------------------------------------------
// Rendering

fn render_index_body(
    actions: &[ScheduledAction],
    models: &[ModelOption],
    default_tz: &str,
    lang: Lang,
) -> Html {
    let init = BuilderInit::defaults(default_tz);
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
            h1(class: "text-2xl font-bold mb-2") { (t(lang, "scheduled-heading")) }
            p(class: "text-base-content/60 text-sm mb-6") {
                (t(lang, "scheduled-intro"))
            }

            (render_form("/scheduled", "sched-create-form", &t(lang, "scheduled-create-submit"), None, models, &init, lang))

            section(class: "card border border-base-300") {
                div(class: "card-body") {
                    h2(class: "card-title") { (t(lang, "scheduled-list-heading")) }
                    ul(id: "sched-list", class: "sched-list flex flex-col divide-y divide-base-300") {
                        for a in actions.iter() {
                            (render_action_row(a, lang))
                        }
                    }
                    if actions.is_empty() {
                        p(class: "text-base-content/60 text-sm") {
                            (t(lang, "scheduled-list-empty"))
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

fn render_edit_body(action: &ScheduledAction, models: &[ModelOption], lang: Lang) -> Html {
    let init = BuilderInit::from_cron(&action.cron, &action.timezone);
    html! {
        div(class: "max-w-5xl mx-auto w-full px-4 sm:px-6 pt-14 sm:pt-6 pb-6") {
            div(class: "flex items-center gap-3 mb-4") {
                a(
                    href: "/scheduled",
                    class: "btn btn-ghost btn-sm",
                    "data-on:click__prevent": "@get('/scheduled')"
                ) {
                    (icons::chevron_left(16)) (t(lang, "scheduled-back"))
                }
                h1(class: "text-2xl font-bold") { (t(lang, "scheduled-edit-heading")) }
            }
            (render_form(
                &format!("/scheduled/{}", action.id),
                "sched-edit-form",
                &t(lang, "scheduled-save-submit"),
                Some(action),
                models,
                &init,
                lang,
            ))
        }
    }
    .to_html()
}

/// The create/edit form, including the schedule builder. `action` is
/// `Some` on the edit page (prefills name/prompt/model/tools).
fn render_form(
    post_url: &str,
    form_id: &str,
    submit_label: &str,
    action: Option<&ScheduledAction>,
    models: &[ModelOption],
    init: &BuilderInit,
    lang: Lang,
) -> Html {
    let models_empty = models.is_empty();
    let selected_model = action
        .map(|a| a.model.clone())
        .or_else(|| models.first().map(|m| m.id.clone()))
        .unwrap_or_default();
    let signals = compliance_signals(models, &selected_model);
    let any_gdpr = models.iter().any(|m| !m.gdpr);
    let any_nda = models.iter().any(|m| !m.nda);
    let name_val = action.map(|a| a.name.clone()).unwrap_or_default();
    let prompt_val = action.map(|a| a.prompt.clone()).unwrap_or_default();
    let tools_on = action.map(|a| a.tools_enabled).unwrap_or(true);
    let reuse_on = action.map(|a| a.reuse_conversation).unwrap_or(false);
    let reuse_rounds_val = action
        .map(|a| a.reuse_rounds)
        .unwrap_or(DEFAULT_REUSE_ROUNDS)
        .to_string();
    let submit_directive = format!("@post('{post_url}', {{contentType: 'form'}})");
    let model_opts: Vec<(String, String, bool)> = models
        .iter()
        .map(|m| (m.id.clone(), model_label(m, lang), m.id == selected_model))
        .collect();
    let post_url_owned = post_url.to_string();
    let submit_label = submit_label.to_string();

    html! {
        // `novalidate`: the live-preview `@post` (fired by the schedule
        // controls' `data-on:change`) serializes this whole form, and
        // datastar refuses to send a form that fails HTML5 validation —
        // which it always would while the required Name/Prompt are still
        // empty, so the preview would never update until they were filled.
        // Server-side `prepare()` is the real validation gate (it returns a
        // clear toast for each problem), so dropping the browser's check
        // costs nothing and lets the preview update from an empty form.
        form(
            id: (form_id),
            action: (post_url_owned),
            method: "post",
            novalidate: "novalidate",
            class: "card border border-base-300 mb-6",
            "data-on:submit__prevent": (submit_directive)
        ) {
            div(class: "card-body gap-4") {
                // --- Name ---
                label(class: "flex flex-col gap-1 w-full") {
                    div(class: "label") { span(class: "label-text") { (t(lang, "scheduled-name-label")) } }
                    input(
                        name: "name",
                        type: "text",
                        required: "required",
                        maxlength: "128",
                        value: (name_val),
                        placeholder: (t(lang, "scheduled-name-placeholder")),
                        class: "input input-bordered w-full"
                    );
                }

                // --- Model + compliance banner ---
                label(class: "flex flex-col gap-1 w-full") {
                    div(class: "label") { span(class: "label-text") { (t(lang, "scheduled-model-label")) } }
                    if models_empty {
                        input(
                            name: "model",
                            type: "text",
                            required: "required",
                            value: (selected_model.clone()),
                            placeholder: (t(lang, "scheduled-model-placeholder")),
                            class: "input input-bordered w-full"
                        );
                    } else {
                        select(
                            name: "model",
                            required: "required",
                            "data-on:change": "$selectedModel = evt.target.value",
                            class: "select select-bordered w-full"
                        ) {
                            for (value, label, sel) in model_opts.iter() {
                                if *sel {
                                    option(value: (value.clone()), selected: "selected") { (label.clone()) }
                                } else {
                                    option(value: (value.clone())) { (label.clone()) }
                                }
                            }
                        }
                    }
                }
                // Compliance signal store + reactive banners (same mechanism
                // as the chat composer). Seed `selectedModel` from the live
                // DOM value on mount so a prefilled/edit selection is correct
                // before any `change`.
                div(
                    "data-signals": (signals),
                    "data-init": "$selectedModel = document.querySelector('[name=model]')?.value ?? $selectedModel",
                    style: "display:none"
                ) {}
                if any_gdpr {
                    div(
                        class: "alert alert-error",
                        role: "alert",
                        "data-show": "$gdprFlagged.includes($selectedModel)",
                        style: "display:none"
                    ) {
                        (icons::alert(20))
                        span {
                            (t(lang, "scheduled-gdpr-warning"))
                        }
                    }
                }
                if any_nda {
                    div(
                        class: "alert alert-error",
                        role: "alert",
                        "data-show": "$ndaFlagged.includes($selectedModel)",
                        style: "display:none"
                    ) {
                        (icons::alert(20))
                        span {
                            (t(lang, "scheduled-nda-warning"))
                        }
                    }
                }

                // --- Prompt ---
                label(class: "flex flex-col gap-1 w-full") {
                    div(class: "label") { span(class: "label-text") { (t(lang, "scheduled-prompt-label")) } }
                    textarea(
                        name: "prompt",
                        required: "required",
                        rows: "4",
                        maxlength: "8000",
                        placeholder: (t(lang, "scheduled-prompt-placeholder")),
                        class: "textarea textarea-bordered w-full"
                    ) { (prompt_val) }
                }

                // --- Schedule builder ---
                (render_schedule_builder(init, lang))

                // --- Tools toggle ---
                label(class: "label cursor-pointer justify-start gap-3") {
                    if tools_on {
                        input(type: "checkbox", name: "tools", checked: "checked", class: "checkbox checkbox-sm");
                    } else {
                        input(type: "checkbox", name: "tools", class: "checkbox checkbox-sm");
                    }
                    span(class: "label-text") {
                        (t(lang, "scheduled-tools-toggle-label"))
                    }
                }

                // --- Conversation reuse toggle ---
                // `$reuse` drives whether the rounds input is enabled, seeded
                // from the checkbox's initial checked state on mount.
                div("data-signals": (format!("{{reuse: {reuse_on}}}")), style: "display:none") {}
                div(class: "flex flex-wrap items-center gap-3") {
                    label(class: "label cursor-pointer justify-start gap-3") {
                        if reuse_on {
                            input(
                                type: "checkbox", name: "reuse", checked: "checked",
                                "data-on:change": "$reuse = evt.target.checked",
                                class: "checkbox checkbox-sm"
                            );
                        } else {
                            input(
                                type: "checkbox", name: "reuse",
                                "data-on:change": "$reuse = evt.target.checked",
                                class: "checkbox checkbox-sm"
                            );
                        }
                        span(class: "label-text") {
                            (t(lang, "scheduled-reuse-toggle-label"))
                        }
                    }
                    label(class: "flex items-center gap-2 text-sm", "data-show": "$reuse") {
                        span(class: "label-text opacity-70") { (t(lang, "scheduled-reuse-rounds-prefix")) }
                        input(
                            name: "reuse_rounds",
                            type: "number",
                            min: "1",
                            max: "50",
                            value: (reuse_rounds_val),
                            "aria-label": (t(lang, "scheduled-reuse-rounds-aria")),
                            class: "input input-bordered input-sm w-20"
                        );
                        span(class: "label-text opacity-70") { (t(lang, "scheduled-reuse-rounds-suffix")) }
                    }
                }

                div(class: "card-actions justify-end") {
                    button(type: "submit", class: "btn btn-primary") { (submit_label) }
                }
            }
        }
    }
    .to_html()
}

/// The directive each schedule control fires on change to refresh the
/// server-computed preview. `contentType: 'form'` serializes the whole
/// create form and posts it to `/scheduled/preview`; the handler ignores
/// the name/prompt/model fields and only reads the schedule + timezone.
/// (Requires `novalidate` on the form — see the form's comment — or
/// datastar refuses to send while the required fields are still empty.)
const PREVIEW_REFRESH: &str = "@post('/scheduled/preview', {contentType: 'form'})";

/// The Repeat selector + per-mode sub-panels + live preview. Every change
/// reposts `/scheduled/preview`, which patches `#schedule-preview`.
fn render_schedule_builder(init: &BuilderInit, lang: Lang) -> Html {
    let signals = format!(
        "{{mode: {}}}",
        serde_json::to_string(&init.mode).unwrap_or_else(|_| "\"daily\"".into())
    );
    let weekdays = init.weekdays.clone();
    // Mon-first display order, mapped to cron numbering (Sun = 0).
    let day_cells: Vec<(u8, String)> = vec![
        (1, t(lang, "scheduled-weekday-mon")),
        (2, t(lang, "scheduled-weekday-tue")),
        (3, t(lang, "scheduled-weekday-wed")),
        (4, t(lang, "scheduled-weekday-thu")),
        (5, t(lang, "scheduled-weekday-fri")),
        (6, t(lang, "scheduled-weekday-sat")),
        (0, t(lang, "scheduled-weekday-sun")),
    ];
    let minute_str = format!("{:02}", init.minute);
    let hour_str = format!("{:02}", init.hour);
    let dom_str = init.dom.to_string();
    let advanced_str = init.advanced.clone();
    let tz_str = init.timezone.clone();
    let initial_preview = render_preview(&init.advanced_raw, &init.timezone, lang);

    // The outer div owns the `$mode` signal store; the controls below carry
    // their own `data-on:change` to refresh the preview (see PREVIEW_REFRESH).
    html! {
        div("data-signals": (signals)) {
        div(class: "rounded-lg border border-base-300 p-4 flex flex-col gap-4") {
            div(class: "text-sm font-medium") { (t(lang, "scheduled-builder-heading")) }

            // Repeat selector.
            div(class: "flex flex-wrap gap-2") {
                (mode_radio("hourly", &t(lang, "scheduled-mode-hourly"), &init.mode))
                (mode_radio("daily", &t(lang, "scheduled-mode-daily"), &init.mode))
                (mode_radio("weekly", &t(lang, "scheduled-mode-weekly"), &init.mode))
                (mode_radio("monthly", &t(lang, "scheduled-mode-monthly"), &init.mode))
                (mode_radio("advanced", &t(lang, "scheduled-mode-advanced"), &init.mode))
            }

            // Weekly: which weekdays. (Shown only in weekly mode.)
            div("data-show": "$mode === 'weekly'", class: "flex flex-wrap gap-1") {
                for (num, label) in day_cells.iter() {
                    (weekday_toggle(*num, label, weekdays.contains(num)))
                }
            }

            // Monthly: which day of the month. (Shown only in monthly mode.)
            div("data-show": "$mode === 'monthly'", class: "flex items-end gap-2") {
                label(class: "flex flex-col gap-1") {
                    div(class: "label") { span(class: "label-text") { (t(lang, "scheduled-on-day-label")) } }
                    input(name: "dom", type: "number", min: "1", max: "31", value: (dom_str), "data-on:change": (PREVIEW_REFRESH), class: "input input-bordered w-24");
                }
                span(class: "text-base-content/60 pb-3") { (t(lang, "scheduled-of-every-month")) }
            }

            // Time of day + timezone share one row. ONE hour + ONE minute
            // input, shared by every non-advanced mode and rendered exactly
            // once — otherwise each mode panel would carry its own
            // `hour`/`minute` and the form would post duplicate fields (which
            // serde_urlencoded rejects). The hour + ":" hide in hourly mode
            // (which needs only a minute); "of every hour" shows there instead.
            div(class: "flex items-end flex-wrap gap-6") {
                div("data-show": "$mode !== 'advanced'", class: "flex flex-col gap-1") {
                    div(class: "label") { span(class: "label-text") { (t(lang, "scheduled-at-label")) } }
                    div(class: "flex items-end gap-1") {
                        span("data-show": "$mode !== 'hourly'", class: "flex items-end gap-1") {
                            input(name: "hour", type: "number", min: "0", max: "23", value: (hour_str), "aria-label": (t(lang, "scheduled-hour-aria")), "data-on:change": (PREVIEW_REFRESH), class: "input input-bordered w-20");
                            span(class: "font-bold pb-3") { ":" }
                        }
                        input(name: "minute", type: "number", min: "0", max: "59", value: (minute_str), "aria-label": (t(lang, "scheduled-minute-aria")), "data-on:change": (PREVIEW_REFRESH), class: "input input-bordered w-20");
                        span("data-show": "$mode === 'hourly'", class: "text-base-content/60 pb-3 ml-1") { (t(lang, "scheduled-of-every-hour")) }
                    }
                }

                // Timezone.
                label(class: "flex flex-col gap-1 w-full max-w-xs") {
                    div(class: "label") { span(class: "label-text") { (t(lang, "scheduled-timezone-label")) } }
                    input(name: "timezone", type: "text", value: (tz_str), placeholder: (t(lang, "scheduled-timezone-placeholder")), "data-on:change": (PREVIEW_REFRESH), class: "input input-bordered w-full");
                }
            }

            // Advanced panel.
            div("data-show": "$mode === 'advanced'", class: "flex flex-col gap-1") {
                label(class: "flex flex-col gap-1") {
                    div(class: "label") { span(class: "label-text") { (t(lang, "scheduled-cron-label")) } }
                    input(name: "advanced", type: "text", value: (advanced_str), placeholder: "0 9 * * *", "data-on:change": (PREVIEW_REFRESH), class: "input input-bordered w-full font-mono");
                }
                span(class: "text-xs text-base-content/60") {
                    (t(lang, "scheduled-cron-help"))
                }
            }

            // Live, server-computed preview.
            div(id: "schedule-preview", class: "rounded-md bg-base-200 p-3 text-sm") {
                (initial_preview)
            }
        }
        }
    }
    .to_html()
}

fn mode_radio(value: &str, label: &str, current: &str) -> Html {
    let value = value.to_string();
    let label = label.to_string();
    // Switch the visible panel *and* refresh the preview in one handler.
    let set = format!("$mode = '{value}'; {PREVIEW_REFRESH}");
    let checked = value == current;
    html! {
        label(class: "label cursor-pointer gap-2 rounded-md border border-base-300 px-3 py-1.5") {
            if checked {
                input(type: "radio", name: "mode", value: (value.clone()), checked: "checked", "data-on:change": (set), class: "radio radio-sm");
            } else {
                input(type: "radio", name: "mode", value: (value.clone()), "data-on:change": (set), class: "radio radio-sm");
            }
            span(class: "label-text") { (label) }
        }
    }
    .to_html()
}

fn weekday_toggle(num: u8, label: &str, checked: bool) -> Html {
    let name = format!("wd{num}");
    let label = label.to_string();
    html! {
        label(class: "label cursor-pointer gap-1.5 rounded-md border border-base-300 px-2.5 py-1") {
            if checked {
                input(type: "checkbox", name: (name), checked: "checked", "data-on:change": (PREVIEW_REFRESH), class: "checkbox checkbox-xs");
            } else {
                input(type: "checkbox", name: (name), "data-on:change": (PREVIEW_REFRESH), class: "checkbox checkbox-xs");
            }
            span(class: "label-text text-xs") { (label) }
        }
    }
    .to_html()
}

/// The summary + next-three-runs block. Used for both the initial render
/// and the live `/scheduled/preview` patches.
fn render_preview(cron: &str, timezone: &str, lang: Lang) -> Html {
    let parsed = match Cron::parse(cron) {
        Ok(c) => c,
        Err(e) => return render_preview_error(&e.to_string()),
    };
    if TimeZone::get(timezone).is_err() {
        return render_preview_error(&t_args(
            lang,
            "scheduled-err-unknown-timezone",
            &i18n::args([("tz", timezone.to_string().into())]),
        ));
    }
    let tz = resolve_tz(timezone);
    let summary = parsed.describe();
    let runs = parsed.upcoming(Timestamp::now(), &tz, 3);
    let run_lines: Vec<String> = runs
        .iter()
        .map(|t| {
            t.to_zoned(tz.clone())
                .strftime("%a %b %-d, %H:%M")
                .to_string()
        })
        .collect();
    let tz_label = timezone.to_string();
    html! {
        div(class: "flex flex-col gap-1") {
            div(class: "font-medium") { (summary) " (" (tz_label) ")" }
            if run_lines.is_empty() {
                div(class: "text-base-content/60") { (t(lang, "scheduled-no-upcoming-runs")) }
            } else {
                div(class: "text-base-content/70") {
                    (t(lang, "scheduled-next-runs-prefix"))
                    (run_lines.join("  ·  "))
                }
            }
        }
    }
    .to_html()
}

fn render_preview_error(msg: &str) -> Html {
    let msg = msg.to_string();
    html! {
        div(class: "text-error flex items-center gap-2") {
            (icons::alert(16))
            span { (msg) }
        }
    }
    .to_html()
}

/// One row in the list. Single source of truth for the initial render and
/// the toggle SSE patch.
fn render_action_row(a: &ScheduledAction, lang: Lang) -> Html {
    let row_id = format!("sched-row-{}", a.id);
    let summary = Cron::parse(&a.cron)
        .map(|c| c.describe())
        .unwrap_or_else(|_| format!("cron: {}", a.cron));
    let schedule_line = format!("{} · {} ({})", a.model, summary, a.timezone);
    let next_line = match (a.enabled, a.next_run_at) {
        (false, _) => t(lang, "scheduled-status-paused"),
        (true, Some(t_run)) => t_args(
            lang,
            "scheduled-next-run",
            &i18n::args([(
                "when",
                t_run
                    .to_zoned(resolve_tz(&a.timezone))
                    .strftime("%a %b %-d, %H:%M")
                    .to_string()
                    .into(),
            )]),
        ),
        (true, None) => t(lang, "scheduled-no-upcoming-run"),
    };
    let prompt_preview: String = {
        let p = a.prompt.trim();
        if p.chars().count() > 96 {
            let mut s: String = p.chars().take(96).collect();
            s.push('…');
            s
        } else {
            p.to_string()
        }
    };
    let last_line = a.last_run_at.map(|t| {
        let when = t
            .to_zoned(resolve_tz(&a.timezone))
            .strftime("%b %-d, %H:%M")
            .to_string();
        let ok = a.last_status.as_deref() == Some("ok");
        (when, ok, a.last_session_id.clone())
    });

    let toggle_url = format!("/scheduled/{}/toggle", a.id);
    let delete_url = format!("/scheduled/{}/delete", a.id);
    let edit_url = format!("/scheduled/{}/edit", a.id);
    let toggle_directive = format!("@post('{toggle_url}', {{contentType: 'form'}})");
    let delete_directive = format!("@post('{delete_url}', {{contentType: 'form'}})");
    let edit_directive = format!("@get('{edit_url}')");
    let enabled = a.enabled;
    let name = a.name.clone();

    html! {
        li(id: (row_id), class: "flex items-start gap-4 py-3") {
            div(class: "flex-1 min-w-0") {
                div(class: "flex items-center gap-2") {
                    span(class: "text-sm font-medium text-base-content") { (name) }
                    if enabled {
                        span(class: "badge badge-success badge-sm") { (t(lang, "scheduled-badge-active")) }
                    } else {
                        span(class: "badge badge-ghost badge-sm") { (t(lang, "scheduled-badge-paused")) }
                    }
                }
                div(class: "text-xs text-base-content/60 truncate") { (prompt_preview) }
                div(class: "text-xs text-base-content/70 mt-0.5") { (schedule_line) }
                div(class: "text-xs text-base-content/60 mt-0.5 flex flex-wrap items-center gap-x-3") {
                    span { (next_line) }
                    if let Some((when, ok, session)) = last_line.as_ref() {
                        if *ok {
                            if let Some(sid) = session {
                                a(
                                    href: (format!("/chat/{sid}")),
                                    class: "link link-hover text-success",
                                    "data-on:click__prevent": (format!("@get('/chat/{sid}')"))
                                ) {
                                    (t_args(lang, "scheduled-last-success-open", &i18n::args([("when", when.clone().into())])))
                                }
                            } else {
                                span(class: "text-success") {
                                    (t_args(lang, "scheduled-last-success", &i18n::args([("when", when.clone().into())])))
                                }
                            }
                        } else {
                            if let Some(sid) = session {
                                a(
                                    href: (format!("/chat/{sid}")),
                                    class: "link link-hover text-error",
                                    "data-on:click__prevent": (format!("@get('/chat/{sid}')"))
                                ) {
                                    (t_args(lang, "scheduled-last-failure-open", &i18n::args([("when", when.clone().into())])))
                                }
                            } else {
                                span(class: "text-error") {
                                    (t_args(lang, "scheduled-last-failure", &i18n::args([("when", when.clone().into())])))
                                }
                            }
                        }
                    }
                }
            }
            div(class: "flex items-center gap-1 shrink-0") {
                // Pause / resume.
                form(action: (toggle_url), method: "post", class: "m-0", "data-on:submit__prevent": (toggle_directive)) {
                    button(
                        type: "submit",
                        class: "btn btn-ghost btn-sm btn-square",
                        title: (if enabled { t(lang, "scheduled-pause-title") } else { t(lang, "scheduled-resume-title") }),
                        "aria-label": (if enabled { t(lang, "scheduled-pause-title") } else { t(lang, "scheduled-resume-title") })
                    ) {
                        if enabled { (icons::pause(16)) } else { (icons::play(16)) }
                    }
                }
                // Edit (SPA nav to the edit sub-page).
                a(href: (edit_url), class: "btn btn-ghost btn-sm btn-square", title: (t(lang, "scheduled-edit-title")), "aria-label": (t(lang, "scheduled-edit-title")), "data-on:click__prevent": (edit_directive)) {
                    (icons::pencil(16))
                }
                // Delete.
                form(action: (delete_url), method: "post", class: "m-0", "data-on:submit__prevent": (delete_directive)) {
                    button(type: "submit", class: "btn btn-ghost btn-sm btn-square text-error", title: (t(lang, "scheduled-delete-title")), "aria-label": (t(lang, "scheduled-delete-title"))) {
                        (icons::trash(16))
                    }
                }
            }
        }
    }
    .to_html()
}
