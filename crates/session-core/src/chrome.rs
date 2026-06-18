// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Shared page chrome — Theme cookie, SSE-event helpers, Flash
//! toasts, cookie parsing, body collection, and the unauthenticated
//! `<html>` layout. Lives in `session-core` so a future second
//! consumer can paint the same styling and the same datastar SSE
//! patches without forking.
//!
//! What stays per-binary: the sidebar (nav items + auth model), the
//! authed-layout wrapper that wraps it, the auth gate, the login
//! page shape, and the page handlers themselves.

use plait::{Html, ToHtml, html};
use rama::http::service::web::response::IntoResponse;
use rama::http::{Body, HeaderMap, HeaderValue, Request, Response, StatusCode, header};

use crate::assets;

// ---------------------------------------------------------------------------
// Theme.

/// Cookie name carrying the user's theme preference. Read on every
/// page render; written by `theme_toggle` after a flip.
pub const THEME_COOKIE: &str = "theme";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    /// Reads `theme=` from the request's Cookie header. Defaults to
    /// Dark when missing or unrecognised — operators run these
    /// tools in tooling contexts and dark reads better.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        match read_cookie(headers, THEME_COOKIE).as_deref() {
            Some("light") => Theme::Light,
            _ => Theme::Dark,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }
    pub fn flip(self) -> Self {
        match self {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        }
    }
}

/// `Set-Cookie` value for the theme. 1-year max-age so the
/// preference rides reloads + fresh tabs.
pub fn set_theme_header(theme: Theme) -> HeaderValue {
    let value = format!(
        "theme={}; Path=/; SameSite=Lax; Max-Age={}",
        theme.as_str(),
        60 * 60 * 24 * 365
    );
    HeaderValue::try_from(value).expect("theme cookie value is ascii")
}

pub fn theme_toggle_icon(current: Theme) -> Html {
    match current {
        Theme::Dark => crate::icons::sun(18),
        Theme::Light => crate::icons::moon(18),
    }
}

pub fn theme_toggle_aria(current: Theme) -> &'static str {
    match current {
        Theme::Dark => "Switch to light theme",
        Theme::Light => "Switch to dark theme",
    }
}

/// The theme-toggle form — used for the initial sidebar render *and*
/// as the `mode outer` SSE patch payload after a flip, so the two
/// can't drift.
pub fn render_theme_toggle_form(theme: Theme) -> Html {
    html! {
        form(
            id: "theme-toggle-form",
            action: "/theme/toggle",
            method: "post",
            class: "m-0",
            "data-on:submit__prevent": "@post('/theme/toggle', {contentType: 'form'})"
        ) {
            button(
                type: "submit",
                class: "btn btn-ghost btn-square btn-sm",
                title: "Toggle theme",
                "aria-label": (theme_toggle_aria(theme))
            ) {
                (theme_toggle_icon(theme))
            }
        }
    }
    .to_html()
}

/// Handler: POST /theme/toggle. Flips the theme cookie + returns SSE
/// patches that swap the toggle-form's icon/label and re-paint
/// `<html data-theme>` / `<html class>` in place. Both binaries mount
/// this on the same path.
pub async fn theme_toggle(req: Request) -> Response {
    let current = Theme::from_headers(req.headers());
    let next = current.flip();
    let next_str = next.as_str();
    let script = format!(
        "{{ let h = document.documentElement; \
            h.setAttribute('data-theme', '{next_str}'); \
            h.className = '{next_str}'; }}"
    );
    let form_html = render_theme_toggle_form(next).to_string();
    let mut resp = sse_response(&[
        sse_patch(Some("#theme-toggle-form"), Some("outer"), &form_html),
        sse_script(&script),
    ]);
    resp.headers_mut()
        .append(header::SET_COOKIE, set_theme_header(next));
    resp
}

// ---------------------------------------------------------------------------
// Datastar request detection.

/// True iff this request was issued by the datastar runtime (any
/// `@get` / `@post`). Pages use this to decide between a full page
/// render and the surgical SSE patches `nav_or_html_page` emits.
pub fn is_datastar_request(headers: &HeaderMap) -> bool {
    headers
        .get("datastar-request")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

// ---------------------------------------------------------------------------
// Cookies.

/// Pull a named cookie out of a `Cookie:` header. Tolerates whitespace
/// after `;`; no percent-decoding (current callers store URL-safe
/// values only).
pub fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(header::COOKIE)?.to_str().ok()?;
    for piece in header.split(';') {
        let piece = piece.trim();
        if let Some((k, v)) = piece.split_once('=')
            && k == name
        {
            return Some(v.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Flash + toast.

#[derive(Clone, Debug)]
pub struct Flash {
    pub kind: FlashKind,
    pub message: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FlashKind {
    Success,
    Error,
    Info,
}

impl FlashKind {
    /// Left-border accent class — neutral toast surface, only this
    /// 4 px bar carries the status hue (shadcn-style).
    pub fn border_accent(self) -> &'static str {
        match self {
            FlashKind::Success => "border-l-success",
            FlashKind::Error => "border-l-error",
            FlashKind::Info => "border-l-info",
        }
    }
}

/// The toast region every page mounts. Datastar SSE patches append
/// `.toast-item` children to it.
pub fn toast_container() -> Html {
    html! {
        div(id: "toasts", class: "toast toast-bottom toast-end") {}
    }
    .to_html()
}

/// Single toast element. Matches `window.pushToast` in
/// `ui/ts/app.ts` so client-side and server-side toasts look the same.
pub fn render_toast(f: &Flash) -> Html {
    let classes = format!(
        "toast-item pointer-events-auto bg-base-100 text-base-content \
         border border-base-300 border-l-4 {} \
         rounded-lg shadow-md px-3 py-2 text-sm max-w-sm",
        f.kind.border_accent()
    );
    let msg = f.message.clone();
    html! {
        div(class: (classes), role: "status") { (msg) }
    }
    .to_html()
}

// ---------------------------------------------------------------------------
// SSE event helpers (datastar-patch-elements / -signals).

/// Build a `datastar-patch-elements` SSE event payload (terminated by
/// the blank line that ends an SSE event). `elements_html` may be
/// empty — `mode remove` doesn't need a body.
pub fn sse_patch(
    selector: Option<&str>,
    mode: Option<&str>,
    elements_html: &str,
) -> rama::bytes::Bytes {
    let mut out = String::from("event: datastar-patch-elements\n");
    if let Some(sel) = selector {
        out.push_str(&format!("data: selector {sel}\n"));
    }
    if let Some(m) = mode {
        out.push_str(&format!("data: mode {m}\n"));
    }
    if !elements_html.is_empty() {
        for line in elements_html.split('\n') {
            out.push_str("data: elements ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push('\n');
    rama::bytes::Bytes::from(out.into_bytes())
}

/// Fire a one-shot snippet of JS on the client. Datastar 1.x dropped
/// the standalone `datastar-execute-script` event; we ride on the
/// element-patching pipeline (append a `<script>` to `<body>`, let the
/// browser execute, the script removes itself).
pub fn sse_script(js: &str) -> rama::bytes::Bytes {
    let payload =
        format!("<script>try{{ {js} }} finally {{ document.currentScript?.remove(); }}</script>");
    sse_patch(Some("body"), Some("append"), &payload)
}

/// `datastar-patch-signals` event. The body is a JSON object that
/// Datastar merges into the global signal store.
pub fn sse_signals(signals_json: &str) -> rama::bytes::Bytes {
    let mut out = String::from("event: datastar-patch-signals\n");
    for line in signals_json.split('\n') {
        out.push_str("data: signals ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    rama::bytes::Bytes::from(out.into_bytes())
}

/// Bundle a set of pre-built SSE event payloads into a single response.
pub fn sse_response(events: &[rama::bytes::Bytes]) -> Response {
    let mut payload = Vec::with_capacity(events.iter().map(|e| e.len()).sum());
    for ev in events {
        payload.extend_from_slice(ev);
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(payload.into())
        .unwrap()
}

/// Convenience: append a freshly-rendered toast into `#toasts`.
pub fn sse_toast(flash: &Flash) -> rama::bytes::Bytes {
    let toast = render_toast(flash).to_string();
    sse_patch(Some("#toasts"), Some("append"), &toast)
}

/// Convenience: an SSE response that fires one toast and nothing
/// else. Used by failure-branches that have nothing to patch.
pub fn sse_toast_response(kind: FlashKind, message: impl Into<String>) -> Response {
    sse_response(&[sse_toast(&Flash {
        kind,
        message: message.into(),
    })])
}

// ---------------------------------------------------------------------------
// Body collection.

pub async fn read_body_to_bytes(body: Body) -> Result<rama::bytes::Bytes, String> {
    use rama::http::body::util::BodyExt;
    body.collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| format!("reading body: {e}"))
}

// ---------------------------------------------------------------------------
// Plain (unauthed) HTML responses.

/// 303 redirect — Post/Redirect/Get so reloads don't re-submit.
pub fn see_other(to: &str) -> Response {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, to)
        .body("".into())
        .unwrap()
}

/// Wrap an arbitrary HTML body string in an `200 OK; text/html`
/// response with the usual `Permissions-Policy` we set for every page
/// (mic + geolocation same-origin only; camera disabled).
///
/// `geolocation=(self)` (not `()`!) is load-bearing: an empty allowlist
/// disables the feature entirely, so `navigator.geolocation` rejects
/// with `PERMISSION_DENIED` *without ever prompting* — which is exactly
/// what `get_user_location`'s in-chat "share your location?" prompt
/// needs to NOT happen. `(self)` lets the same-origin page request it,
/// at which point the browser shows its native allow/deny prompt.
pub fn html_response(body: String) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (
                rama::http::HeaderName::from_static("permissions-policy"),
                "microphone=(self), camera=(), geolocation=(self)",
            ),
        ],
        body,
    )
        .into_response()
}

/// Minimal `<html>` chrome — daisyUI stylesheet + datastar runtime +
/// a slot for `body`. No sidebar; used by the login/error pages and
/// anything else that doesn't sit inside the authed app shell.
pub fn layout(theme: Theme, title: &str, body: Html) -> String {
    let theme_str = theme.as_str();
    let css_href = assets::app_css_url();
    let datastar_src = assets::datastar_js_url();
    let frag = html! {
        html(lang: "en", "data-theme": (theme_str), class: (theme_str)) {
            head {
                meta(charset: "utf-8");
                meta(name: "viewport", content: "width=device-width, initial-scale=1");
                title { (title) }
                link(rel: "stylesheet", href: (css_href));
                script(type: "module", src: (datastar_src)) {}
            }
            body(class: "min-h-dvh bg-base-100 text-base-content") {
                (body)
                (toast_container())
            }
        }
    };
    frag.to_html().to_string()
}

/// `html_response(layout(...))`.
pub fn html_page(theme: Theme, title: &str, body: Html) -> Response {
    html_response(layout(theme, title, body))
}
