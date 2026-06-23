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
use session_core::icons;

use crate::rama_server::session::Session;
use crate::rama_server::state::RamaState;
use crate::server::db::users;

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
    /// Per-user MCP connector store (`/integrations`). Shown to every
    /// signed-in user; each connects their own accounts (Google, GitHub, …).
    Integrations,
    /// Usage statistics (`/usage`). Shown to every signed-in user (scoped
    /// to their own requests); admins get an in-page "All users" toggle.
    Usage,
    /// Admin-only pages (model defaults, future operator tooling).
    /// The sidebar entry is only rendered for users whose `roles`
    /// includes `"admin"`; non-admins never see it.
    Admin,
    /// Admin-only upstream backends status page (`/admin/backends`).
    /// Same `admin`-role gate as [`NavItem::Admin`]; its own variant so
    /// the sidebar highlight lands on it rather than on Models.
    Backends,
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

/// One conversation in the sidebar list. Sourced from the persisted
/// `chat_sessions` rows — the chat handlers prefetch this, every
/// other authed handler does too so the sidebar is consistent across
/// the app.
pub(super) struct SidebarSession {
    pub id: String,
    pub title: Option<String>,
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

fn render_app_sidebar(
    active: Option<NavItem>,
    user_email: &str,
    is_admin: bool,
    theme: Theme,
    chat: &SidebarChat,
) -> Html {
    let user_email = user_email.to_string();
    let sessions: Vec<SidebarSession> = chat
        .sessions
        .iter()
        .map(|s| SidebarSession {
            id: s.id.clone(),
            title: s.title.clone(),
        })
        .collect();
    let active_sess = chat.active_session_id.clone();
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
                (sidebar_nav_link("/chat", NavItem::Chat, active, icons::message(16), "Chat"))
                (nav_group("workspace", "Workspace", html! {
                    (sidebar_nav_link("/memory", NavItem::Memory, active, icons::folder(16), "Memory"))
                    (sidebar_nav_link("/scheduled", NavItem::Scheduled, active, icons::clock(16), "Scheduled"))
                    (sidebar_nav_link("/integrations", NavItem::Integrations, active, icons::plug(16), "Integrations"))
                    (sidebar_nav_link("/tools", NavItem::Tools, active, icons::sliders(16), "Tools"))
                }.to_html()))
                (nav_group("account", "Account", html! {
                    (sidebar_nav_link("/tokens", NavItem::Tokens, active, icons::key(16), "Tokens"))
                    (sidebar_nav_link("/usage", NavItem::Usage, active, icons::chart(16), "Usage"))
                }.to_html()))
                if is_admin {
                    (nav_group("admin", "Admin", html! {
                        (sidebar_nav_link("/admin/users", NavItem::Users, active, icons::users(16), "Users"))
                        (sidebar_nav_link("/admin/models", NavItem::Admin, active, icons::cpu(16), "Models"))
                        (sidebar_nav_link("/admin/backends", NavItem::Backends, active, icons::cube(16), "Backends"))
                        (sidebar_nav_link("/rag", NavItem::Rag, active, icons::database(16), "RAG"))
                        (sidebar_nav_link("/admin/skills", NavItem::Skills, active, icons::sparkles(16), "Skills"))
                        (sidebar_nav_link("/admin/connectors", NavItem::Connectors, active, icons::plug(16), "Connectors"))
                    }.to_html()))
                }
            }
            div(class: "app-sidebar__sessions-section") {
                div(class: "app-sidebar__sessions-header") {
                    span(class: "app-sidebar__sessions-label") { "Conversations" }
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
                            "aria-label": "Start a new conversation",
                            title: "New conversation"
                        ) {
                            (icons::plus(14))
                        }
                    }
                }
                ul(id: "session-list", class: "app-sidebar__sessions") {
                    for s in sessions.iter() {
                        (render_sidebar_session(s, active_sess.as_deref()))
                    }
                }
            }
            div(class: "app-sidebar__user") {
                span(class: "app-sidebar__email") { (user_email) }
                div(class: "app-sidebar__user-actions") {
                    (chrome::render_theme_toggle_form(theme))
                    form(
                        method: "post",
                        action: "/auth/logout",
                        class: "m-0"
                    ) {
                        button(
                            type: "submit",
                            class: "btn btn-ghost btn-square btn-sm",
                            title: "Sign out",
                            "aria-label": "Sign out"
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
                    title: "Source code (AGPL-3.0)"
                ) {
                    "Source · AGPL-3.0 · " (crate::build_info::version_label())
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
fn nav_group(key: &str, label: &str, items: Html) -> Html {
    let key = key.to_string();
    let label = label.to_string();
    let directive = format!("@post('/nav/toggle/{key}')");
    let aria = format!("Toggle {label} section");
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

/// One conversation row in the sidebar. Hover reveals the delete
/// button; active row gets a soft tinted background.
fn render_sidebar_session(s: &SidebarSession, active_id: Option<&str>) -> Html {
    let id = s.id.clone();
    let row_id = format!("session-row-{id}");
    let href = format!("/chat/{id}");
    let delete_url = format!("/chat/{id}/delete");
    let directive = sidebar_nav_directive(&href);
    let delete_directive = format!("@post('{delete_url}', {{contentType: 'form'}})");
    let title = s
        .title
        .clone()
        .unwrap_or_else(|| "Untitled chat".to_string());
    let is_active = active_id == Some(&s.id);
    let row_class = if is_active {
        "session-row session-row--active"
    } else {
        "session-row"
    };
    html! {
        li(id: (row_id), class: "session-row__item") {
            // The whole row is the clickable target so a sloppy
            // mobile tap on the padding doesn't fall through. The
            // delete form sits as a sibling, absolutely positioned
            // over the right edge — clicks on the trash button
            // don't bubble through the link.
            a(
                href: (href),
                class: (row_class),
                "data-on:click__prevent": (directive)
            ) {
                span(class: "session-row__title") { (title) }
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
                    "aria-label": "Delete conversation",
                    title: "Delete conversation"
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
fn nav_or_html_page(
    datastar: bool,
    theme: Theme,
    nav: NavSections,
    active: NavItem,
    title: &str,
    user_email: &str,
    is_admin: bool,
    impersonating: bool,
    body: Html,
    url: &str,
    chat: &SidebarChat,
) -> Response {
    if !datastar {
        return html_authed_page(
            theme,
            nav,
            Some(active),
            title,
            user_email,
            is_admin,
            impersonating,
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
    let sidebar_html =
        render_app_sidebar(Some(active), user_email, is_admin, theme, chat).to_string();
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
        // `max-w-5xl mx-auto` matches the other authed pages
        // (dashboard, tokens) so the chat lines up with the same
        // reading-column on desktop. Empty page-bg gutters on
        // either side at wide viewports.
        //
        // No bottom padding at any size: the composer floats
        // absolutely over the conversation (see main.css), so any
        // page-bg padding under it reads as a sticky "bar". On
        // phone we also drop the top padding because the floating
        // drawer-button takes the same role. Clearance for both
        // floating elements is moved into `#conversation`'s own
        // padding so messages don't sit permanently behind them.
        Some(NavItem::Chat) => {
            "chat-main relative flex-1 min-h-0 flex flex-col w-full max-w-5xl \
             mx-auto px-4 sm:px-6 sm:pt-4"
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
    nav: NavSections,
    active: Option<NavItem>,
    title: &str,
    user_email: &str,
    is_admin: bool,
    impersonating: bool,
    body: Html,
    chat: &SidebarChat,
) -> Response {
    let html = layout_authed(
        theme,
        nav,
        active,
        title,
        user_email,
        is_admin,
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
    nav: NavSections,
    active: Option<NavItem>,
    title: &str,
    user_email: &str,
    is_admin: bool,
    impersonating: bool,
    body: Html,
    chat: &SidebarChat,
) -> String {
    let theme_str = theme.as_str();
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
            lang: "en",
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
                            "aria-label": "Open menu"
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
                            (render_impersonation_banner(user_email))
                        }
                        main(class: (main_class)) {
                            (body)
                        }
                    }
                    div(class: "drawer-side z-40") {
                        label(
                            "for": "app-sidebar-toggle",
                            "aria-label": "Close menu",
                            class: "drawer-overlay"
                        ) {}
                        (render_app_sidebar(active, user_email, is_admin, theme, chat))
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
fn render_impersonation_banner(email: &str) -> Html {
    let email = email.to_string();
    html! {
        div(
            id: "impersonation-banner",
            class: "shrink-0 flex items-center gap-3 px-4 py-2 \
                    bg-warning text-warning-content border-b border-warning/40",
            role: "alert"
        ) {
            (icons::alert(18))
            span(class: "text-sm font-medium min-w-0 break-words") {
                "You are impersonating " strong { (email) } "."
            }
            form(
                method: "post",
                action: "/impersonate/stop",
                class: "m-0 ml-auto shrink-0"
            ) {
                button(type: "submit", class: "btn btn-sm") {
                    "Return to your account"
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
async fn require_session_or_redirect(
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
        && path_and_query.starts_with('/')
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
        .filter(|rt| rt.starts_with('/'));
    let body = html! {
        main(class: "min-h-dvh flex items-center justify-center p-8") {
            div(class: "card border border-base-300 w-full max-w-md") {
                div(class: "card-body") {
                    h2(class: "card-title text-2xl") { "Sign in to LLM Gateway" }
                    p(class: "text-base-content/70") {
                        "Authenticate with your company's OIDC provider to mint "
                        "API tokens and route LLM requests."
                    }
                    form(action: "/auth/login", method: "get", class: "mt-2") {
                        if let Some(rt) = return_to.as_ref() {
                            input(type: "hidden", name: "return_to", value: (rt.clone()));
                        }
                        button(
                            type: "submit",
                            class: "btn btn-primary btn-block"
                        ) {
                            "Continue with OIDC →"
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
                            "Source code · AGPL-3.0"
                        }
                    }
                }
            }
        }
    }
    .to_html();
    chrome::html_page(theme, "Sign in — LLM Gateway", body)
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
// Composer, /chat/stream SSE endpoint, tool-call loop, and the bubble
// renderers all live in `chat.rs`. We pub-re-export the four handler
// entry points so the router (which calls `pages::chat_index` etc.)
// doesn't have to know about the split.
mod chat;
pub use chat::{
    chat_attachment, chat_cancel, chat_capabilities_toggle, chat_edit, chat_effort_set,
    chat_export_markdown, chat_export_pdf, chat_fork, chat_index, chat_message_send, chat_retry,
    chat_session_create, chat_session_delete, chat_session_view, chat_share_toggle, chat_tail,
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
    connectors_delete as admin_connectors_delete, connectors_index as admin_connectors_index,
    connectors_restore as admin_connectors_restore, connectors_save as admin_connectors_save,
    connectors_toggle as admin_connectors_toggle,
};

// ---------------------------------------------------------------------------
// Admin (model defaults, future operator tooling). Gated on the
// `admin` role at the handler entry; non-admins never see the
// sidebar entry either.
mod admin;
pub use admin::{
    models_index as admin_models_index, models_reasoning_save as admin_models_reasoning_save,
    models_save as admin_models_save,
};

// Admin upstream-backends status page (`/admin/backends`). Read-only;
// same `admin`-role gate as the model-defaults page.
mod backends;
pub use backends::backends_index as admin_backends_index;

// Admin skills viewer + manager (`/admin/skills`, upload, delete, grants).
// Same admin gate.
mod skills;
pub use skills::{
    skills_delete as admin_skills_delete, skills_download as admin_skills_download,
    skills_grants_save as admin_skills_grants_save, skills_index as admin_skills_index,
    skills_upload as admin_skills_upload,
};

// Admin RAG-collections CRUD (`/rag`). Same admin gate.
mod rag;
pub use rag::{
    rag_add_ref, rag_add_sources_bulk, rag_cancel_edit, rag_create, rag_delete, rag_edit_form,
    rag_index, rag_ref_delete, rag_ref_reindex, rag_ref_set_primary, rag_reindex, rag_update,
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

fn internal_error_html(user_email: &str, message: &str) -> Response {
    let body = html! {
        div(class: "alert alert-error max-w-md mx-auto items-start") {
            (icons::alert(20))
            div(class: "flex-1") {
                div(class: "font-bold") { "Internal error" }
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
                NavSections::default(),
                None,
                "Error — LLM Gateway",
                user_email,
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
                div(class: "font-bold") { "Forbidden" }
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
                NavSections::default(),
                None,
                "Forbidden — LLM Gateway",
                user_email,
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
