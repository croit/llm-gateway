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
use rama::http::service::web::extract::Path;
use rama::http::service::web::response::IntoResponse;
use rama::http::{Body, HeaderMap, HeaderValue, Request, Response, StatusCode, header};

use crate::assets;
use crate::i18n::{Lang, set_lang_header, t};
use crate::icons;

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

pub fn theme_toggle_aria(current: Theme, lang: Lang) -> String {
    match current {
        Theme::Dark => t(lang, "chrome-theme-toggle-aria-to-light"),
        Theme::Light => t(lang, "chrome-theme-toggle-aria-to-dark"),
    }
}

/// The theme-toggle form — used for the initial sidebar render *and*
/// as the `mode outer` SSE patch payload after a flip, so the two
/// can't drift.
pub fn render_theme_toggle_form(theme: Theme, lang: Lang) -> Html {
    let title = t(lang, "chrome-theme-toggle-title");
    let aria = theme_toggle_aria(theme, lang);
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
                title: (title),
                "aria-label": (aria)
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
    let lang = Lang::from_headers(req.headers());
    let next = current.flip();
    let next_str = next.as_str();
    let script = format!(
        "{{ let h = document.documentElement; \
            h.setAttribute('data-theme', '{next_str}'); \
            h.className = '{next_str}'; }}"
    );
    let form_html = render_theme_toggle_form(next, lang).to_string();
    let mut resp = sse_response(&[
        sse_patch(Some("#theme-toggle-form"), Some("outer"), &form_html),
        sse_script(&script),
    ]);
    resp.headers_mut()
        .append(header::SET_COOKIE, set_theme_header(next));
    resp
}

// ---------------------------------------------------------------------------
// Language switcher.

/// Which way the language dropdown opens relative to its trigger button —
/// `Down` for a switcher anchored near the top of the viewport (the login
/// page), `Up` for one anchored near the bottom (the app sidebar's user
/// row), same reasoning as the composer's `effort-panel`/`cap-panel`
/// (bottom-anchored triggers open upward so the panel doesn't run off
/// the bottom of the screen).
#[derive(Copy, Clone)]
pub enum LangPanelAnchor {
    Up,
    Down,
}

/// The flag shown on the switcher trigger + each dropdown row. Emoji
/// flags render in colour with no image assets or CSS. Not a strict
/// country<->language mapping (English isn't tied to one nation) — just
/// the conventional shorthand every language-switcher UI uses.
fn lang_flag(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "🇬🇧",
        Lang::De => "🇩🇪",
        Lang::Fr => "🇫🇷",
        Lang::Es => "🇪🇸",
        Lang::Ru => "🇷🇺",
        Lang::Zh => "🇨🇳",
    }
}

/// The language switcher — a flag-icon trigger button that opens a
/// hand-rolled popover listing all 6 languages as flag + name rows,
/// same pattern as the chat page's tools/effort/export popovers
/// (`data-signals` + `data-show` + `click__outside` dismissal; native
/// `<details class="dropdown">` was dropped project-wide because its
/// positioning/card styles don't survive the Tailwind purge on mobile).
/// Each row is its own plain `<form method="post" action="/lang">` —
/// unlike the popovers above, submitting one is a real page reload, not
/// an SSE patch (see `lang_set`), so this degrades to a working,
/// if-unstyled, list of buttons even if the popover-toggle JS didn't
/// load. `next` is the current page's path, carried as a hidden field
/// so the redirect lands back where the user was.
pub fn render_lang_switcher_form(lang: Lang, next: &str, anchor: LangPanelAnchor) -> Html {
    let aria = t(lang, "chrome-lang-switcher-aria");
    let panel_class = match anchor {
        LangPanelAnchor::Up => {
            "lang-panel lang-panel--up rounded-box border border-base-300 bg-base-100 shadow"
        }
        LangPanelAnchor::Down => {
            "lang-panel lang-panel--down rounded-box border border-base-300 bg-base-100 shadow"
        }
    };
    html! {
        div(
            id: "lang-switcher",
            "data-signals": "{langMenu: false}",
            "data-on:click__outside": "$langMenu = false",
            style: "position:relative"
        ) {
            button(
                type: "button",
                "data-on:click": "$langMenu = !$langMenu",
                class: "btn btn-ghost btn-square btn-sm",
                title: (aria.clone()),
                "aria-label": (aria)
            ) {
                span(class: "text-base leading-none", "aria-hidden": "true") { (lang_flag(lang)) }
            }
            div(
                class: (panel_class),
                "data-show": "$langMenu",
                "data-on:click": "$langMenu = false",
                style: "display:none"
            ) {
                for candidate in Lang::ALL {
                    (render_lang_option(candidate, lang, next))
                }
            }
        }
    }
    .to_html()
}

/// One flag + name row in the language dropdown. A real `<form>` (not a
/// datastar `@post`) so selecting a language works even without JS —
/// only the panel's open/close toggle needs it.
fn render_lang_option(candidate: Lang, current: Lang, next: &str) -> Html {
    let is_current = candidate == current;
    let row_class = if is_current {
        "chat-pop-item chat-pop-item--active"
    } else {
        "chat-pop-item"
    };
    let next = next.to_string();
    html! {
        form(method: "post", action: "/lang", class: "m-0") {
            input(type: "hidden", name: "lang", value: (candidate.code()));
            input(type: "hidden", name: "next", value: (next));
            button(type: "submit", class: (row_class)) {
                span(class: "chat-pop-item__check") { (icons::check(14)) }
                span("aria-hidden": "true") { (lang_flag(candidate)) }
                span { (candidate.label()) }
            }
        }
    }
    .to_html()
}

/// A same-origin, absolute path — starts with exactly one `/` (not
/// `//`, which some URL parsers treat as protocol-relative to another
/// host, and not `/\`, an old browser normalisation quirk) with no
/// scheme. Mirrors `oidc_handlers::is_safe_return_to`; duplicated
/// rather than shared because that lives in the `gateway` crate, which
/// depends on `session-core`, not the other way around.
fn is_safe_redirect_target(path: &str) -> bool {
    let path = path.trim_start_matches(|c: char| c.is_ascii_whitespace());
    path.starts_with('/') && !path.starts_with("//") && !path.starts_with("/\\")
}

/// Handler: POST /lang. Sets the `lang` cookie from the submitted
/// `lang` form field and 303-redirects to `next` (falling back to `/`
/// if `next` is absent or not a safe same-origin path). Reachable
/// without a session — the switcher is rendered on `/login` too.
pub async fn lang_set(req: Request) -> Response {
    #[derive(serde::Deserialize)]
    struct LangForm {
        lang: String,
        next: Option<String>,
    }
    let (_, body) = req.into_parts();
    let Ok(bytes) = read_body_to_bytes(body).await else {
        return see_other("/");
    };
    let Ok(form) = serde_urlencoded::from_bytes::<LangForm>(&bytes) else {
        return see_other("/");
    };
    let Some(lang) = Lang::from_code(&form.lang) else {
        return see_other("/");
    };
    let target = form
        .next
        .filter(|n| is_safe_redirect_target(n))
        .unwrap_or_else(|| "/".to_string());
    let mut resp = see_other(&target);
    resp.headers_mut()
        .append(header::SET_COOKIE, set_lang_header(lang));
    resp
}

// ---------------------------------------------------------------------------
// Sidebar section collapse state.

/// Cookie carrying which sidebar nav-groups the user has expanded.
/// Read on every full page render (to paint the initial `<html>`
/// `data-nav-*` attributes); written by [`nav_sections_toggle`] after a
/// flip. Mirrors the `theme` cookie pattern — server-side, so the state
/// survives both a full reload *and* the SPA sidebar morph: the
/// attributes live on `<html>`, which nav patches never replace, and the
/// CSS keys off them.
pub const NAV_SECTIONS_COOKIE: &str = "nav_sections";

/// The collapsible sidebar nav-groups + their open/closed state.
///
/// Defaults: Workspace open, Account + Admin collapsed — the common case
/// is reaching for a workspace page (Memory / Scheduled / Tools), while
/// Account and Admin are occasional. A first-time visitor (no cookie)
/// gets these defaults; once they toggle anything the full open-set is
/// persisted explicitly.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NavSections {
    pub workspace: bool,
    pub account: bool,
    pub admin: bool,
}

impl Default for NavSections {
    fn default() -> Self {
        Self {
            workspace: true,
            account: false,
            admin: false,
        }
    }
}

impl NavSections {
    /// Parse the `nav_sections` cookie. Absent → [`Default`] (Workspace
    /// open). Present → exactly the listed groups are open; a
    /// present-but-`none` value means all collapsed, which is distinct
    /// from absent (default).
    pub fn from_headers(headers: &HeaderMap) -> Self {
        match read_cookie(headers, NAV_SECTIONS_COOKIE) {
            Some(v) => Self::parse(&v),
            None => Self::default(),
        }
    }

    fn parse(value: &str) -> Self {
        let mut s = Self {
            workspace: false,
            account: false,
            admin: false,
        };
        for token in value.split(',') {
            match token.trim() {
                "workspace" => s.workspace = true,
                "account" => s.account = true,
                "admin" => s.admin = true,
                _ => {}
            }
        }
        s
    }

    /// Serialise to the cookie value: a comma list of open groups, or
    /// `none` when all are collapsed — so the stored value is never empty
    /// and always overrides the absent-cookie default.
    fn serialize(self) -> String {
        let mut open = Vec::new();
        if self.workspace {
            open.push("workspace");
        }
        if self.account {
            open.push("account");
        }
        if self.admin {
            open.push("admin");
        }
        if open.is_empty() {
            "none".to_string()
        } else {
            open.join(",")
        }
    }

    /// Flip the named group. Unknown names are ignored.
    fn toggle(&mut self, section: &str) {
        match section {
            "workspace" => self.workspace = !self.workspace,
            "account" => self.account = !self.account,
            "admin" => self.admin = !self.admin,
            _ => {}
        }
    }

    /// `Some(open?)` for a known group, `None` for an unknown name.
    fn is_open(self, section: &str) -> Option<bool> {
        match section {
            "workspace" => Some(self.workspace),
            "account" => Some(self.account),
            "admin" => Some(self.admin),
            _ => None,
        }
    }

    /// `"open"` / `"closed"` — the value used for the `<html>`
    /// `data-nav-*` attributes the collapse CSS keys off.
    pub fn attr(open: bool) -> &'static str {
        if open { "open" } else { "closed" }
    }
}

/// `Set-Cookie` value for the nav-sections preference. 1-year max-age,
/// same as the theme cookie, so the layout rides reloads + fresh tabs.
fn set_nav_sections_header(sections: NavSections) -> HeaderValue {
    let value = format!(
        "{NAV_SECTIONS_COOKIE}={}; Path=/; SameSite=Lax; Max-Age={}",
        sections.serialize(),
        60 * 60 * 24 * 365
    );
    HeaderValue::try_from(value).expect("nav_sections cookie value is ascii")
}

/// Handler: POST /nav/toggle/{section}. Flips one sidebar group's
/// open/closed state in the cookie and returns an SSE patch that sets
/// the matching `<html data-nav-{section}>` attribute in place — the CSS
/// keyed off that attribute shows/hides the group's items. Because the
/// attribute lives on `<html>` (outside the nav-patched `#app-sidebar`),
/// the state survives both an in-page navigation and a full reload
/// (which re-reads the cookie). Mirrors [`theme_toggle`].
pub async fn nav_sections_toggle(Path(section): Path<String>, req: Request) -> Response {
    let mut sections = NavSections::from_headers(req.headers());
    sections.toggle(&section);
    let Some(open) = sections.is_open(&section) else {
        // Unknown group — no-op, leave the cookie untouched. `section`
        // is now known to be one of the literal group names, so it's
        // safe to splice into the script below.
        return sse_response(&[]);
    };
    let attr = NavSections::attr(open);
    let script = format!("document.documentElement.setAttribute('data-nav-{section}', '{attr}');");
    let mut resp = sse_response(&[sse_script(&script)]);
    resp.headers_mut()
        .append(header::SET_COOKIE, set_nav_sections_header(sections));
    resp
}

// ---------------------------------------------------------------------------
// HTML escaping.

/// Escape the five HTML-significant characters (`& < > " '`) so a string
/// can be spliced into markup as inert text. Shared by every hand-built
/// HTML fragment that isn't going through plait's auto-escaping (e.g. the
/// gateway's OIDC form fields and the DB layer's search-snippet
/// highlighter) so the escape set can't drift between copies.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
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
///
/// Anchored top-center (`toast-top toast-center`) rather than the daisyUI
/// default top-end: the top-right corner carries page chrome (the Upstreams
/// sticky "Apply changes" bar) and the bottom-right corner carries the feedback
/// FAB, both of which a corner-anchored toast (`z-index:70`) would sit on top of
/// and hide. Top-center clears both.
pub fn toast_container() -> Html {
    html! {
        div(id: "toasts", class: "toast toast-top toast-center") {}
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

/// The PWA `<head>` markup — manifest link, both theme-color metas,
/// apple-touch-icon, and favicon. Shared by every layout (this one and
/// the gateway's authed shell) so the manifest path, theme colors, and
/// icon links stay in lockstep; changing them in one place is enough.
///
/// Mirrored onto the login/error chrome too so that page is also
/// installable (Chrome's install prompt needs the manifest link present
/// on whatever page is open).
pub fn pwa_head_links() -> Html {
    html! {
        link(rel: "manifest", href: "/manifest.webmanifest");
        meta(name: "theme-color", content: "#1d1d1b", media: "(prefers-color-scheme: dark)");
        meta(name: "theme-color", content: "#ffffff", media: "(prefers-color-scheme: light)");
        link(rel: "apple-touch-icon", href: (assets::apple_touch_icon_url()));
        link(rel: "icon", href: "/favicon.ico");
    }
    .to_html()
}

/// Minimal `<html>` chrome — daisyUI stylesheet + datastar runtime +
/// a slot for `body`. No sidebar; used by the login/error pages and
/// anything else that doesn't sit inside the authed app shell.
///
/// `next` is the path the language switcher should redirect back to
/// after a change (see `render_lang_switcher_form`) — typically the
/// same path this page was requested at.
pub fn layout(theme: Theme, lang: Lang, next: &str, title: &str, body: Html) -> String {
    let theme_str = theme.as_str();
    let css_href = assets::app_css_url();
    let datastar_src = assets::datastar_js_url();
    let lang_code = lang.code();
    let frag = html! {
        html(lang: (lang_code), "data-theme": (theme_str), class: (theme_str)) {
            head {
                meta(charset: "utf-8");
                meta(name: "viewport", content: "width=device-width, initial-scale=1");
                title { (title) }
                link(rel: "stylesheet", href: (css_href));
                // PWA: manifest, theme-color, apple-touch-icon, favicon.
                (pwa_head_links())
                script(type: "module", src: (datastar_src)) {}
            }
            body(class: "min-h-dvh bg-base-100 text-base-content") {
                div(class: "absolute top-3 right-3") {
                    (render_lang_switcher_form(lang, next, LangPanelAnchor::Down))
                }
                (body)
                (toast_container())
            }
        }
    };
    frag.to_html().to_string()
}

/// `html_response(layout(...))`.
pub fn html_page(theme: Theme, lang: Lang, next: &str, title: &str, body: Html) -> Response {
    html_response(layout(theme, lang, next, title, body))
}
