// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Reusable tool on/off toggle list — the grouped, daisyUI-styled
//! capability switches shared by the per-user `/tools` page and the
//! per-token panel on `/tokens`.
//!
//! Both pages render the *same* catalog of capabilities (grouped by
//! [`Category`], one row per toggle key) and persist a negative
//! allowlist (default-on; a row records an explicit disable). They differ
//! only in where a toggle POSTs and how its row's DOM id is namespaced —
//! captured by [`ToggleCtx`] — so the markup, the checkbox-presence
//! convergence trick, and the category grouping all live here once.

use std::collections::HashSet;

use plait::{Html, ToHtml, html};

use crate::rama_server::state::RamaState;
use crate::server::tools::catalog::{self, Category, ToolEntry};

/// What distinguishes one host page's toggle list from another's: the
/// endpoint each row POSTs to and the prefix its `<div>` id carries (so
/// multiple lists — e.g. one per token — never collide on the same id).
#[derive(Clone)]
pub struct ToggleCtx {
    /// Endpoint a row's form submits to (form `action` + the datastar
    /// `@post` directive). The handler reads `tool_key` + `enabled`.
    pub post_path: String,
    /// Prefix for each row's DOM id; the full id is `{row_id_prefix}-{key}`.
    /// The host page's SSE patch must target the same id to swap a row in
    /// place. `/tools` uses `"tool-row"` (preserving its existing ids).
    pub row_id_prefix: String,
}

impl ToggleCtx {
    /// DOM id for the row governing `key`.
    pub fn row_id(&self, key: &str) -> String {
        format!("{}-{}", self.row_id_prefix, key)
    }
}

/// The capability toggle entries a user's roles grant, grouped + de-noised
/// for display. The single home both `/tools` and the `/tokens` per-token
/// panel resolve the row list through, so they never drift.
pub fn entries_for_roles(state: &RamaState, roles: &[String]) -> Vec<ToolEntry> {
    let role_ids = state.rbac.role_ids_for(roles);
    let allowed = state.rbac.allowed_tools(&role_ids, &state.tools);
    catalog::entries(&state.tools, &allowed, &state.typst_templates)
}

/// The set of valid toggle keys for these entries — used to reject bogus
/// keys before persisting a choice.
pub fn valid_keys(entries: &[ToolEntry]) -> HashSet<String> {
    entries.iter().map(|e| e.key.clone()).collect()
}

/// Split the (already category-sorted) entries into contiguous groups,
/// preserving order. Returns `(category, entries)` pairs.
pub fn group_by_category(entries: &[ToolEntry]) -> Vec<(Category, Vec<ToolEntry>)> {
    let mut groups: Vec<(Category, Vec<ToolEntry>)> = Vec::new();
    for entry in entries {
        match groups.last_mut() {
            Some((cat, rows)) if *cat == entry.category => rows.push(entry.clone()),
            _ => groups.push((entry.category, vec![entry.clone()])),
        }
    }
    groups
}

/// The full grouped toggle list: one `<section>` card per category, each
/// holding its rows. `disabled` carries the toggle keys currently off (a
/// key absent from it is on). Shared by both host pages.
pub fn render_toggle_sections(
    entries: &[ToolEntry],
    disabled: &HashSet<String>,
    ctx: &ToggleCtx,
) -> Html {
    let groups = group_by_category(entries);
    html! {
        for (category, rows) in groups.iter() {
            section(class: "card border border-base-300 mb-6") {
                div(class: "card-body") {
                    h2(class: "card-title text-base") { (category.label()) }
                    div(class: "flex flex-col divide-y divide-base-300") {
                        for entry in rows.iter() {
                            (render_toggle_row(entry, !disabled.contains(&entry.key), ctx))
                        }
                    }
                }
            }
        }
    }
    .to_html()
}

/// One tool row: a human title with the underlying function name as a
/// subtle mono badge, the plain-language description below, and a daisyUI
/// toggle on the right. The toggle is a checkbox inside a form that
/// `@post`s on change; the host's SSE response swaps this same row back in
/// with the persisted state. The desired state rides in the form
/// (checkbox presence), so double-clicks converge rather than race a
/// read-modify-write.
pub fn render_toggle_row(entry: &ToolEntry, enabled: bool, ctx: &ToggleCtx) -> Html {
    let row_id = ctx.row_id(&entry.key);
    let title = entry.title.clone();
    let tech = entry.tech.clone();
    let description = entry.description.clone();
    let key = entry.key.clone();
    let action = ctx.post_path.clone();
    let directive = format!("@post('{}', {{contentType: 'form'}})", ctx.post_path);
    html! {
        div(id: (row_id), class: "flex items-center gap-4 py-3") {
            div(class: "flex-1 min-w-0") {
                div(class: "flex items-baseline gap-2 flex-wrap") {
                    span(class: "text-sm font-medium text-base-content") { (title) }
                    code(class: "text-xs text-base-content/50 font-mono") { (tech) }
                }
                div(class: "text-xs text-base-content/60 mt-0.5") { (description) }
            }
            form(
                action: (action),
                method: "post",
                class: "m-0",
                "data-on:change__prevent": (directive)
            ) {
                input(type: "hidden", name: "tool_key", value: (key));
                if enabled {
                    input(
                        type: "checkbox",
                        name: "enabled",
                        value: "true",
                        class: "toggle toggle-primary",
                        checked: "checked",
                        "aria-label": "Toggle tool"
                    );
                } else {
                    input(
                        type: "checkbox",
                        name: "enabled",
                        value: "true",
                        class: "toggle toggle-primary",
                        "aria-label": "Toggle tool"
                    );
                }
            }
        }
    }
    .to_html()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, category: Category) -> ToolEntry {
        ToolEntry {
            key: key.to_string(),
            title: key.to_string(),
            tech: key.to_string(),
            description: "desc".to_string(),
            category,
        }
    }

    #[test]
    fn row_id_is_prefixed() {
        let ctx = ToggleCtx {
            post_path: "/tools/toggle".into(),
            row_id_prefix: "tool-row".into(),
        };
        assert_eq!(ctx.row_id("search_web"), "tool-row-search_web");
    }

    #[test]
    fn group_by_category_preserves_contiguous_runs() {
        let entries = vec![
            entry("search_web", Category::Web),
            entry("fetch_url", Category::Web),
            entry("get_current_timestamp", Category::Utility),
        ];
        let groups = group_by_category(&entries);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, Category::Web);
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, Category::Utility);
    }

    #[test]
    fn enabled_row_renders_checked_and_posts_to_ctx_path() {
        let ctx = ToggleCtx {
            post_path: "/tokens/abc/tools/toggle".into(),
            row_id_prefix: "token-abc-toolrow".into(),
        };
        let e = entry("rag_search", Category::Utility);
        let on = render_toggle_row(&e, true, &ctx).to_string();
        assert!(on.contains("token-abc-toolrow-rag_search"));
        assert!(on.contains("/tokens/abc/tools/toggle"));
        assert!(on.contains("checked"));
        let off = render_toggle_row(&e, false, &ctx).to_string();
        assert!(!off.contains("checked"));
    }
}
