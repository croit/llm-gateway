// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Server-rendered HTML pages for the rama gateway.
//!
//! Templates are inline plait `html! { ... }` macros — compile-time
//! validated, auto-escaping any interpolated `&str` / `String`.
//! daisyUI's component classes (and Tailwind utilities) give us the
//! design system without pulling in React; the CSS bundle is served by
//! `session_core::assets::app_css`.

use std::sync::Arc;

use plait::{Html, ToHtml, html};
use rama::http::service::web::extract::State;
use rama::http::{Body, Method, Request, Response, StatusCode, header};

use session_core::assets;
use session_core::chrome::{
    self, Flash, FlashKind, NavSections, Theme, html_response, read_body_to_bytes, see_other,
    sse_patch, sse_response, sse_script, sse_toast,
};
use session_core::i18n::{self, Lang, t, t_args};
use session_core::icons;

use gateway_core::rama_server::session::Session;
use gateway_core::server::db::users;
use gateway_runtime::rama_server::state::RamaState;

/// Resolve the caller's session or bail out of the handler. Expands to the
/// `require_session_or_redirect` match that early-`return`s the redirect
/// `Response` on failure — replacing the ~45 hand-written copies of that
/// 4-line block across the page handlers. The binding stays at the call site,
/// so every shape works:
/// `let (session, user) = require_session!(state, req);`
/// `let (_, user) = require_session!(state, req);`
///
/// Defined before the `mod …;` page declarations below so textual macro
/// scoping makes it available in every page submodule without an import.
/// `require_session_or_redirect` resolves in the caller's scope (each handler
/// already has it in scope), so no extra `use` is needed there either.
macro_rules! require_session {
    ($state:expr, $req:expr) => {
        match require_session_or_redirect(&$state, &$req).await {
            Ok(s) => s,
            Err(resp) => return resp,
        }
    };
}

// Two CSS classes (`.chat-prose` and `.thinking-prose`) carry the
// markdown styling for chat replies + reasoning blocks. See
// `ui/src/main.css` for the rule set — both share one parameterised
// block via CSS custom properties; the thinking variant overrides
// just the knobs (size, contrast, list indent…) plus the left rail.
//
// Theme, theme cookie, theme-toggle handler, FlashKind, sse_* helpers,
// the read-cookie + body-collector + see-other shims, and the bare
// `<html>` layout all live in `session_core::chrome` — both this
// crate and the orchestrator import them so the rendered chrome is
// byte-identical across binaries.

/// Which nav-bar entry is the currently-active page. The layout uses
/// this to put `tab-active` on the matching link so the daisyUI
/// `tabs-border` underline lands on the right item.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum NavItem {
    Tokens,
    Chat,
    /// Per-user tool on/off page (`/tools`). Shown to every signed-in
    /// user, not just admins.
    Tools,
    /// Per-user memory management page (`/memory`). Shown to every
    /// signed-in user.
    Memory,
    /// Per-user scheduled-actions page (`/scheduled`). Shown to every
    /// signed-in user; each manages their own schedules.
    Scheduled,
    /// Per-user webhooks page (`/webhooks`). Shown to every signed-in user;
    /// each manages their own inbound triggers.
    Webhooks,
    /// Per-user MCP connector store (`/integrations`). Shown to every
    /// signed-in user; each connects their own accounts (Google, GitHub, …).
    Integrations,
    /// Usage statistics (`/usage`). Shown to every signed-in user (scoped
    /// to their own requests); admins get an in-page "All users" toggle.
    Usage,
    /// Per-user **private** skills page (`/skills`). Shown to every signed-in
    /// user; each manages their own private Agent Skills, distinct from the
    /// admin-only global [`NavItem::Skills`] page.
    MySkills,
    /// Admin-only pages (model defaults, future operator tooling).
    /// The sidebar entry is only rendered for users whose `roles`
    /// includes `"admin"`; non-admins never see it.
    Admin,
    /// Admin-only merged upstream pools + backends page (`/admin/upstreams`).
    /// Same `admin`-role gate as [`NavItem::Admin`]; its own variant so the
    /// sidebar highlight lands on it rather than on Models. Replaced the old
    /// separate Backends + Pools entries.
    Upstreams,
    /// Admin-only RAG collection management page (`/rag`).
    Rag,
    /// Admin-only registered-users roster + impersonation (`/admin/users`).
    /// Same `admin`-role gate as the other admin entries.
    Users,
    /// Admin-only installed-skills overview (`/admin/skills`). Same
    /// `admin`-role gate as the other operator pages.
    Skills,
    /// Admin-only MCP connector catalog management (`/admin/connectors`).
    /// Same `admin`-role gate as the other operator pages.
    Connectors,
    /// Admin-only rate-limit / quota editor (`/admin/limits`). Same
    /// `admin`-role gate as the other operator pages.
    Limits,
    /// Admin-only ComfyUI workflow catalog (`/admin/comfyui`) — live
    /// snapshot of the loaded workflows, operator-triggered reload, and
    /// the per-workflow parameter surface. Same admin-role gate as the
    /// other operator pages.
    Comfyui,
    /// Admin-only gateway-groups editor (`/admin/groups`) — OIDC→group mappings
    /// plus per-group tool/skill grants. Same `admin`-role gate.
    Groups,
}

/// Datastar directive that intercepts the click and triggers an
/// `@get(href)` against the server. The server detects this via
/// `Datastar-Request: true` and returns SSE patches that swap
/// `<main>` + the sidebar + the title + `history.pushState` — no
/// full page reload.
fn nav_get_directive(href: &str) -> String {
    format!("@get('{href}')")
}

/// Same as `nav_get_directive`, plus the snippet that closes the
/// mobile drawer. Datastar morphs the sidebar across nav patches, so
/// just nav-patching doesn't close the slide-over — we have to flip
/// the drawer-toggle checkbox ourselves.
fn sidebar_nav_directive(href: &str) -> String {
    format!(
        "document.getElementById('app-sidebar-toggle').checked = false; {}",
        nav_get_directive(href)
    )
}

/// Render one `<option>`, marking it selected only when `selected` is true.
///
/// `selected`/`checked`/`readonly` are *presence-based* boolean HTML attributes:
/// a browser treats `selected="false"` as selected. plait's `attr: (bool)`
/// renders the literal `="false"` form, so it can't express "unselected" — the
/// attribute must be omitted entirely. These helpers do that, keeping the two
/// admin CRUD pages (backends/pools) and the capability selects correct.
pub(super) fn select_option(value: &str, label: &str, selected: bool) -> plait::Html {
    use plait::{ToHtml, html};
    if selected {
        html! { option(value: (value.to_string()), selected: "selected") { (label.to_string()) } }
            .to_html()
    } else {
        html! { option(value: (value.to_string())) { (label.to_string()) } }.to_html()
    }
}

/// Render a labelled `<input type=checkbox>`, emitting `checked` only when set
/// (see [`select_option`] for why the attribute is omitted rather than
/// `="false"`). `mono` renders the label in a monospace font (backend ids).
/// A standalone helper so the `html!` never captures the caller's locals — the
/// plait macro would otherwise move them into per-attribute closures.
pub(super) fn bool_checkbox(
    name: &str,
    value: &str,
    label: &str,
    checked: bool,
    mono: bool,
) -> plait::Html {
    use plait::{ToHtml, html};
    let span_class = if mono {
        "label-text text-sm font-mono"
    } else {
        "label-text text-sm"
    };
    let name = name.to_string();
    let value = value.to_string();
    let label = label.to_string();
    if checked {
        html! {
            label(class: "label cursor-pointer gap-2 justify-start") {
                input(type: "checkbox", name: (name), value: (value), checked: "checked", class: "checkbox checkbox-sm");
                span(class: (span_class)) { (label) }
            }
        }
        .to_html()
    } else {
        html! {
            label(class: "label cursor-pointer gap-2 justify-start") {
                input(type: "checkbox", name: (name), value: (value), class: "checkbox checkbox-sm");
                span(class: (span_class)) { (label) }
            }
        }
        .to_html()
    }
}

/// Render a `name` text input (the primary key of a CRUD row): read-only when
/// `readonly` (rename = delete + re-add), otherwise editable with a
/// `placeholder`. Standalone for the same reason as [`bool_checkbox`].
pub(super) fn pk_name_input(value: &str, placeholder: &str, readonly: bool) -> plait::Html {
    use plait::{ToHtml, html};
    let value = value.to_string();
    let placeholder = placeholder.to_string();
    if readonly {
        html! {
            input(type: "text", name: "name", value: (value), required: "required", readonly: "readonly", class: "input input-bordered input-sm font-mono w-full");
        }
        .to_html()
    } else {
        html! {
            input(type: "text", name: "name", value: (value), required: "required", placeholder: (placeholder), class: "input input-bordered input-sm font-mono w-full");
        }
        .to_html()
    }
}

/// One conversation in the sidebar list. Sourced from the persisted
/// `chat_sessions` rows — the chat handlers prefetch this, every
/// other authed handler does too so the sidebar is consistent across
/// the app.
pub(super) struct SidebarSession {
    pub id: String,
    pub title: Option<String>,
    /// Pinned conversations render with a filled star and sort to the top
    /// of the list (the DB query in `list_sessions` does the ordering).
    pub pinned: bool,
}

/// Everything the sidebar needs to render its lower half.
#[derive(Default)]
pub(super) struct SidebarChat {
    pub sessions: Vec<SidebarSession>,
    /// The currently-open session id, if the active page is /chat/{id}.
    /// Drives the row highlight.
    pub active_session_id: Option<String>,
}

/// Fetch the chat-sidebar payload for a user. Called from every
/// authed page handler so the sidebar conversation list is consistent
/// across the app (`+ New chat` works from anywhere). On a DB hiccup
/// we return an empty list rather than failing the whole page render
/// — the sidebar is chrome, not the primary content.
pub(super) async fn fetch_sidebar_chat(
    state: &RamaState,
    user_id: &str,
    active_session_id: Option<String>,
) -> SidebarChat {
    use session_core::db as chat;
    let sessions = chat::list_sessions(&state.db, user_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|s| SidebarSession {
            id: s.id,
            title: s.title,
            pinned: s.pinned,
        })
        .collect();
    SidebarChat {
        sessions,
        active_session_id,
    }
}

/// The global app sidebar. Replaces the old top nav-bar — brand at
/// the top, primary nav (Chat / Tools / Tokens), conversation list (always
/// rendered so "New chat" is reachable from any page), then a compact
/// user block at the bottom (email + theme toggle + sign-out).
///
/// Re-rendered as one unit on each nav patch — `nav_or_html_page`
/// outer-patches `#app-sidebar`. Keeps the implementation simple
/// (one selector, one render call) at the cost of re-emitting the
/// full conversation list per nav, which is acceptable for the small
/// per-user counts we expect.
/// True when the user holds any role flagged `admin = true` in config.
/// Used to gate `/admin/*` routes and conditionally render the Admin
/// sidebar entry.
///
/// `user.roles` holds the raw OIDC group claims (e.g. `"engineering"`,
/// `"platform-admins"`). We translate through the RBAC resolver to the
/// internal role IDs, then ask the resolver whether any of them carries
/// the admin capability — the role name is irrelevant.
pub(super) fn is_admin(state: &RamaState, user: &users::User) -> bool {
    let role_ids = state.rbac.role_ids_for(&user.roles);
    state.rbac.is_admin(&role_ids)
}

/// SSE response that fires a single toast. Shared feedback path for the
/// datastar action handlers (success / failure / no-op branches).
pub(super) fn toast(kind: FlashKind, message: impl Into<String>) -> Response {
    sse_response(&[sse_toast(&Flash {
        kind,
        message: message.into(),
    })])
}

/// A datastar `datastar-patch-signals` event that sets the `topologyDirty`
/// signal to `count`. The `/admin/upstreams` apply bar binds its visibility +
/// counter to this signal, so a pool/backend save or delete (and the reload
/// that clears it) updates the bar in place — no full page re-render. Shared by
/// the pools/backends save+delete handlers and the reload handler.
pub(super) fn dirty_signal(count: u32) -> rama::bytes::Bytes {
    session_core::chrome::sse_signals(&format!("{{topologyDirty: {count}}}"))
}

/// Read a request body and parse it as a urlencoded form. A
/// read/parse failure maps to a ready-to-return error toast, so handlers
/// can `match … { Ok(f) => f, Err(resp) => return resp }`. Centralises
/// the read+parse+toast boilerplate the datastar form handlers share.
pub(super) async fn read_form<T: serde::de::DeserializeOwned>(body: Body) -> Result<T, Response> {
    let bytes = read_body_to_bytes(body)
        .await
        .map_err(|msg| toast(FlashKind::Error, msg))?;
    serde_urlencoded::from_bytes(&bytes)
        .map_err(|err| toast(FlashKind::Error, format!("malformed form: {err}")))
}

/// The first value for `key` in a `Vec<(String, String)>` form body (parsed via
/// `read_form::<Vec<_>>`), or `""` if absent. The pairs form is the general one:
/// unlike a serde struct it preserves repeated keys — see [`fields_all`] for
/// multi-valued checkbox groups. Shared by the admin CRUD pages.
pub(super) fn field<'a>(pairs: &'a [(String, String)], key: &str) -> &'a str {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

/// Every value submitted for `key` (a multi-valued checkbox group, e.g. a pool's
/// `backends`), in submission order.
pub(super) fn fields_all(pairs: &[(String, String)], key: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .collect()
}

/// An unchecked HTML checkbox isn't submitted at all; a checked one submits its
/// value (`"on"` by default). Treat any present truthy value as checked.
pub(super) fn checkbox_on(v: &str) -> bool {
    matches!(v.trim(), "on" | "true" | "1" | "yes")
}

/// Parse a numeric form field, falling back to `default` on blank/invalid input.
pub(super) fn parse_u32(v: &str, default: u32) -> u32 {
    v.trim().parse().unwrap_or(default)
}

/// Split a comma-separated field into trimmed, non-empty entries.
pub(super) fn parse_csv(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn render_app_sidebar(
    active: Option<NavItem>,
    user_email: &str,
    is_admin: bool,
    skills_enabled: bool,
    theme: Theme,
    lang: Lang,
    chat: &SidebarChat,
) -> Html {
    let user_email = user_email.to_string();
    let sessions: Vec<SidebarSession> = chat
        .sessions
        .iter()
        .map(|s| SidebarSession {
            id: s.id.clone(),
            title: s.title.clone(),
            pinned: s.pinned,
        })
        .collect();
    let active_sess = chat.active_session_id.clone();
    let source_line = t_args(
        lang,
        "nav-source-line",
        &i18n::args([("version", crate::build_info::version_label().into())]),
    );
    html! {
        aside(id: "app-sidebar", class: "app-sidebar") {
            div(class: "app-sidebar__brand") {
                a(
                    href: "/",
                    class: "app-sidebar__brand-link",
                    "data-on:click__prevent": (sidebar_nav_directive("/"))
                ) {
                    "LLM Gateway"
                }
            }
            // Chat is the hero entry — always visible, never inside a
            // collapsible group. The rest are grouped (Workspace /
            // Account / Admin); each group folds independently, with the
            // open/closed state persisted via the `nav_sections` cookie
            // and driven purely by `<html data-nav-*>` + CSS (see
            // `nav_group`), so a nav patch that re-renders this sidebar
            // never has to know the fold state.
            nav(class: "app-sidebar__primary") {
                (sidebar_nav_link("/chat", NavItem::Chat, active, icons::message(16), &t(lang, "nav-chat")))
                (nav_group(lang, "workspace", &t(lang, "nav-group-workspace"), html! {
                    (sidebar_nav_link("/memory", NavItem::Memory, active, icons::folder(16), &t(lang, "nav-memory")))
                    (sidebar_nav_link("/scheduled", NavItem::Scheduled, active, icons::clock(16), &t(lang, "nav-scheduled")))
                    (sidebar_nav_link("/webhooks", NavItem::Webhooks, active, icons::send(16), &t(lang, "nav-webhooks")))
                    (sidebar_nav_link("/integrations", NavItem::Integrations, active, icons::plug(16), &t(lang, "nav-integrations")))
                    // Hidden unless private skills are usable (configured + the
                    // directory is accessible) — see `AppState::user_skills_enabled`.
                    if skills_enabled {
                        (sidebar_nav_link("/skills", NavItem::MySkills, active, icons::sparkles(16), &t(lang, "nav-my-skills")))
                    }
                    (sidebar_nav_link("/tools", NavItem::Tools, active, icons::sliders(16), &t(lang, "nav-tools")))
                }.to_html()))
                (nav_group(lang, "account", &t(lang, "nav-group-account"), html! {
                    (sidebar_nav_link("/tokens", NavItem::Tokens, active, icons::key(16), &t(lang, "nav-tokens")))
                    (sidebar_nav_link("/usage", NavItem::Usage, active, icons::chart(16), &t(lang, "nav-usage")))
                }.to_html()))
                if is_admin {
                    (nav_group(lang, "admin", &t(lang, "nav-group-admin"), html! {
                        (sidebar_nav_link("/admin/users", NavItem::Users, active, icons::users(16), &t(lang, "nav-users")))
                        (sidebar_nav_link("/admin/groups", NavItem::Groups, active, icons::users(16), &t(lang, "nav-groups")))
                        (sidebar_nav_link("/admin/upstreams", NavItem::Upstreams, active, icons::cube(16), &t(lang, "nav-upstreams")))
                        (sidebar_nav_link("/admin/models", NavItem::Admin, active, icons::cpu(16), &t(lang, "nav-models")))
                        (sidebar_nav_link("/rag", NavItem::Rag, active, icons::database(16), &t(lang, "nav-rag")))
                        (sidebar_nav_link("/admin/skills", NavItem::Skills, active, icons::sparkles(16), &t(lang, "nav-skills")))
                        (sidebar_nav_link("/admin/connectors", NavItem::Connectors, active, icons::plug(16), &t(lang, "nav-connectors")))
                        (sidebar_nav_link("/admin/comfyui", NavItem::Comfyui, active, icons::sparkles(16), "ComfyUI"))
                        (sidebar_nav_link("/admin/limits", NavItem::Limits, active, icons::sliders(16), &t(lang, "nav-limits")))
                    }.to_html()))
                }
            }
            div(class: "app-sidebar__sessions-section", "data-signals": "{searchQuery: '', searchOpen: false}") {
                div(class: "app-sidebar__sessions-header") {
                    span(class: "app-sidebar__sessions-label") { (t(lang, "nav-conversations-label")) }
                    div(class: "app-sidebar__sessions-actions") {
                        // Search toggle. Clicking reveals the input row below
                        // the header and focuses it; the icon alone lives in
                        // the header. No-JS clients fall back to the always-
                        // visible input (the `data-show` below does nothing
                        // without Datastar).
                        button(
                            type: "button",
                            class: "app-sidebar__search-toggle",
                            "aria-label": (t(lang, "nav-search-aria")),
                            title: (t(lang, "nav-search-title")),
                            "data-on:click":
                                "$searchOpen = !$searchOpen; $searchOpen && requestAnimationFrame(() => el.closest('.app-sidebar__sessions-section').querySelector('.app-sidebar__search-input').focus())"
                        ) {
                            (icons::search(14))
                        }
                        form(
                            method: "post",
                            action: "/chat/sessions",
                            "data-on:submit__prevent":
                                "document.getElementById('app-sidebar-toggle').checked = false; @post('/chat/sessions', {contentType: 'form'})",
                            class: "m-0"
                        ) {
                            button(
                                type: "submit",
                                class: "app-sidebar__new-btn",
                                "aria-label": (t(lang, "nav-new-conversation-aria")),
                                title: (t(lang, "nav-new-conversation-title"))
                            ) {
                                (icons::plus(14))
                            }
                        }
                    }
                }
                // Conversation search input, hidden until the toggle above
                // opens it. Typing searches live: `data-on:input` fires the
                // same Datastar `@get` after a 500ms debounce, so results
                // stream in without pressing Enter (submit still works as a
                // fallback and for the no-JS path). The `@get` (NOT a native
                // GET) makes the server see `Datastar-Request: true` and answer
                // with an SSE patch of `#session-list` rather than a full-page
                // navigation. The query rides in `$searchQuery`, url-encoded
                // into the request path. On the no-JS path the plain
                // `action`/`method` still submit, and the handler serves a
                // full results page. Escape closes the input.
                div(class: "app-sidebar__search-row", "data-show": "$searchOpen") {
                    form(
                        id: "sidebar-search-form",
                        method: "get",
                        action: "/chat/search",
                        class: "app-sidebar__search-form m-0",
                        "data-on:submit__prevent":
                            "@get('/chat/search?q=' + encodeURIComponent($searchQuery))"
                    ) {
                        input(
                            type: "text",
                            name: "q",
                            placeholder: (t(lang, "nav-search-placeholder")),
                            class: "input input-sm app-sidebar__search-input",
                            "data-bind": "searchQuery",
                            "data-on:input__debounce.500ms":
                                "@get('/chat/search?q=' + encodeURIComponent($searchQuery))",
                            "data-on:keydown": "evt.key === 'Escape' && ($searchOpen = false)"
                        );
                    }
                }
                (render_session_list(&sessions, active_sess.as_deref(), lang))
            }
            div(class: "app-sidebar__user") {
                span(class: "app-sidebar__email") { (user_email) }
                div(class: "app-sidebar__user-actions") {
                    (chrome::render_lang_switcher_form(lang, "/", chrome::LangPanelAnchor::Up))
                    (chrome::render_theme_toggle_form(theme, lang))
                    form(
                        method: "post",
                        action: "/auth/logout",
                        class: "m-0"
                    ) {
                        button(
                            type: "submit",
                            class: "btn btn-ghost btn-square btn-sm",
                            title: (t(lang, "nav-sign-out")),
                            "aria-label": (t(lang, "nav-sign-out"))
                        ) {
                            (icons::logout(16))
                        }
                    }
                }
            }
            // AGPL-3.0 §13: offer network users the corresponding source of
            // the running build. Links to the repo (env-overridable for forks)
            // pinned to the built version + commit.
            div(class: "px-4 py-2 border-t border-base-300/60") {
                a(
                    href: (crate::build_info::source_url()),
                    class: "text-[11px] leading-tight text-base-content/45 link link-hover",
                    target: "_blank",
                    rel: "noopener noreferrer",
                    title: (t(lang, "nav-source-title"))
                ) {
                    (source_line)
                }
            }
        }
    }
    .to_html()
}

/// One top-nav row in the sidebar. Subtle active treatment — a
/// soft tinted background + slightly bolder weight, not daisyUI's
/// inverted-color `menu-active`.
fn sidebar_nav_link(
    href: &str,
    item: NavItem,
    active: Option<NavItem>,
    icon: Html,
    label: &str,
) -> Html {
    let selected = Some(item) == active;
    let class = if selected {
        "app-sidebar__nav-link app-sidebar__nav-link--active"
    } else {
        "app-sidebar__nav-link"
    };
    let label = label.to_string();
    let href = href.to_string();
    let directive = sidebar_nav_directive(&href);
    html! {
        a(href: (href), class: (class), "data-on:click__prevent": (directive)) {
            (icon)
            span { (label) }
        }
    }
    .to_html()
}

/// A collapsible group of nav links with an uppercase header + a
/// chevron. The header `POST`s to `/nav/toggle/{key}`, which flips the
/// `nav_sections` cookie and sets `<html data-nav-{key}>` in place; the
/// CSS (`html[data-nav-{key}="closed"] .app-sidebar__group[data-group=…]`)
/// hides the items + rotates the chevron. Rendering is stateless — the
/// markup is identical open or closed, so a nav patch that re-renders the
/// sidebar doesn't need to know the fold state (it lives on `<html>`,
/// which patches never touch).
fn nav_group(lang: Lang, key: &str, label: &str, items: Html) -> Html {
    let key = key.to_string();
    let label = label.to_string();
    let directive = format!("@post('/nav/toggle/{key}')");
    let aria = t_args(
        lang,
        "nav-group-toggle-aria",
        &i18n::args([("label", label.clone().into())]),
    );
    html! {
        div(class: "app-sidebar__group", "data-group": (key)) {
            button(
                type: "button",
                class: "app-sidebar__group-header",
                "data-on:click__prevent": (directive),
                "aria-label": (aria)
            ) {
                span(class: "app-sidebar__group-label") { (label) }
                span(class: "app-sidebar__group-chevron") { (icons::chevron_down(14)) }
            }
            div(class: "app-sidebar__group-items") {
                (items)
            }
        }
    }
    .to_html()
}

/// The `<ul>` of conversation rows. Pulled out of `render_app_sidebar` so
/// the pin toggle can re-patch just `#session-list` (the pin re-sorts the
/// list — pinned rows float to the top — so a single-row patch won't do).
fn render_session_list(sessions: &[SidebarSession], active_id: Option<&str>, lang: Lang) -> Html {
    html! {
        ul(id: "session-list", class: "app-sidebar__sessions") {
            for s in sessions.iter() {
                (render_sidebar_session(s, active_id, lang, None))
            }
        }
    }
    .to_html()
}

/// Render search results into the sidebar's `#session-list` (the SSE patch
/// replaces the normal list in place). Each row is a *full* sidebar row —
/// same pin/delete forms, active-row highlight, and `#session-row-{id}` id
/// as [`render_sidebar_session`] — with the match snippet appended, so
/// searching never strips the sidebar's affordances.
pub(super) fn render_search_results(hits: &[session_core::db::SearchHit], lang: Lang) -> Html {
    html! {
        ul(id: "session-list", class: "app-sidebar__sessions") {
            for h in hits.iter() {
                (render_search_hit_row(h, lang))
            }
        }
    }
    .to_html()
}

/// One search-result row: the same full sidebar row (pin/delete/active/id)
/// plus the highlighted snippet. Delegates to [`render_sidebar_session`] so
/// the two paths can't drift.
fn render_search_hit_row(hit: &session_core::db::SearchHit, lang: Lang) -> Html {
    let s = SidebarSession {
        id: hit.session_id.clone(),
        title: hit.title.clone(),
        pinned: hit.pinned,
    };
    let snippet = (!hit.snippet.is_empty()).then_some(hit.snippet.as_str());
    // No active row: search is issued from any page and the patch carries no
    // "currently open" session context.
    render_sidebar_session(&s, None, lang, snippet)
}

/// Full-page search-results body for the no-JS fallback path (a plain GET
/// to `/chat/search`). The JS path never hits this — it SSE-patches the
/// sidebar's `#session-list` in place. Rendered into the main content
/// column with the query echoed back so the page doesn't look like the
/// search was ignored.
pub(super) fn render_search_page_body(
    query: &str,
    hits: &[session_core::db::SearchHit],
    lang: Lang,
) -> Html {
    html! {
        div(class: "p-4 max-w-3xl mx-auto w-full") {
            h1(class: "text-lg font-semibold mb-3") {
                (t_args(lang, "nav-search-results-heading", &i18n::args([("query", query.to_string().into())])))
            }
            if hits.is_empty() {
                p(class: "opacity-60 text-sm") { (t(lang, "nav-search-no-results")) }
            } else {
                ul(class: "app-sidebar__sessions") {
                    for h in hits.iter() {
                        (render_search_hit_row(h, lang))
                    }
                }
            }
        }
    }
    .to_html()
}

/// One conversation row in the sidebar. Hover reveals the pin + delete
/// buttons (a pinned row keeps its star lit); active row gets a soft
/// tinted background. `snippet`, when present, is a pre-escaped highlight
/// excerpt (search results) appended below the title — see
/// [`render_search_hit_row`].
fn render_sidebar_session(
    s: &SidebarSession,
    active_id: Option<&str>,
    lang: Lang,
    snippet: Option<&str>,
) -> Html {
    let id = s.id.clone();
    let row_id = format!("session-row-{id}");
    let href = format!("/chat/{id}");
    let delete_url = format!("/chat/{id}/delete");
    let pin_url = format!("/chat/{id}/pin");
    let directive = sidebar_nav_directive(&href);
    let delete_directive = format!("@post('{delete_url}', {{contentType: 'form'}})");
    let pin_directive = format!("@post('{pin_url}', {{contentType: 'form'}})");
    let title = s
        .title
        .clone()
        .unwrap_or_else(|| t(lang, "nav-untitled-chat"));
    let is_active = active_id == Some(&s.id);
    let row_class = if is_active {
        "session-row session-row--active"
    } else {
        "session-row"
    };
    // Carry the *currently active* session id in the pin form so the
    // handler — which re-renders the whole `#session-list` — can preserve
    // the active-row highlight (the pin POST itself doesn't navigate). The
    // sidebar is global, so this is empty on non-chat pages (no active row).
    let active_field = active_id.unwrap_or("").to_string();
    let pin_class = if s.pinned {
        "session-row__pin session-row__pin--active"
    } else {
        "session-row__pin"
    };
    let (pin_label, pin_icon) = if s.pinned {
        (t(lang, "nav-unpin-conversation"), icons::star_filled(12))
    } else {
        (t(lang, "nav-pin-conversation"), icons::star(12))
    };
    let delete_label = t(lang, "nav-delete-conversation");
    let snippet = snippet.map(str::to_string);
    html! {
        li(id: (row_id), class: "session-row__item") {
            // The whole row is the clickable target so a sloppy
            // mobile tap on the padding doesn't fall through. The
            // pin + delete forms sit as siblings, absolutely positioned
            // over the right edge — clicks on those buttons don't bubble
            // through the link.
            a(
                href: (href),
                class: (row_class),
                "data-on:click__prevent": (directive)
            ) {
                span(class: "session-row__title") { (title) }
                // Search-result snippet: pre-escaped at the DB layer (only
                // the `<b>` highlight is live markup — see
                // `db::highlight_snippet`), so splicing it raw is XSS-safe.
                if let Some(snip) = snippet.as_deref() {
                    span(class: "session-row__snippet") { #(snip.to_string()) }
                }
            }
            form(
                method: "post",
                action: (pin_url),
                "data-on:submit__prevent": (pin_directive),
                class: "m-0 session-row__pin-form"
            ) {
                input(type: "hidden", name: "active", value: (active_field));
                button(
                    type: "submit",
                    class: (pin_class),
                    "aria-label": (pin_label.clone()),
                    title: (pin_label)
                ) {
                    (pin_icon)
                }
            }
            form(
                method: "post",
                action: (delete_url),
                "data-on:submit__prevent": (delete_directive),
                class: "m-0 session-row__delete-form"
            ) {
                button(
                    type: "submit",
                    class: "session-row__delete",
                    "aria-label": (delete_label.clone()),
                    title: (delete_label)
                ) {
                    (icons::trash(12))
                }
            }
        }
    }
    .to_html()
}

/// For an authed page: return the full HTML page on a normal browser
/// load, or SSE patches (main + sidebar + title + history.pushState)
/// on a datastar-driven navigation.
///
/// Same body fragment in both branches — the SSE path just wraps the
/// body in a fresh `<main>` (so the class can flip between the chat
/// layout and the default), re-renders the global sidebar (active
/// nav item + active conversation row), updates `<title>`, and
/// pushes the URL.
#[allow(clippy::too_many_arguments)]
/// Request-scoped page chrome shared by every authed index handler: the
/// header-derived display prefs (`theme`/`lang`/`nav`/`datastar`) plus the
/// session/user-derived nav state (`user_email`/`is_admin`/`skills_enabled`/
/// `impersonating`). Bundled so [`nav_or_html_page`] takes one `&PageCtx`
/// instead of the old 13 positional args (four of them `bool`/enum that were
/// trivial to transpose). Build it with [`PageCtx::new`] for the common case;
/// override a field with struct-update syntax where a handler differs (e.g.
/// admin-only pages that force `is_admin: true`).
pub(super) struct PageCtx {
    pub theme: Theme,
    pub lang: Lang,
    pub nav: NavSections,
    pub datastar: bool,
    pub user_email: String,
    pub is_admin: bool,
    pub skills_enabled: bool,
    pub impersonating: bool,
}

fn nav_or_html_page(
    p: &PageCtx,
    active: NavItem,
    title: &str,
    body: Html,
    url: &str,
    chat: &SidebarChat,
) -> Response {
    if !p.datastar {
        return html_authed_page(
            p.theme,
            p.lang,
            p.nav,
            Some(active),
            title,
            &p.user_email,
            p.is_admin,
            p.skills_enabled,
            p.impersonating,
            body,
            chat,
        );
    }
    let main_class = main_class_for(Some(active));
    let main_html = html! {
        main(class: (main_class)) { (body) }
    }
    .to_html()
    .to_string();
    let title_html = html! { title { (title) } }.to_html().to_string();
    let sidebar_html = render_app_sidebar(
        Some(active),
        &p.user_email,
        p.is_admin,
        p.skills_enabled,
        p.theme,
        p.lang,
        chat,
    )
    .to_string();
    let push_url = serde_json::to_string(url).expect("url is JSON-encodable");
    // After the patch lands, push the URL and — if this page has a chat
    // composer (`#message`, only on /chat) — focus it so the user can
    // type immediately. The `if (m)` guard makes it a no-op on every
    // other page. `autofocus` covers the full-page-load path; this
    // covers the Datastar nav path (+ New chat / switching chats).
    let script = format!(
        "history.pushState(null, '', {push_url}); \
         {{ const m = document.getElementById('message'); if (m) m.focus(); }}"
    );
    sse_response(&[
        sse_patch(Some("main"), Some("outer"), &main_html),
        sse_patch(Some("#app-sidebar"), Some("outer"), &sidebar_html),
        sse_patch(Some("title"), Some("outer"), &title_html),
        sse_script(&script),
    ])
}

/// Tailwind classes for the page's `<main>`. The chat page wants a
/// full-height flex column so the composer can be the last
/// `flex-shrink-0` item (and the conversation can scroll internally
/// inside the middle slot); everywhere else takes the normal
/// scrollable-block-with-vertical-padding layout.
fn main_class_for(active: Option<NavItem>) -> &'static str {
    match active {
        // Full-width: the chat + docked canvas (`.chat-shell`) span the whole
        // content area so a pulled-out canvas — and the chat beside it — use
        // all the horizontal space instead of being boxed into a centered
        // reading column with dead gutters. The conversation keeps its own
        // readable max-width *inside* `.chat-col` (see main.css), so with the
        // canvas closed it still centers like before; with the canvas open the
        // column simply fills the space the gutters used to waste.
        //
        // No bottom padding at any size: the composer floats
        // absolutely over the conversation (see main.css), so any
        // page-bg padding under it reads as a sticky "bar". On
        // phone we also drop the top padding because the floating
        // drawer-button takes the same role. Clearance for both
        // floating elements is moved into `#conversation`'s own
        // padding so messages don't sit permanently behind them.
        Some(NavItem::Chat) => {
            // `px-2` on phone (was `px-4`) so the conversation + composer use
            // nearly the full viewport width — 16px gutters each side wasted a
            // lot on a narrow screen; `sm:px-6` restores roomy gutters on wider
            // viewports.
            "chat-main relative flex-1 min-h-0 flex flex-col w-full px-2 sm:px-6 sm:pt-4"
        }
        _ => "flex-1 min-h-0 overflow-y-auto",
    }
}

// The plain (non-authed) `layout` + `html_page` live in
// `session_core::chrome` — used by the login page.

/// Authed equivalent of `html_page` — wraps body in the layout with
/// the global sidebar, theme toggle, and conversation list. `active`
/// marks the currently-selected primary-nav item (None for pages
/// that don't belong to one, like the error pages).
#[allow(clippy::too_many_arguments)]
fn html_authed_page(
    theme: Theme,
    lang: Lang,
    nav: NavSections,
    active: Option<NavItem>,
    title: &str,
    user_email: &str,
    is_admin: bool,
    skills_enabled: bool,
    impersonating: bool,
    body: Html,
    chat: &SidebarChat,
) -> Response {
    let html = layout_authed(
        theme,
        lang,
        nav,
        active,
        title,
        user_email,
        is_admin,
        skills_enabled,
        impersonating,
        body,
        chat,
    );
    html_response(html)
}

/// Page chrome with the global sidebar (daisyUI drawer — pinned on
/// large screens, slide-over on mobile). Replaces the old top
/// nav-bar: brand + primary nav + conversation list + user controls
/// all live in one column. Used by every page that's behind auth.
#[allow(clippy::too_many_arguments)]
fn layout_authed(
    theme: Theme,
    lang: Lang,
    nav: NavSections,
    active: Option<NavItem>,
    title: &str,
    user_email: &str,
    is_admin: bool,
    skills_enabled: bool,
    impersonating: bool,
    body: Html,
    chat: &SidebarChat,
) -> String {
    let theme_str = theme.as_str();
    let lang_code = lang.code();
    let css_href = assets::app_css_url();
    let datastar_src = assets::datastar_js_url();
    let app_src = assets::app_js_url();
    let pcm_recorder = assets::pcm_recorder_js_url();
    let main_class = main_class_for(active);
    // Sidebar nav-group fold state rides on `<html>` (NOT inside the
    // nav-patched `#app-sidebar`) so it survives SPA navigation — the
    // collapse CSS keys off these attributes; `nav_sections_toggle`
    // flips them in place.
    let nav_workspace = NavSections::attr(nav.workspace);
    let nav_account = NavSections::attr(nav.account);
    let nav_admin = NavSections::attr(nav.admin);
    let frag = html! {
        html(
            lang: (lang_code),
            "data-theme": (theme_str),
            class: (theme_str),
            "data-nav-workspace": (nav_workspace),
            "data-nav-account": (nav_account),
            "data-nav-admin": (nav_admin)
        ) {
            head {
                meta(charset: "utf-8");
                meta(name: "viewport", content: "width=device-width, initial-scale=1");
                title { (title) }
                link(rel: "stylesheet", href: (css_href));
                // PWA: manifest, theme-color, apple-touch-icon, favicon.
                (chrome::pwa_head_links())
                // app.js defines the `window.chat*` globals (chatScroll,
                // chatComposer, …). It MUST execute before datastar: datastar
                // processes `data-init` (e.g. `window.chatScroll.init(el)` on
                // #conversation) during its own module execution, so if app.js
                // ran later — it used to sit at body-end — that init threw
                // "chatScroll is undefined". Both are deferred, so they run in
                // document order; placing app.js first guarantees the globals
                // exist when datastar mounts the DOM. `defer` still runs it
                // after parse, so its own DOM wiring sees the page.
                script(src: (app_src), defer: "defer", "data-pcm-recorder": (pcm_recorder)) {}
                script(type: "module", src: (datastar_src)) {}
            }
            // The whole authed app lives inside a daisyUI drawer.
            // `lg:drawer-open` pins the sidebar on >= 1024px; below
            // that it becomes a slide-over toggled by the hamburger
            // in `.app-mobile-bar`.
            body(class: "bg-base-100 text-base-content") {
                // `overflow-hidden` on the shell + `min-h-0` on the
                // grid items below keeps the body itself from ever
                // scrolling — instead, the page content (chat
                // conversation, tokens list, dashboard card) scrolls
                // internally while the sidebar stays sticky. Without
                // this daisyUI's drawer leaves drawer-content's
                // height content-driven, the body scrolls when
                // content overflows viewport, and the "sticky"
                // sidebar slides off-screen with the document.
                div(class: "app-shell drawer lg:drawer-open h-dvh overflow-hidden") {
                    input(
                        id: "app-sidebar-toggle",
                        type: "checkbox",
                        class: "drawer-toggle"
                    );
                    div(class: "drawer-content relative flex flex-col min-w-0 min-h-0 overflow-hidden") {
                        // Floating drawer-open trigger. Only shown on
                        // mobile (`lg:hidden`); on large screens the
                        // sidebar is already pinned. Positioned over
                        // the chat content so we don't reserve a
                        // dedicated top strip for it — every pixel
                        // counts on a phone above the keyboard. The
                        // open drawer-side itself takes the same `for`
                        // target via the drawer-overlay label so the
                        // close gesture still works.
                        label(
                            "for": "app-sidebar-toggle",
                            class: "app-mobile-menu-btn lg:hidden",
                            "aria-label": (t(lang, "nav-open-menu-aria"))
                        ) {
                            (icons::menu(18))
                        }
                        // Impersonation banner. A sibling of `main` (not
                        // inside it) so Datastar in-page navigation — which
                        // outer-patches `main`, `#app-sidebar`, and `title`
                        // but nothing else — leaves it standing for the whole
                        // impersonation session. Only full-page loads (the
                        // start/stop redirects) re-render the shell, which is
                        // exactly when the banner should appear or vanish.
                        if impersonating {
                            (render_impersonation_banner(user_email, lang))
                        }
                        main(class: (main_class)) {
                            (body)
                        }
                        // Feedback widget. Siblings of `main` (like the
                        // impersonation banner) so they survive Datastar SPA
                        // navigation — rendered once on full load, persist
                        // across in-page nav patches. The FAB starts hidden;
                        // `feedback.ts` reveals it once `/feedback/config`
                        // confirms the feature is configured.
                        (feedback::render_fab(lang))
                        (feedback::render_dialog(lang))
                        (feedback::render_confirm(lang))
                    }
                    div(class: "drawer-side z-40") {
                        label(
                            "for": "app-sidebar-toggle",
                            "aria-label": (t(lang, "nav-close-menu-aria")),
                            class: "drawer-overlay"
                        ) {}
                        (render_app_sidebar(active, user_email, is_admin, skills_enabled, theme, lang, chat))
                    }
                }
                (chrome::toast_container())
            }
        }
    };
    frag.to_html().to_string()
}

/// Persistent, deliberately loud banner shown on every authed page while
/// the current session is an admin impersonation. `email` is the *target*
/// being acted as (the session's effective user). The "Return to your
/// account" control is a plain form POST to `/impersonate/stop` — no
/// Datastar — so it triggers a full navigation that re-renders the shell
/// without the banner and drops the impersonation cookie. Full
/// impersonation is unrestricted by design (the admin can act entirely as
/// the user); this banner + the impersonation_audit trail are the
/// accountability controls.
fn render_impersonation_banner(email: &str, lang: Lang) -> Html {
    let email = email.to_string();
    let prefix = t(lang, "impersonation-banner-prefix");
    html! {
        div(
            id: "impersonation-banner",
            class: "shrink-0 flex items-center gap-3 px-4 py-2 \
                    bg-warning text-warning-content border-b border-warning/40",
            role: "alert"
        ) {
            (icons::alert(18))
            span(class: "text-sm font-medium min-w-0 break-words") {
                (prefix) " " strong { (email) } "."
            }
            form(
                method: "post",
                action: "/impersonate/stop",
                class: "m-0 ml-auto shrink-0"
            ) {
                button(type: "submit", class: "btn btn-sm") {
                    (t(lang, "impersonation-return-button"))
                }
            }
        }
    }
    .to_html()
}

// The toast auto-dismiss + voice-composer glue lives in
// `crates/session-core/assets/app.js`, served via `session_core::assets::app_js`.

/// Admin gate. Wraps `require_session_or_redirect` + checks the
/// `admin` role. Anonymous → /login redirect (standard
/// not-logged-in flow); logged-in-but-not-admin → 403 page (don't
/// bounce them to /login, they'd just loop). Returns the user on
/// success so the caller doesn't have to look it up again.
pub(super) async fn require_admin_or_403(
    state: &RamaState,
    req: &Request,
) -> Result<(Session, users::User), Response> {
    let (session, user) = require_session_or_redirect(state, req).await?;
    if !is_admin(state, &user) {
        return Err(forbidden_html(&user.email, "admin role required"));
    }
    Ok((session, user))
}

/// Auth gate that redirects to /login on miss (vs the API gate which
/// returns 401 JSON). Returns either the resolved session or the
/// redirect Response that the caller should `return`.
pub(super) async fn require_session_or_redirect(
    state: &RamaState,
    req: &Request,
) -> Result<(Session, users::User), Response> {
    let session = match state.sessions.lookup_from_headers(req.headers()).await {
        Ok(Some(s)) => s,
        Ok(None) => return Err(login_redirect(req)),
        Err(err) => {
            tracing::warn!(error = %err, "session lookup");
            return Err(login_redirect(req));
        }
    };
    match users::find_by_id(&state.db, &session.user_id).await {
        Ok(Some(u)) => Ok((session, u)),
        Ok(None) | Err(_) => Err(login_redirect(req)),
    }
}

/// Bounce an unauthenticated request to `/login`, preserving the originally
/// requested URL as `?return_to=…` so a deep link — e.g. a shared chat handed
/// to a colleague who isn't signed in yet — survives the OIDC round-trip
/// instead of dumping the user on the default surface (`/chat`, i.e. *their*
/// latest/new conversation). Only GETs to same-origin paths are carried; a
/// non-GET (no point replaying a POST after login) or an odd target falls back
/// to a bare `/login`. `/auth/login` + the callback re-validate `return_to` and
/// only honour same-origin `/`-paths, so this can't become an open redirect.
fn login_redirect(req: &Request) -> Response {
    if req.method() == Method::GET
        && let Some(path_and_query) = req.uri().path_and_query().map(|pq| pq.as_str())
        && gateway_core::rama_server::session::is_safe_return_to(path_and_query)
        && !path_and_query.starts_with("/login")
        && let Ok(query) = serde_urlencoded::to_string([("return_to", path_and_query)])
    {
        return see_other(&format!("/login?{query}"));
    }
    see_other("/login")
}

/// GET /login — the standalone sign-in page: a single centered Card
/// with the "Continue with OIDC" button.
pub async fn login(State(_state): State<Arc<RamaState>>, req: Request) -> Response {
    let theme = Theme::from_headers(req.headers());
    let lang = Lang::from_headers(req.headers());
    // Carry a deep-link target across the OIDC round-trip. `login_redirect`
    // sends unauthenticated deep links here as `?return_to=/path`; forward that
    // into the GET form as a hidden field so `/auth/login` persists it and the
    // callback lands the user back on the page they actually requested (e.g. a
    // shared chat) rather than the default surface. Same-origin paths only —
    // the same guard `/auth/login` and the callback apply.
    let return_to = req
        .uri()
        .query()
        .and_then(|q| serde_urlencoded::from_str::<LoginPageQuery>(q).ok())
        .and_then(|q| q.return_to)
        .filter(|rt| gateway_core::rama_server::session::is_safe_return_to(rt));
    let body = html! {
        main(class: "min-h-dvh flex items-center justify-center p-8") {
            div(class: "card border border-base-300 w-full max-w-md") {
                div(class: "card-body") {
                    h2(class: "card-title text-2xl") { (t(lang, "login-heading")) }
                    p(class: "text-base-content/70") {
                        (t(lang, "login-description"))
                    }
                    form(action: "/auth/login", method: "get", class: "mt-2") {
                        if let Some(rt) = return_to.as_ref() {
                            input(type: "hidden", name: "return_to", value: (rt.clone()));
                        }
                        button(
                            type: "submit",
                            class: "btn btn-primary btn-block"
                        ) {
                            (t(lang, "login-continue-button"))
                        }
                    }
                    // AGPL-3.0 §13 source offer, also reachable pre-login.
                    p(class: "mt-4 text-center text-xs text-base-content/45") {
                        a(
                            href: (crate::build_info::source_url()),
                            class: "link link-hover",
                            target: "_blank",
                            rel: "noopener noreferrer"
                        ) {
                            (t(lang, "login-source-link"))
                        }
                    }
                }
            }
        }
    }
    .to_html();
    chrome::html_page(theme, lang, "/login", "Sign in — LLM Gateway", body)
}

/// Query for the `/login` page — the optional deep-link target threaded through
/// from `login_redirect` and forwarded into the `/auth/login` form.
#[derive(serde::Deserialize)]
struct LoginPageQuery {
    return_to: Option<String>,
}

// `theme_toggle` lives in `session_core::chrome::theme_toggle`; the
// router mounts it directly.

// ---------------------------------------------------------------------------
// Chat
//
// Composer, message-send + tail SSE endpoints, tool-call loop, and the bubble
// renderers all live in `chat.rs`. We pub-re-export the four handler
// entry points so the router (which calls `pages::chat_index` etc.)
// doesn't have to know about the split.
mod chat;
pub use chat::{
    chat_attachment, chat_attachment_remove, chat_cancel, chat_capabilities_toggle,
    chat_document_edit, chat_document_view, chat_edit, chat_effort_set, chat_export_markdown,
    chat_export_pdf, chat_fork, chat_index, chat_message_send, chat_retry, chat_search,
    chat_session_create, chat_session_delete, chat_session_pin, chat_session_view,
    chat_share_toggle, chat_tail, chat_turn_thinking,
};

// SSE helpers (`sse_patch`, `sse_script`, `sse_signals`,
// `sse_response`, `sse_toast`) live in `session_core::chrome` — both
// binaries use the exact same wire format, so any drift between
// gateway and orchestrator would be a bug.

// ---------------------------------------------------------------------------
// Tokens
//
// CRUD handlers, the list + row + minted-banner renderers all live in
// `tokens.rs`. Re-export the four handler entry points so the router
// continues to call `pages::tokens_index` etc. without any change.
// Reusable tool on/off toggle list shared by /tools and the /tokens
// per-token panel (`tool_toggles`). The resolver helpers are re-exported
// so the JSON token API can validate toggle keys against the same source.
mod tool_toggles;
pub use tool_toggles::{entries_for_roles, valid_keys};

mod tokens;
pub use tokens::{
    tokens_create, tokens_delete, tokens_index, tokens_mcp_policy, tokens_revoke, tokens_rotate,
    tokens_tools_master, tokens_tools_toggle,
};

// ---------------------------------------------------------------------------
// Tools
//
// Per-user tool on/off page (`/tools` + `/tools/toggle`). Available to
// every signed-in user; the list is scoped to the tools their roles
// grant. Re-export the two handler entry points for the router.
mod tools;
pub use tools::{tools_index, tools_toggle};

// ---------------------------------------------------------------------------
// Memory
//
// Per-user memory management page (`/memory` + create/edit/delete).
// Available to every signed-in user; the assistant-facing side is the
// `remember` / `recall` tools (see `server::tools::memory`).
mod memory;
pub use memory::{memory_create, memory_delete, memory_edit, memory_index};

// ---------------------------------------------------------------------------
// Scheduled actions
//
// Per-user prompts that run on a cron schedule (`/scheduled` + create /
// update / toggle / delete / preview, plus the edit sub-page). Available
// to every signed-in user; scoped to the owner in the data layer. The
// firing loop lives in `server::scheduled::worker`.
mod scheduled;
pub use scheduled::{
    scheduled_create, scheduled_delete, scheduled_edit_form, scheduled_index, scheduled_preview,
    scheduled_toggle, scheduled_update,
};

// ---------------------------------------------------------------------------
// Webhooks
//
// Per-user prompts fired by an inbound HTTP call (`/webhooks` + create /
// update / toggle / rotate / delete, plus the edit sub-page). The public
// trigger `webhook_trigger` (on `/hooks/{secret}`) has no session — the
// secret in the URL is the credential. Available to every signed-in user;
// scoped to the owner in the data layer.
mod webhooks;
pub use webhooks::{
    webhook_trigger, webhooks_create, webhooks_delete, webhooks_edit_form, webhooks_index,
    webhooks_rerun, webhooks_rerun_form, webhooks_rotate, webhooks_runs, webhooks_toggle,
    webhooks_update,
};

// Per-user MCP connector store (`/integrations`). OAuth connect/callback +
// per-tool permissions. The admin-managed catalog lives in `connectors`.
mod integrations;
pub use integrations::{
    integrations_callback, integrations_connect, integrations_connect_token,
    integrations_disconnect, integrations_index, integrations_retry, integrations_tool_mode,
    integrations_tools_all,
};

// Admin-managed MCP connector catalog (`/admin/connectors`).
mod connectors;
pub use connectors::{
    connectors_audit as admin_connectors_audit, connectors_delete as admin_connectors_delete,
    connectors_index as admin_connectors_index, connectors_restore as admin_connectors_restore,
    connectors_save as admin_connectors_save, connectors_toggle as admin_connectors_toggle,
};

// ---------------------------------------------------------------------------
// Admin (model defaults, future operator tooling). Gated on the
// `admin` role at the handler entry; non-admins never see the
// sidebar entry either.
mod admin;
pub use admin::{
    models_clear as admin_models_clear, models_defaults_save as admin_models_defaults_save,
    models_index as admin_models_index, models_save as admin_models_save,
    models_search_save as admin_models_search_save, upstreams_reload as admin_upstreams_reload,
};

// Merged upstream pools + backends page (`/admin/upstreams`). The GET page
// lives here; the old `/admin/pools` + `/admin/backends` GET routes 302 here.
// Same `admin`-role gate as the model-defaults page.
mod upstreams;
pub use upstreams::{
    backends_redirect as admin_backends_redirect, pools_redirect as admin_pools_redirect,
    upstreams_index as admin_upstreams_index,
};

// Backend CRUD write handlers (paths `/admin/backends/*`, unchanged); the page
// they back is now `/admin/upstreams`. Same `admin`-role gate.
mod backends;
pub use backends::{
    backends_delete as admin_backends_delete, backends_save as admin_backends_save,
};

// Pool CRUD write handlers (paths `/admin/pools/*`, unchanged); the page they
// back is now `/admin/upstreams`. Same `admin`-role gate.
mod pools;
pub use pools::{
    pools_delete as admin_pools_delete, pools_fallback_save as admin_pools_fallback_save,
    pools_save as admin_pools_save,
};

// Admin rate-limit / quota editor (`/admin/limits`). Same admin gate.
mod limits;
pub use limits::{
    limits_delete as admin_limits_delete, limits_index as admin_limits_index,
    limits_save as admin_limits_save,
};

// Admin skills viewer + manager (`/admin/skills`, upload, delete, grants).
// Same admin gate.
mod skills;
pub use skills::{
    skills_delete as admin_skills_delete, skills_download as admin_skills_download,
    skills_grants_save as admin_skills_grants_save, skills_index as admin_skills_index,
    skills_upload as admin_skills_upload,
};

// Per-user private skills page (`/skills`, upload, save, delete, download).
// Signed-in-user gate (not admin) — each user manages their own bundles.
mod skills_user;
pub use skills_user::{
    user_skills_delete, user_skills_download, user_skills_index, user_skills_save,
    user_skills_upload,
};

// `/admin/comfyui` — operator viewer for the headless ComfyUI workflow
// catalog (live snapshot + reload trigger). Same admin gate as the other
// operator pages.
mod comfyui;
pub use comfyui::{comfyui_index as admin_comfyui_index, comfyui_reload as admin_comfyui_reload};

// Admin RAG-collections CRUD (`/rag`). Same admin gate.
mod rag;
// The source-kind picker + provider field sets, rendered from each
// provider's own declared config fields (see `rag_source`).
mod rag_oauth;
mod rag_profiles;
mod rag_source;
pub use rag::{
    rag_add_ref, rag_add_sources_bulk, rag_cancel_edit, rag_create, rag_delete, rag_edit_form,
    rag_index, rag_ref_cancel_edit, rag_ref_delete, rag_ref_edit_form, rag_ref_log,
    rag_ref_reindex, rag_ref_set_primary, rag_ref_update, rag_reindex, rag_status, rag_sync_hook,
    rag_sync_token, rag_sync_token_clear, rag_test_source, rag_update,
};
pub use rag_oauth::{rag_connect, rag_oauth_callback};
pub use rag_profiles::{profile_create, profile_delete, profile_update, profiles_index};

// Admin gateway-groups editor (`/admin/groups`) — OIDC→group mappings + per-group
// tool/skill grants. Same admin gate.
mod groups;
pub use groups::{
    groups_delete as admin_groups_delete, groups_index as admin_groups_index,
    groups_save as admin_groups_save,
};

// Admin user roster + impersonation (`/admin/users`, `/admin/users/impersonate`)
// plus the un-gated `/impersonate/stop`. Roster + start are admin-only; stop is
// reachable by the impersonated (possibly non-admin) session so it can get back.
mod admin_users;
pub use admin_users::{impersonate_stop, users_impersonate, users_index as admin_users_index};

// Usage statistics: `/usage` for every signed-in user (scoped to their own
// requests), with an admin-only in-page "All users" toggle (`?scope=all`).
mod usage;
pub use usage::usage_index;

// Feedback widget: a floating button on every authed page that files a
// GitHub issue (with optional voice-to-fields + a viewport screenshot). The
// FAB + dialog are static chrome mounted in `layout_authed`; the three JSON
// endpoints are re-exported for the router.
mod feedback;
pub use feedback::{feedback_config, feedback_extract, feedback_submit};

// Both error pages below hardcode `Theme::Dark` (not derived from the
// request) since they're rare failure paths, not a preference-sensitive
// surface — same reasoning now extends to `Lang::En`: these two hardcode
// English rather than threading `lang` through the ~80 call sites that
// invoke them with an ad-hoc `message`, matching the existing precedent.
fn internal_error_html(user_email: &str, message: &str) -> Response {
    let body = html! {
        div(class: "alert alert-error max-w-md mx-auto items-start") {
            (icons::alert(20))
            div(class: "flex-1") {
                div(class: "font-bold") { (t(Lang::En, "error-internal-heading")) }
                div { (message) }
            }
        }
    }
    .to_html();
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(
            layout_authed(
                Theme::Dark,
                Lang::En,
                NavSections::default(),
                None,
                "Error — LLM Gateway",
                user_email,
                false,
                false,
                false,
                body,
                &SidebarChat::default(),
            )
            .into(),
        )
        .unwrap()
}

/// 403 page for the admin-only routes. Renders inside the standard
/// authed layout (the user *is* logged in, just not as admin), so
/// the sidebar still shows their other entries.
pub(super) fn forbidden_html(user_email: &str, message: &str) -> Response {
    let body = html! {
        div(class: "alert alert-warning max-w-md mx-auto items-start") {
            (icons::alert(20))
            div(class: "flex-1") {
                div(class: "font-bold") { (t(Lang::En, "error-forbidden-heading")) }
                div { (message) }
            }
        }
    }
    .to_html();
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(
            layout_authed(
                Theme::Dark,
                Lang::En,
                NavSections::default(),
                None,
                "Forbidden — LLM Gateway",
                user_email,
                false,
                false,
                false,
                body,
                &SidebarChat::default(),
            )
            .into(),
        )
        .unwrap()
}

// `read_body_to_bytes` lives in `session_core::chrome::read_body_to_bytes`.
