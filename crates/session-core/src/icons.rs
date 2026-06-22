// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Inline SVG icons used by the rama pages.
//!
//! No HTTP round-trip per icon — the SVG markup is embedded directly
//! into the response via plait's `#(raw)` interpolation. Paths follow
//! the Lucide / Gravity-UI outlined style (24×24 viewBox, 1.75 stroke,
//! `currentColor` so they pick up the surrounding text color).
//!
//! Each function takes an explicit `size` (in pixels) so the SVG
//! element carries `width=… height=…` attributes — relying on a
//! `size-N` Tailwind class alone meant browsers without the CSS
//! cascade landed (page paints before stylesheet, or an attribute
//! conflict we never tracked down) fell back to the ~300×150
//! intrinsic default. With explicit attributes the icons render at
//! the requested size unconditionally.

use plait::{Html, ToHtml, html};

/// Wrap a raw `<svg>` string in a `plait::Html` so it splices into a
/// parent `html! { ... }` without escaping.
fn raw(svg: String) -> Html {
    html! { #(svg) }.to_html()
}

fn render(size: u32, body: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\" \
         width=\"{size}\" height=\"{size}\" \
         fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.75\" \
         stroke-linecap=\"round\" stroke-linejoin=\"round\" \
         class=\"inline-block shrink-0 align-text-bottom\" aria-hidden=\"true\">\
         {body}</svg>",
    )
}

/// Microphone — used by the chat composer's voice-record button.
pub fn mic(size: u32) -> Html {
    raw(render(
        size,
        r#"<rect x="9" y="2" width="6" height="11" rx="3"/><path d="M5 10v1a7 7 0 0 0 14 0v-1"/><path d="M12 18v3"/><path d="M8 22h8"/>"#,
    ))
}

/// Stop (filled square) — voice button when actively recording.
pub fn stop(size: u32) -> Html {
    raw(render(
        size,
        r#"<rect x="6" y="6" width="12" height="12" rx="2" fill="currentColor"/>"#,
    ))
}

pub fn sun(size: u32) -> Html {
    raw(render(
        size,
        r#"<circle cx="12" cy="12" r="4"/><path d="M12 2v2"/><path d="M12 20v2"/><path d="m4.93 4.93 1.41 1.41"/><path d="m17.66 17.66 1.41 1.41"/><path d="M2 12h2"/><path d="M20 12h2"/><path d="m6.34 17.66-1.41 1.41"/><path d="m19.07 4.93-1.41 1.41"/>"#,
    ))
}

/// Bar chart (lucide `bar-chart-3`) — the Usage / statistics nav entry.
pub fn chart(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M3 3v18h18"/><path d="M18 17V9"/><path d="M13 17V5"/><path d="M8 17v-3"/>"#,
    ))
}

/// Clock — the Scheduled-actions nav entry + schedule affordances.
pub fn clock(size: u32) -> Html {
    raw(render(
        size,
        r#"<circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/>"#,
    ))
}

/// Pause (two bars) — pause a running schedule.
pub fn pause(size: u32) -> Html {
    raw(render(
        size,
        r#"<rect x="6" y="5" width="4" height="14" rx="1"/><rect x="14" y="5" width="4" height="14" rx="1"/>"#,
    ))
}

/// Play (triangle) — resume a paused schedule.
pub fn play(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M7 4.5v15l13 -7.5z" fill="currentColor"/>"#,
    ))
}

/// Pencil — edit a scheduled action.
pub fn pencil(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M12 20h9"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/>"#,
    ))
}

/// Left chevron — "back" affordance on sub-pages.
pub fn chevron_left(size: u32) -> Html {
    raw(render(size, r#"<path d="m15 18-6-6 6-6"/>"#))
}

pub fn moon(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/>"#,
    ))
}

/// Home — dashboard nav link.
pub fn home(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="m3 9 9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/><polyline points="9 22 9 12 15 12 15 22"/>"#,
    ))
}

/// Key — tokens nav link.
pub fn key(size: u32) -> Html {
    raw(render(
        size,
        r#"<circle cx="7.5" cy="15.5" r="3.5"/><path d="m10 13 9-9"/><path d="m17 6 3 3"/><path d="m14 9 3 3"/>"#,
    ))
}

/// Speech bubble — chat nav link.
pub fn message(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8z"/>"#,
    ))
}

pub fn logout(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><polyline points="16 17 21 12 16 7"/><line x1="21" y1="12" x2="9" y2="12"/>"#,
    ))
}

/// Check-circle — success toasts.
pub fn check(size: u32) -> Html {
    raw(render(
        size,
        r#"<circle cx="12" cy="12" r="9"/><polyline points="9 12 11 14 15 10"/>"#,
    ))
}

/// Alert-triangle — error toasts.
pub fn alert(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="m10.29 3.86-8.16 14a2 2 0 0 0 1.71 3h16.32a2 2 0 0 0 1.71-3l-8.16-14a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/>"#,
    ))
}

/// Down chevron — used by the mobile nav dropdown's `<summary>` to
/// hint at expandability.
pub fn chevron_down(size: u32) -> Html {
    raw(render(size, r#"<polyline points="6 9 12 15 18 9"/>"#))
}

/// Simple stroke-dasharray spinner. Uses Tailwind's built-in
/// `animate-spin` so the arc rotates. `currentColor` makes it inherit
/// the surrounding text color.
pub fn spinner(size: u32) -> Html {
    raw(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\" \
         width=\"{size}\" height=\"{size}\" \
         class=\"inline-block shrink-0 animate-spin\" aria-hidden=\"true\">\
         <circle cx=\"12\" cy=\"12\" r=\"9\" \
         stroke=\"currentColor\" stroke-width=\"2.5\" fill=\"none\" \
         stroke-dasharray=\"35 65\" stroke-linecap=\"round\"/>\
         </svg>"
    ))
}

/// Info-circle — neutral toasts.
pub fn info(size: u32) -> Html {
    raw(render(
        size,
        r#"<circle cx="12" cy="12" r="9"/><line x1="12" y1="11" x2="12" y2="16"/><line x1="12" y1="8" x2="12.01" y2="8"/>"#,
    ))
}

/// Up arrow — chat composer send button (ChatGPT-style).
pub fn send(size: u32) -> Html {
    raw(render(
        size,
        r#"<line x1="12" y1="19" x2="12" y2="5"/><polyline points="5 12 12 5 19 12"/>"#,
    ))
}

/// Clipboard with a duplicated leaf — copy-to-clipboard buttons.
pub fn copy(size: u32) -> Html {
    raw(render(
        size,
        r#"<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>"#,
    ))
}

/// Horizontal sliders — model picker summary chip.
pub fn sliders(size: u32) -> Html {
    raw(render(
        size,
        r#"<line x1="4" y1="6" x2="11" y2="6"/><line x1="15" y1="6" x2="20" y2="6"/><line x1="4" y1="12" x2="6" y2="12"/><line x1="10" y1="12" x2="20" y2="12"/><line x1="4" y1="18" x2="13" y2="18"/><line x1="17" y1="18" x2="20" y2="18"/><circle cx="13" cy="6" r="2"/><circle cx="8" cy="12" r="2"/><circle cx="15" cy="18" r="2"/>"#,
    ))
}

/// Hamburger — mobile sidebar toggle.
pub fn menu(size: u32) -> Html {
    raw(render(
        size,
        r#"<line x1="4" y1="6" x2="20" y2="6"/><line x1="4" y1="12" x2="20" y2="12"/><line x1="4" y1="18" x2="20" y2="18"/>"#,
    ))
}

/// A filled brand logo (simple-icons path, 24×24 viewBox) in `color`. Unlike
/// [`render`], brand marks are solid fills, not strokes.
fn brand(size: u32, color: &str, path: &str) -> Html {
    raw(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\" \
         width=\"{size}\" height=\"{size}\" fill=\"{color}\" \
         class=\"inline-block shrink-0 align-text-bottom\" aria-hidden=\"true\">\
         <path d=\"{path}\"/></svg>",
    ))
}

// Brand logo paths (simple-icons, CC0). `currentColor` for GitHub so it adapts
// to light/dark; the rest carry their brand color.
const SI_GITHUB: &str = "M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12";
const SI_GITLAB: &str = "m23.6004 9.5927-.0337-.0862L20.3.9814a.851.851 0 0 0-.3362-.405.8748.8748 0 0 0-.9997.0539.8748.8748 0 0 0-.29.4399l-2.2055 6.748H7.5375l-2.2057-6.748a.8573.8573 0 0 0-.29-.4412.8748.8748 0 0 0-.9997-.0537.8585.8585 0 0 0-.3362.4049L.4332 9.5015l-.0325.0862a6.0657 6.0657 0 0 0 2.0119 7.0105l.0113.0087.03.0213 4.976 3.7264 2.462 1.8633 1.4995 1.1321a1.0085 1.0085 0 0 0 1.2197 0l1.4995-1.1321 2.4619-1.8633 5.006-3.7489.0125-.01a6.0682 6.0682 0 0 0 2.0094-7.003z";
const SI_ATLASSIAN: &str = "M7.12 11.084a.683.683 0 00-1.16.126L.075 22.974a.703.703 0 00.63 1.018h8.19a.678.678 0 00.63-.39c1.767-3.65.696-9.203-2.406-12.52zM11.434.386a15.515 15.515 0 00-.906 15.317l3.95 7.9a.703.703 0 00.628.388h8.19a.703.703 0 00.63-1.017L12.63.38a.664.664 0 00-1.196.006z";
const SI_GMAIL: &str = "M24 5.457v13.909c0 .904-.732 1.636-1.636 1.636h-3.819V11.73L12 16.64l-6.545-4.91v9.273H1.636A1.636 1.636 0 0 1 0 19.366V5.457c0-2.023 2.309-3.178 3.927-1.964L5.455 4.64 12 9.548l6.545-4.91 1.528-1.145C21.69 2.28 24 3.434 24 5.457z";
const SI_GOOGLE_DRIVE: &str = "M12.01 1.485c-2.082 0-3.754.02-3.743.047.01.02 1.708 3.001 3.774 6.62l3.76 6.574h3.76c2.081 0 3.753-.02 3.742-.047-.005-.02-1.708-3.001-3.775-6.62l-3.76-6.574zm-4.76 1.73a789.828 789.861 0 0 0-3.63 6.319L0 15.868l1.89 3.298 1.885 3.297 3.62-6.335 3.618-6.33-1.88-3.287C8.1 4.704 7.255 3.22 7.25 3.214zm2.259 12.653-.203.348c-.114.198-.96 1.672-1.88 3.287a423.93 423.948 0 0 1-1.698 2.97c-.01.026 3.24.042 7.222.042h7.244l1.796-3.157c.992-1.734 1.85-3.23 1.906-3.323l.104-.167h-7.249z";
const SI_GOOGLE_CALENDAR: &str = "M18.316 5.684H24v12.632h-5.684V5.684zM5.684 24h12.632v-5.684H5.684V24zM18.316 5.684V0H1.895A1.894 1.894 0 0 0 0 1.895v16.421h5.684V5.684h12.632zm-7.207 6.25v-.065c.272-.144.5-.349.687-.617s.279-.595.279-.982c0-.379-.099-.72-.3-1.025a2.05 2.05 0 0 0-.832-.714 2.703 2.703 0 0 0-1.197-.257c-.6 0-1.094.156-1.481.467-.386.311-.65.671-.793 1.078l1.085.452c.086-.249.224-.461.413-.633.189-.172.445-.257.767-.257.33 0 .602.088.816.264a.86.86 0 0 1 .322.703c0 .33-.12.589-.36.778-.24.19-.535.284-.886.284h-.567v1.085h.633c.407 0 .748.109 1.02.327.272.218.407.499.407.843 0 .336-.129.614-.387.832s-.565.327-.924.327c-.351 0-.651-.103-.897-.311-.248-.208-.422-.502-.521-.881l-1.096.452c.178.616.505 1.082.977 1.401.472.319.984.478 1.538.477a2.84 2.84 0 0 0 1.293-.291c.382-.193.684-.458.902-.794.218-.336.327-.72.327-1.149 0-.429-.115-.797-.344-1.105a2.067 2.067 0 0 0-.881-.689zm2.093-1.931l.602.913L15 10.045v5.744h1.187V8.446h-.827l-2.158 1.557z";

const SI_GOOGLE: &str = "M12.48 10.92v3.28h7.84c-.24 1.84-.853 3.187-1.787 4.133-1.147 1.147-2.933 2.4-6.053 2.4-4.827 0-8.6-3.893-8.6-8.72s3.773-8.72 8.6-8.72c2.6 0 4.507 1.027 5.907 2.347l2.307-2.307C18.747 1.44 16.133 0 12.48 0 5.867 0 .307 5.387.307 12s5.56 12 12.173 12c3.573 0 6.267-1.173 8.373-3.36 2.16-2.16 2.84-5.213 2.84-7.667 0-.76-.053-1.467-.173-2.053H12.48z";

/// Inline brand logo for a known connector key, sized `size`. `None` for keys
/// without a bundled logo (caller falls back to the connector's `icon` text).
pub fn connector_logo(key: &str, size: u32) -> Option<Html> {
    let (color, path) = match key {
        "github" => ("currentColor", SI_GITHUB),
        "gitlab" => ("#FC6D26", SI_GITLAB),
        "atlassian" => ("#0052CC", SI_ATLASSIAN),
        // The single self-hosted Google Workspace connector (Gmail, Calendar,
        // Drive, Docs, …) carries the Google "G".
        "google_workspace" => ("#4285F4", SI_GOOGLE),
        "gmail" => ("#EA4335", SI_GMAIL),
        "google_drive" => ("#4285F4", SI_GOOGLE_DRIVE),
        "google_calendar" => ("#4285F4", SI_GOOGLE_CALENDAR),
        _ => return None,
    };
    Some(brand(size, color, path))
}

/// Plug — MCP connectors / integrations (sidebar + store).
pub fn plug(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M12 22v-5"/><path d="M9 8V2"/><path d="M15 8V2"/><path d="M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z"/>"#,
    ))
}

/// Plus — "New chat" button in the sidebar.
pub fn plus(size: u32) -> Html {
    raw(render(
        size,
        r#"<line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/>"#,
    ))
}

/// Trash — per-row delete in the chat sidebar.
pub fn trash(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M3 6h18"/><path d="m19 6-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>"#,
    ))
}

/// Folder.
pub fn folder(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M20 20H4a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2z"/>"#,
    ))
}

/// Padlock.
pub fn lock(size: u32) -> Html {
    raw(render(
        size,
        r#"<rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>"#,
    ))
}

/// Cube.
pub fn cube(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"/><path d="M3.27 6.96 12 12.01l8.73-5.05"/><path d="M12 22.08V12"/>"#,
    ))
}

/// CPU / chip — the admin model-defaults nav link. Distinct from the
/// `sliders` icon so Models, Tools, and Skills don't collide visually.
pub fn cpu(size: u32) -> Html {
    raw(render(
        size,
        r#"<rect x="4" y="4" width="16" height="16" rx="2"/><rect x="9" y="9" width="6" height="6"/><path d="M9 1v3"/><path d="M15 1v3"/><path d="M9 20v3"/><path d="M15 20v3"/><path d="M20 9h3"/><path d="M20 14h3"/><path d="M1 9h3"/><path d="M1 14h3"/>"#,
    ))
}

/// Sparkles — the admin skills nav link.
pub fn sparkles(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M12 3l1.9 5.8a2 2 0 0 0 1.3 1.3L21 12l-5.8 1.9a2 2 0 0 0-1.3 1.3L12 21l-1.9-5.8a2 2 0 0 0-1.3-1.3L3 12l5.8-1.9a2 2 0 0 0 1.3-1.3z"/><path d="M5 3v4"/><path d="M19 17v4"/><path d="M3 5h4"/><path d="M17 19h4"/>"#,
    ))
}

/// Database (stacked cylinder) — the admin RAG-collections nav link.
/// Distinct from `folder`, which Memory keeps.
pub fn database(size: u32) -> Html {
    raw(render(
        size,
        r#"<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14a9 3 0 0 0 18 0V5"/><path d="M3 12a9 3 0 0 0 18 0"/>"#,
    ))
}

/// Users (two people) — the admin user-roster nav link.
pub fn users(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>"#,
    ))
}

/// Play square.
pub fn play_square(size: u32) -> Html {
    raw(render(
        size,
        r#"<rect x="3" y="3" width="18" height="18" rx="2"/><polygon points="10 8 16 12 10 16" fill="currentColor" stroke="none"/>"#,
    ))
}

/// Paperclip — chat composer's "attach files" button.
pub fn paperclip(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="m21 12-9.5 9.5a4.95 4.95 0 1 1-7-7L13 6a3.5 3.5 0 1 1 5 5L9.5 19.5a2 2 0 0 1-2.83-2.83L15 8.5"/>"#,
    ))
}

/// Download — tray with a down-arrow. Chat-export menu trigger.
pub fn download(size: u32) -> Html {
    raw(render(
        size,
        r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/>"#,
    ))
}

/// X-mark — composer chip remove button + generic dismiss.
pub fn x_mark(size: u32) -> Html {
    raw(render(
        size,
        r#"<line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/>"#,
    ))
}
