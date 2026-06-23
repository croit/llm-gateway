// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Feedback widget — a floating button on every signed-in page that opens a
//! dialog to file a GitHub issue.
//!
//! Ported from the `yachtlistings2` / `croit.erp` React widgets, rebuilt
//! natively for this stack:
//!   - The FAB + `<dialog>` are static chrome rendered once in
//!     `layout_authed` (siblings of `<main>`, so they survive Datastar SPA
//!     navigation). All behaviour lives in `ui/ts/feedback.ts`, wired via
//!     `window.feedback`.
//!   - Voice input reuses the existing in-browser recorder + the
//!     `/api/v0/transcriptions` endpoint (VAD + Whisper). The transcript is
//!     then turned into structured form fields by `POST /feedback/extract`
//!     (a chat-model pass, the `chat/title.rs` idiom).
//!   - A viewport screenshot is captured client-side (`modern-screenshot`)
//!     and sent as base64; `POST /feedback` commits it to GitHub and opens
//!     the issue.
//!
//! Three JSON endpoints (deliberately not Datastar/SSE — the client uses
//! plain `fetch` so it can carry the screenshot bytes and a transcript):
//!   - GET  /feedback/config   → `{ enabled, voice_enabled, transcription_model, … }`
//!   - POST /feedback/extract  → transcript → structured fields
//!   - POST /feedback          → file the issue

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::service::web::response::IntoResponse;
use rama::http::{Request, Response, StatusCode, header};
use serde::Deserialize;
use serde_json::json;
use session_core::chrome::read_body_to_bytes;
use session_core::icons;

use crate::rama_server::session::Session;
use crate::rama_server::state::RamaState;
use crate::server::db::users;
use crate::server::github::{self, IssueInput};
use crate::server::upstreams::PoolKind;

// ---------------------------------------------------------------------------
// Small JSON helpers (these endpoints are fetch'd, not Datastar-driven).

fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

fn json_ok(value: serde_json::Value) -> Response {
    json_response(StatusCode::OK, value)
}

fn json_err(status: StatusCode, message: &str) -> Response {
    json_response(status, json!({ "error": { "message": message } }))
}

/// Session gate that returns a 401 JSON envelope (not a redirect) on miss —
/// these are API-shaped endpoints called from the dialog.
async fn require_session_json(state: &RamaState, req: &Request) -> Result<Session, Response> {
    match state.sessions.lookup_from_headers(req.headers()).await {
        Ok(Some(s)) => Ok(s),
        Ok(None) => Err(json_err(StatusCode::UNAUTHORIZED, "no active session")),
        Err(err) => {
            tracing::warn!(error = %err, "feedback: session lookup");
            Err(json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session lookup failed",
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// GET /feedback/config

/// Tells the client whether to reveal the FAB and the voice block, and which
/// transcription model to record against. Model *selection* is an operator
/// concern (config), never the end user's — the form has no model picker; the
/// client just needs the resolved voice model id to attach to its upload.
pub async fn feedback_config(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_session_json(&state, &req).await {
        return resp;
    }
    let enabled = state
        .config
        .feedback
        .as_ref()
        .map(|f| f.is_configured())
        .unwrap_or(false);

    let transcription_models = state.upstreams.models_for_kind(PoolKind::Transcription);
    let chat_models = state.upstreams.models_for_kind(PoolKind::Chat);
    // Voice→fields needs both a transcription model (to record against) and a
    // chat model (to extract the fields).
    let voice_enabled = !transcription_models.is_empty() && !chat_models.is_empty();
    let voice_model = resolve_model(
        state
            .config
            .feedback
            .as_ref()
            .and_then(|f| f.voice_model.clone()),
        &transcription_models,
    );

    json_ok(json!({
        "enabled": enabled,
        "voice_enabled": voice_enabled,
        "voice_model": voice_model,
    }))
}

/// Resolve a configured model id against the live advertised set: honour the
/// configured one if it's actually available, else fall back to the first
/// advertised model (or `None` when the pool is empty).
fn resolve_model(configured: Option<String>, available: &[String]) -> Option<String> {
    configured
        .filter(|m| !m.is_empty() && available.contains(m))
        .or_else(|| available.first().cloned())
}

// ---------------------------------------------------------------------------
// POST /feedback/extract — voice transcript → structured fields

#[derive(Deserialize)]
struct ExtractRequest {
    transcript: String,
    #[serde(default)]
    locale: Option<String>,
}

pub async fn feedback_extract(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    if let Err(resp) = require_session_json(&state, &req).await {
        return resp;
    }
    let (_, body) = req.into_parts();
    let bytes = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return json_err(StatusCode::BAD_REQUEST, &msg),
    };
    let parsed: ExtractRequest = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(err) => return json_err(StatusCode::BAD_REQUEST, &format!("malformed JSON: {err}")),
    };
    let transcript = parsed.transcript.trim();
    if transcript.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "empty transcript");
    }

    // Model selection is an operator concern: the configured `extraction_model`
    // if it's a currently advertised chat model, else the first available chat
    // model. The client never picks the model.
    let chat_models = state.upstreams.models_for_kind(PoolKind::Chat);
    let model = resolve_model(
        state
            .config
            .feedback
            .as_ref()
            .and_then(|f| f.extraction_model.clone()),
        &chat_models,
    );
    let Some(model) = model else {
        return json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no chat model available for extraction",
        );
    };

    match extract_fields(&state, &model, transcript, parsed.locale.as_deref()).await {
        Ok(fields) => json_ok(fields),
        Err(err) => {
            tracing::warn!(error = %err, %model, "feedback: field extraction failed");
            json_err(
                StatusCode::BAD_GATEWAY,
                &format!("extraction failed: {err}"),
            )
        }
    }
}

/// System prompt for the transcript→fields pass. Forbids invention (empty
/// string when a field isn't derivable) and pins the output language to the
/// caller's UI locale regardless of the spoken language. The trailing
/// `/no_think` mirrors `chat/title.rs` (Qwen3 reasoning-off marker; harmless
/// elsewhere).
const EXTRACT_SYSTEM_PROMPT: &str = "You convert a spoken software-feedback note into a structured bug/feature report. \
Return ONLY a JSON object with these string fields: \
\"title\" (imperative, concise, max 120 chars), \
\"description\" (what happened / what is wanted, factual), \
\"business_value\" (why it matters / who is impacted), \
\"acceptance_criteria\" (a short markdown bullet list of concrete conditions), \
\"priority\" (one of \"low\", \"medium\", \"high\" — use \"high\" only on explicit cues like \"urgent\", \"blocking\", \"production\"). \
Do NOT invent details: if a field cannot be derived from the transcript, use an empty string (for priority default to \"medium\"). \
No preamble, no code fences, no reasoning — output the raw JSON object only.";

/// Single non-streaming chat completion that returns the structured fields.
/// Models the `chat/title.rs::call_upstream` pattern; asks for JSON via
/// `response_format` (honoured by vLLM/OpenAI-compatible servers) and parses
/// leniently so a server that ignores the hint still works.
async fn extract_fields(
    state: &RamaState,
    model: &str,
    transcript: &str,
    locale: Option<&str>,
) -> Result<serde_json::Value, String> {
    let acquired = state
        .upstreams
        .acquire_for(model, PoolKind::Chat)
        .map_err(|e| e.to_string())?;
    let backend = acquired.backend();
    let url = format!("{}/chat/completions", backend.base_url);

    let lang_directive = match locale {
        Some(l) if !l.is_empty() => format!(
            "\n\nWrite every field value in the language with BCP-47 tag \"{l}\", \
             regardless of the transcript's language."
        ),
        _ => String::new(),
    };
    let user_content = format!("Transcript:\n{transcript}{lang_directive}\n\n/no_think");

    let body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": EXTRACT_SYSTEM_PROMPT },
            { "role": "user", "content": user_content },
        ],
        "stream": false,
        "temperature": 0.2,
        "max_tokens": 1200,
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "feedback_fields",
                "strict": true,
                "schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "title": { "type": "string" },
                        "description": { "type": "string" },
                        "business_value": { "type": "string" },
                        "acceptance_criteria": { "type": "string" },
                        "priority": { "type": "string", "enum": ["low", "medium", "high"] }
                    },
                    "required": ["title", "description", "business_value", "acceptance_criteria", "priority"]
                }
            }
        },
        "chat_template_kwargs": { "enable_thinking": false },
    });
    let serialized = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let mut http_req = state
        .http
        .post(&url)
        .header("content-type", "application/json")
        .body(serialized);
    if let Some(key) = backend.api_key.as_deref() {
        http_req = http_req.bearer_auth(key);
    }
    let resp = http_req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    drop(acquired);
    if !status.is_success() {
        return Err(format!(
            "upstream {status}: {}",
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(160)
                .collect::<String>()
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let content = v
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let obj = parse_lenient_json(content)
        .ok_or_else(|| "model returned no parseable JSON".to_string())?;

    // Re-shape into exactly the five fields the client expects, coercing
    // anything odd into a sane default.
    let pick = |key: &str| -> String {
        obj.get(key)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let priority = match obj.get("priority").and_then(|p| p.as_str()).unwrap_or("") {
        "low" => "low",
        "high" => "high",
        _ => "medium",
    };
    Ok(json!({
        "title": pick("title"),
        "description": pick("description"),
        "business_value": pick("business_value"),
        "acceptance_criteria": pick("acceptance_criteria"),
        "priority": priority,
    }))
}

/// Pull a JSON object out of an LLM response that may wrap it in ```json
/// fences or stray prose. Returns the first balanced object found.
fn parse_lenient_json(raw: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim())
        && v.is_object()
    {
        return Some(v);
    }
    let start = raw.find('{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in raw[start..].char_indices() {
        match ch {
            '"' if !escaped => in_str = !in_str,
            '\\' if in_str => {
                escaped = !escaped;
                continue;
            }
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    let candidate = &raw[start..start + i + 1];
                    return serde_json::from_str(candidate).ok();
                }
            }
            _ => {}
        }
        escaped = false;
    }
    None
}

// ---------------------------------------------------------------------------
// POST /feedback — file the issue

#[derive(Deserialize)]
struct SubmitRequest {
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    business_value: String,
    #[serde(default)]
    acceptance_criteria: String,
    #[serde(default)]
    priority: String,
    /// Raw standard-base64 PNG (no `data:` prefix). Optional.
    #[serde(default)]
    screenshot_base64: Option<String>,
    #[serde(default)]
    system_info: serde_json::Value,
}

pub async fn feedback_submit(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let session = match require_session_json(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let Some(cfg) = state.config.feedback.clone() else {
        return json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "feedback is not configured",
        );
    };
    if !cfg.is_configured() {
        return json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "feedback is not configured",
        );
    }

    let (_, body) = req.into_parts();
    let bytes = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return json_err(StatusCode::BAD_REQUEST, &msg),
    };
    let parsed: SubmitRequest = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(err) => return json_err(StatusCode::BAD_REQUEST, &format!("malformed JSON: {err}")),
    };

    let title = parsed.title.trim();
    if title.chars().count() < 4 {
        return json_err(
            StatusCode::BAD_REQUEST,
            "title is required (at least 4 characters)",
        );
    }
    if parsed.description.trim().is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "description is required");
    }

    // Reporter email — best-effort, for attribution in the issue body.
    let reporter_email = users::find_by_id(&state.db, &session.user_id)
        .await
        .ok()
        .flatten()
        .map(|u| u.email)
        .unwrap_or_default();

    let input = IssueInput {
        title: title.to_string(),
        description: parsed.description,
        business_value: parsed.business_value,
        acceptance_criteria: parsed.acceptance_criteria,
        priority: parsed.priority,
        reporter_email,
        screenshot_png_base64: parsed.screenshot_base64.filter(|s| !s.is_empty()),
        system_info: parsed.system_info,
    };

    match github::create_feedback_issue(&state.http, &cfg, input).await {
        Ok(result) => json_ok(json!({
            "ok": true,
            "number": result.number,
            "url": result.url,
        })),
        Err(github::GithubError::NotConfigured) => json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "feedback is not configured",
        ),
        Err(err) => {
            tracing::warn!(error = %err, "feedback: issue creation failed");
            json_err(
                StatusCode::BAD_GATEWAY,
                "could not file the issue — please try again",
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Chrome: the FAB + the dialog. Rendered once in `layout_authed`.

/// The floating action button. Starts hidden (`hidden` attribute) — the
/// client removes it once `GET /feedback/config` confirms the feature is
/// configured, so it never appears on a deployment without a GitHub repo set
/// up. `data-feedback-fab` marks it for exclusion from the screenshot.
pub(super) fn render_fab() -> Html {
    html! {
        button(
            id: "feedback-fab",
            type: "button",
            class: "feedback-fab",
            "data-feedback-fab": "",
            hidden: "hidden",
            title: "Send feedback",
            "aria-label": "Send feedback"
        ) {
            (icons::help(18))
        }
    }
    .to_html()
}

/// The feedback dialog. A native `<dialog>` (opened via `showModal()` from
/// `feedback.ts`). Excluded from the screenshot by `data-feedback-dialog`.
pub(super) fn render_dialog() -> Html {
    html! {
        dialog(
            id: "feedback-dialog",
            class: "feedback-dialog rounded-xl border border-base-300 p-0",
            "data-feedback-dialog": ""
        ) {
            form(id: "feedback-form", method: "dialog", class: "flex flex-col") {
                // Header. The voice button lives here (not in the scrollable
                // body): it acts on the whole form, so it stays pinned next to
                // the close button instead of scrolling away. Hidden until
                // `/feedback/config` confirms voice is available.
                div(class: "flex items-center gap-2 px-5 py-3 border-b border-base-300") {
                    (icons::message(18))
                    h2(class: "font-semibold flex-1") { "Send feedback" }
                    button(
                        id: "feedback-voice-btn",
                        type: "button",
                        class: "btn btn-ghost btn-sm gap-2",
                        hidden: "hidden",
                        title: "Tap, describe the issue, tap again — we'll fill the fields below"
                    ) {
                        span(class: "feedback-voice-icon inline-flex") { (icons::mic(16)) }
                        span(class: "feedback-voice-label") { "Fill in by voice" }
                    }
                    button(
                        id: "feedback-close",
                        type: "button",
                        class: "btn btn-ghost btn-square btn-sm",
                        "aria-label": "Close"
                    ) {
                        (icons::x_mark(16))
                    }
                }

                div(class: "px-5 py-4 flex flex-col gap-3 overflow-y-auto") {
                    label(class: "form-control") {
                        span(class: "label-text text-sm font-medium") { "Title" }
                        input(
                            id: "feedback-title",
                            name: "title",
                            type: "text",
                            required: "required",
                            maxlength: "120",
                            placeholder: "Short summary",
                            class: "input input-bordered w-full"
                        );
                    }
                    label(class: "form-control") {
                        span(class: "label-text text-sm font-medium") { "Description" }
                        textarea(
                            id: "feedback-description",
                            name: "description",
                            required: "required",
                            rows: "4",
                            placeholder: "What happened, or what would you like?",
                            class: "textarea textarea-bordered w-full"
                        ) {}
                    }
                    label(class: "form-control") {
                        span(class: "label-text text-sm font-medium") { "Business value" }
                        textarea(
                            id: "feedback-business",
                            name: "business_value",
                            rows: "2",
                            placeholder: "Why does this matter? Who is impacted?",
                            class: "textarea textarea-bordered w-full"
                        ) {}
                    }
                    label(class: "form-control") {
                        span(class: "label-text text-sm font-medium") { "Acceptance criteria" }
                        textarea(
                            id: "feedback-acceptance",
                            name: "acceptance_criteria",
                            rows: "2",
                            placeholder: "When is this done?",
                            class: "textarea textarea-bordered w-full"
                        ) {}
                    }
                    label(class: "form-control") {
                        span(class: "label-text text-sm font-medium") { "Priority" }
                        select(
                            id: "feedback-priority",
                            name: "priority",
                            class: "select select-bordered w-full"
                        ) {
                            option(value: "low") { "Low" }
                            option(value: "medium", selected: "selected") { "Medium" }
                            option(value: "high") { "High" }
                        }
                    }

                    // Screenshot + annotation.
                    div(class: "feedback-shot") {
                        div(class: "flex items-center gap-2 text-sm") {
                            span(class: "label-text text-sm font-medium") { "Screenshot" }
                            span(id: "feedback-shot-status", class: "text-xs text-base-content/60") {
                                "Capturing…"
                            }
                            div(class: "ml-auto flex gap-2") {
                                button(
                                    id: "feedback-shot-recapture",
                                    type: "button",
                                    class: "btn btn-ghost btn-xs"
                                ) { "Recapture" }
                                button(
                                    id: "feedback-shot-remove",
                                    type: "button",
                                    class: "btn btn-ghost btn-xs"
                                ) { "Remove" }
                            }
                        }
                        // Annotation toolbar — tool, colour, history, zoom.
                        // `feedback.ts` wires every control by id / data-attr.
                        div(id: "feedback-annot-toolbar", class: "feedback-annot-toolbar", hidden: "hidden") {
                            div(class: "feedback-tool-group") {
                                button(type: "button", class: "feedback-tool-btn", "data-tool": "rect", title: "Rectangle") { "▭" }
                                button(type: "button", class: "feedback-tool-btn", "data-tool": "arrow", title: "Arrow") { "↗" }
                                button(type: "button", class: "feedback-tool-btn", "data-tool": "pen", title: "Freehand") { "✎" }
                                button(type: "button", class: "feedback-tool-btn", "data-tool": "text", title: "Text") { "T" }
                                button(type: "button", class: "feedback-tool-btn", "data-tool": "redact", title: "Hide / redact (filled box)") { "▮" }
                            }
                            div(class: "feedback-tool-group") {
                                for c in ["#ef4444", "#3b82f6", "#10b981", "#f59e0b", "#ffffff"] {
                                    button(
                                        type: "button",
                                        class: "feedback-color-btn",
                                        "data-color": (c),
                                        style: (format!("background:{c}")),
                                        title: "Colour",
                                        "aria-label": "Colour"
                                    ) {}
                                }
                            }
                            div(class: "feedback-tool-group") {
                                button(id: "feedback-undo", type: "button", class: "btn btn-ghost btn-xs", title: "Undo") { "↶" }
                                button(id: "feedback-redo", type: "button", class: "btn btn-ghost btn-xs", title: "Redo") { "↷" }
                                button(id: "feedback-clear-annot", type: "button", class: "btn btn-ghost btn-xs", title: "Clear annotations") { "Clear" }
                            }
                            div(class: "feedback-tool-group ml-auto") {
                                button(id: "feedback-zoom-out", type: "button", class: "btn btn-ghost btn-xs", title: "Zoom out") { "−" }
                                button(id: "feedback-zoom-reset", type: "button", class: "btn btn-ghost btn-xs", title: "Reset zoom") { "100%" }
                                button(id: "feedback-zoom-in", type: "button", class: "btn btn-ghost btn-xs", title: "Zoom in") { "+" }
                            }
                        }
                        div(id: "feedback-shot-wrap", class: "feedback-shot-wrap", hidden: "hidden") {
                            canvas(id: "feedback-shot-canvas", class: "feedback-shot-canvas") {}
                        }
                    }

                    // Diagnostics consent. Default on; the user can opt out per
                    // submission. The chat/tool toggle only shows on chat pages.
                    div(class: "flex flex-col gap-1 pt-1") {
                        label(class: "label cursor-pointer justify-start gap-2 py-0") {
                            input(
                                id: "feedback-log-browser",
                                type: "checkbox",
                                class: "checkbox checkbox-sm",
                                checked: "checked"
                            );
                            span(class: "label-text text-sm") { "Submit browser activity log (console + network)" }
                        }
                        label(
                            id: "feedback-log-chat-wrap",
                            class: "label cursor-pointer justify-start gap-2 py-0",
                            hidden: "hidden"
                        ) {
                            input(
                                id: "feedback-log-chat",
                                type: "checkbox",
                                class: "checkbox checkbox-sm",
                                checked: "checked"
                            );
                            span(class: "label-text text-sm") { "Submit chat & tool usage log" }
                        }
                    }
                }

                // Footer
                div(class: "flex items-center justify-end gap-2 px-5 py-3 border-t border-base-300") {
                    button(
                        id: "feedback-cancel",
                        type: "button",
                        class: "btn btn-ghost btn-sm"
                    ) { "Cancel" }
                    button(
                        id: "feedback-submit",
                        type: "submit",
                        class: "btn btn-primary btn-sm"
                    ) { "Send feedback" }
                }
            }
        }
    }
    .to_html()
}

/// The "Are you sure?" confirmation dialog. A second native `<dialog>` stacked
/// on top of the feedback dialog (modal dialogs share the top layer, so it
/// overlays cleanly without z-index juggling). `feedback.ts` opens it on submit
/// — "No" leaves the feedback form open for editing, "Yes" fires the POST. It
/// warns that the issue tracker is public so no personal/private data leaks
/// into the screenshot or submitted fields. Excluded from any screenshot by
/// `data-feedback-dialog`.
pub(super) fn render_confirm() -> Html {
    html! {
        dialog(
            id: "feedback-confirm",
            class: "feedback-confirm rounded-xl border border-base-300 p-0",
            "data-feedback-dialog": ""
        ) {
            div(class: "flex flex-col") {
                // Header
                div(class: "flex items-center gap-2 px-5 py-3 border-b border-base-300") {
                    span(class: "text-warning inline-flex") { (icons::alert(18)) }
                    h2(class: "font-semibold flex-1") { "Are you sure?" }
                }

                // Body — the public-tracker / no-private-data warning.
                div(class: "px-5 py-4 flex flex-col gap-3 text-sm") {
                    p {
                        "This feedback opens a ticket in our "
                        strong { "public" }
                        " issue tracker. Anyone can read it."
                    }
                    p {
                        "Please make sure your screenshot and the submitted data contain "
                        strong { "no personal or private information" }
                        " (names, emails, tokens, customer data, …)."
                    }
                }

                // Footer — "No" returns to the form, "Yes" sends.
                div(class: "flex items-center justify-end gap-2 px-5 py-3 border-t border-base-300") {
                    button(
                        id: "feedback-confirm-cancel",
                        type: "button",
                        class: "btn btn-ghost btn-sm"
                    ) { "No, let me edit" }
                    button(
                        id: "feedback-confirm-ok",
                        type: "button",
                        class: "btn btn-primary btn-sm"
                    ) { "Yes, send" }
                }
            }
        }
    }
    .to_html()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_json_plain_object() {
        let v = parse_lenient_json(r#"{"title":"x","priority":"high"}"#).unwrap();
        assert_eq!(v["title"], "x");
    }

    #[test]
    fn lenient_json_with_fences_and_prose() {
        let raw = "Sure!\n```json\n{\"title\": \"Fix login\", \"priority\": \"low\"}\n```\nDone.";
        let v = parse_lenient_json(raw).unwrap();
        assert_eq!(v["title"], "Fix login");
        assert_eq!(v["priority"], "low");
    }

    #[test]
    fn lenient_json_handles_braces_in_strings() {
        let raw = r#"{"description": "use { and } carefully", "title": "t"}"#;
        let v = parse_lenient_json(raw).unwrap();
        assert_eq!(v["title"], "t");
        assert_eq!(v["description"], "use { and } carefully");
    }

    #[test]
    fn lenient_json_none_when_absent() {
        assert!(parse_lenient_json("no json here").is_none());
    }

    // Wiring contract: `feedback.ts` looks these elements up by id and relies
    // on the screenshot-exclusion markers. If a render id drifts from what the
    // client queries, the widget silently breaks — pin it here.
    #[test]
    fn fab_carries_marker_and_starts_hidden() {
        let html = render_fab().to_string();
        assert!(html.contains("id=\"feedback-fab\""));
        assert!(html.contains("data-feedback-fab"));
        assert!(
            html.contains("hidden"),
            "FAB must start hidden until config confirms"
        );
    }

    #[test]
    fn dialog_exposes_every_id_the_client_queries() {
        let html = render_dialog().to_string();
        // Exclusion marker so the dialog isn't baked into its own screenshot.
        assert!(html.contains("data-feedback-dialog"));
        // Every id `feedback.ts` resolves via getElementById / querySelector.
        for id in [
            "feedback-dialog",
            "feedback-form",
            "feedback-close",
            "feedback-cancel",
            "feedback-submit",
            "feedback-voice-btn",
            "feedback-title",
            "feedback-description",
            "feedback-business",
            "feedback-acceptance",
            "feedback-priority",
            "feedback-shot-status",
            "feedback-shot-recapture",
            "feedback-shot-remove",
            "feedback-annot-toolbar",
            "feedback-shot-wrap",
            "feedback-shot-canvas",
            "feedback-undo",
            "feedback-redo",
            "feedback-clear-annot",
            "feedback-zoom-in",
            "feedback-zoom-out",
            "feedback-zoom-reset",
            "feedback-log-browser",
            "feedback-log-chat",
            "feedback-log-chat-wrap",
        ] {
            assert!(
                html.contains(&format!("id=\"{id}\"")),
                "dialog missing id {id}"
            );
        }
        // The voice label span the client retitles.
        assert!(html.contains("feedback-voice-label"));
        // Annotation tool + colour controls the client wires by data-attr.
        assert!(html.contains("data-tool=\"rect\""));
        assert!(html.contains("data-tool=\"arrow\""));
        assert!(html.contains("data-tool=\"redact\""));
        assert!(html.contains("data-color=\"#ef4444\""));
        // Model pickers must NOT be in the form — model choice is config-only.
        assert!(!html.contains("feedback-text-model"));
        assert!(!html.contains("feedback-voice-model"));
        // The select/no-draw tool was removed (rectangle is the default).
        assert!(!html.contains("data-tool=\"select\""));
        // Priority options the extraction + client share.
        assert!(html.contains("value=\"low\""));
        assert!(html.contains("value=\"high\""));
    }

    // The confirm dialog is opened on submit; `feedback.ts` resolves its
    // controls by id. Pin the wiring + the public-tracker warning copy.
    #[test]
    fn confirm_dialog_exposes_ids_and_warning() {
        let html = render_confirm().to_string();
        // Excluded from the screenshot like the main dialog.
        assert!(html.contains("data-feedback-dialog"));
        for id in [
            "feedback-confirm",
            "feedback-confirm-cancel",
            "feedback-confirm-ok",
        ] {
            assert!(
                html.contains(&format!("id=\"{id}\"")),
                "confirm dialog missing id {id}"
            );
        }
        // The warning must name the public tracker and the no-private-data ask.
        assert!(html.contains("public"));
        assert!(html.contains("no personal or private information"));
    }
}
