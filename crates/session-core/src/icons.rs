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
