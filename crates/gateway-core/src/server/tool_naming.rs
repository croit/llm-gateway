// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Well-known tool ids, id prefixes, and the slug→title humaniser.
//!
//! This is the *naming* vocabulary of the tool surface, split out from
//! the tool `catalog` because three different layers
//! need it and only one of them is above the catalog:
//!
//!   * RBAC (this crate) recognises a `comfyui_*` grant,
//!   * the typst discovery pass (`gateway-features`) humanises manifest ids,
//!   * the catalog and `enable_tools` (above) do the actual grouping.
//!
//! Keeping the strings here lets the resolver stay below the tool registry.
//! `catalog` re-exports everything, so call sites beside the catalog are
//! unchanged.

/// Prefix shared by every per-template typst tool (`typst_<id>` and its
/// `_edit`/`_read`/`_pptx` variants). Each *template* is its own toggle now —
/// `entry_key_for` maps a template's render tool + variants to one key
/// (`typst_<id>`) so a single switch governs that whole template's family,
/// while different templates stay independently selectable.
pub const TYPST_PREFIX: &str = "typst_";

/// Tool-id prefix for ComfyUI workflows (`comfyui_<id>`). Each loaded
/// workflow is its own id, but they all collapse to one [`COMFYUI_KEY`]
/// toggle — one switch governs the whole ComfyUI family.
pub const COMFYUI_PREFIX: &str = "comfyui_";

/// The single toggle key that governs every `comfyui_*` tool. Same
/// pattern as MCP (`mcp__<server>`) and Memory (`remember`+`recall`):
/// the user reasons about ComfyUI as one capability, so one switch turns
/// the whole family on/off. A newly-reloaded workflow is automatically
/// enabled when this toggle is on — the catalog never has to chase a
/// per-workflow preference.
pub const COMFYUI_KEY: &str = "comfyui";

/// Tool id of the skill loader. Sits beside [`BOOTSTRAP_TOOL_ID`] because
/// `AppState::allowed_tools_for_session` force-injects it — the system message
/// advertises the caller's skills every turn, so the loader must always be
/// callable — and that logic lives below the tool implementations.
pub const READ_SKILL_ID: &str = "read_skill";

/// Tool id of the lone always-on bootstrap. It can't itself be enabled
/// via the per-conversation overlay (chicken-and-egg), so it's the one
/// exception baked into `AppState::allowed_tools_for_session`. Every other tool —
/// including memory, time, location, web — is lazy and turned on by the
/// model calling this one with the relevant key.
pub const BOOTSTRAP_TOOL_ID: &str = "enable_tools";

/// Turn a slug into a human label: `quarterly_report` → "Quarterly report".
/// Used as the fallback template name when a manifest declares no `title`.
pub fn prettify(slug: &str) -> String {
    let spaced = slug.replace(['_', '-'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
