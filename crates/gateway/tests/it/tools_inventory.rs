// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Drift guard for [`docs/tools-inventory.md`] and the two `catalog` lookup
//! tables that decide how a tool renders on `/tools`.
//!
//! There is no runtime way to enumerate "every tool this binary could
//! register": the set depends on config (GeoIP, sandbox, typst, skills, image
//! pools), and `main` builds the registry inline. So — same approach as
//! `readme_routes.rs` — this test reads the **source** as one source of truth
//! and the **doc** as the other, and fails when they disagree in either
//! direction:
//!
//!   - a tool id exists in the source but no row documents it (a tool shipped
//!     undocumented), or
//!   - the doc names an id that no tool implements (a stale row survived a
//!     rename or removal).
//!
//! It then asserts, for every discovered id, that `catalog` will actually
//! render it properly: a real category (not the silent `Utility` fallback) and
//! hand-written display copy rather than the model-facing schema text. That is
//! the bug class this test exists for — `convert_document` and
//! `edit_presentation` sat in the wrong group with LLM prose in the settings
//! list, and nothing failed.
//!
//! Discovery is deliberately crate-agnostic (it walks `crates/`, not a fixed
//! path) so the ongoing crate split can move `tools/` without breaking it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gateway_runtime::server::tools::catalog::{
    self, COMFYUI_PREFIX, Category, TYPST_PREFIX, has_display_copy, requires_chat_session,
};

/// Tool ids that intentionally have no row in the inventory. Empty on
/// purpose: every tool the model can call should be documented, including the
/// hidden smoke test (it *is* documented, marked hidden). Kept as an explicit
/// escape hatch so an unusual future case is a conscious choice rather than a
/// reason to delete this test.
const UNDOCUMENTED: &[&str] = &[];

/// Ids that appear in tool-source string literals but are not real registered
/// tools. `mcp__demo__echo` is a fixture inside the MCP manager's own tests.
const NOT_REAL_TOOLS: &[&str] = &["mcp__demo__echo"];

/// Tools whose `category_for` is legitimately `Utility` — the catch-all is a
/// real category for genuinely miscellaneous tools, so this test pins *which*
/// ones rather than banning it. A new id landing here unannounced is the bug.
const EXPECTED_UTILITY: &[&str] = &[
    // Asking the user a question and notifying them are interaction
    // primitives, not capability areas of their own.
    "ask_user",
    "notify_user",
    "company_echo",
    "convert_currency",
    "enable_tools",
    "get_current_timestamp",
    "get_user_location",
];

/// `Tool::id` impls that return a runtime value instead of a literal, i.e. the
/// dynamic families. Keyed by the file that implements them so a *new* dynamic
/// family also has to be acknowledged here (and documented as a family in the
/// inventory's "Dynamic families" table).
/// Paths are crate-qualified suffixes so each entry also records which layer the
/// family lives on — the impls sit above the tool machinery, the machinery in the
/// runtime.
const DYNAMIC_ID_IMPLS: &[&str] = &[
    // typst_<template> + _edit/_read/_pptx
    "gateway-tools/src/typst_render.rs",
    // mcp__<server>__<tool>
    "gateway-runtime/src/server/tools/mcp/mod.rs",
    // AuditedTool — delegates to the wrapped tool
    "gateway-runtime/src/server/tools/mcp/manager.rs",
    // comfyui_<workflow>
    "gateway-runtime/src/server/comfyui_tool.rs",
];

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/crates/gateway.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/gateway has a grandparent")
        .to_path_buf()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `target/` can be huge and holds no first-party source.
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// One `fn id(&self) -> &str` implementation.
enum IdImpl {
    /// Returned a string literal, or a `const NAME: &str = "…"` resolved from
    /// the same file or, failing that, anywhere else in the workspace — the
    /// well-known ids (`read_skill`, `enable_tools`) live in
    /// `gateway_core::server::tool_naming` so that RBAC, the typst discovery
    /// pass and the catalog can all reach them from different layers.
    Static(String),
    /// Returned something computed at runtime (`self.id`, `&self.registry_id`,
    /// `self.inner.id()`), i.e. a dynamic family.
    Dynamic,
}

/// Parse every `Tool::id` impl in `src`. Handles the three shapes the codebase
/// actually uses: a literal, a const (declared here or in another crate), and a
/// runtime value. `corpus` is every source file's text, for the cross-crate case.
fn id_impls(src: &str, corpus: &[String]) -> Vec<IdImpl> {
    let mut out = Vec::new();
    for after in src
        .match_indices("fn id(&self) -> &str")
        .map(|(i, m)| &src[i + m.len()..])
    {
        // The trait *declaration* in `tools::mod` is `fn id(&self) -> &str;`
        // with no body. Requiring `{` immediately (modulo whitespace) skips it
        // — otherwise the scan runs on into the next method's body and reads
        // `max_duration`'s default `None` as a tool id.
        let after = after.trim_start();
        let Some(body) = after.strip_prefix('{') else {
            continue;
        };
        let Some(close) = body.find('}') else {
            continue;
        };
        let body = strip_line_comments(&body[..close]);
        let body = body.trim();
        if let Some(lit) = body
            .strip_prefix('"')
            .and_then(|rest| rest.split_once('"'))
            .map(|(lit, _)| lit)
        {
            out.push(IdImpl::Static(lit.to_string()));
        } else if body.starts_with("self") || body.starts_with("&self") {
            out.push(IdImpl::Dynamic);
        } else if let Some(value) =
            const_value(src, body).or_else(|| corpus.iter().find_map(|c| const_value(c, body)))
        {
            out.push(IdImpl::Static(value));
        } else {
            panic!(
                "unrecognised `Tool::id` body {body:?} — teach this test how to \
                 resolve it, otherwise the tool escapes the inventory guard"
            );
        }
    }
    out
}

/// Drop `//` comment lines from an `fn id` body.
///
/// A comment explaining *why* an id is spelled the way it is belongs next to
/// the id (`undelete_document` has one), and the naive scan would otherwise
/// read the comment text as the body and panic. Only whole comment lines are
/// removed: a `//` inside the string literal itself would have to follow the
/// opening quote on the same line, and no id contains one.
fn strip_line_comments(body: &str) -> String {
    body.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Resolve `const <name>: &str = "<value>";` inside the same file.
fn const_value(src: &str, name: &str) -> Option<String> {
    let needle = format!("const {name}: &str = \"");
    let start = src.find(&needle)? + needle.len();
    let end = src[start..].find('"')? + start;
    Some(src[start..end].to_string())
}

/// Tool ids claimed by an inventory **table row** — i.e. lines of the form
/// `` | `tool_id` | … ``.
///
/// Deliberately not a loose scan for backticked words: the doc also mentions
/// config keys, function names and toggle keys in backticks, so a loose scan
/// would both over-count coverage (a tool named only in prose would pass) and
/// flag every one of those as a stale id. A row is the actual claim.
///
/// Family rows (`` `typst_<id>` ``) carry angle brackets and so never parse as
/// a concrete id — they are matched by prefix instead.
fn documented_ids(doc: &str) -> BTreeSet<String> {
    doc.lines()
        .filter_map(|line| line.trim().strip_prefix("| `"))
        .filter_map(|rest| rest.split_once('`').map(|(id, _)| id))
        .filter(|id| {
            !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .map(str::to_string)
        .collect()
}

fn is_family(id: &str) -> bool {
    id.starts_with(TYPST_PREFIX) || id.starts_with(COMFYUI_PREFIX) || id.starts_with("mcp__")
}

/// Every statically-known tool id, discovered from the source tree.
fn discovered_ids() -> BTreeSet<String> {
    let root = repo_root();
    let mut files = Vec::new();
    rust_sources(&root.join("crates"), &mut files);

    let sources: Vec<String> = files
        .iter()
        .map(|p| std::fs::read_to_string(p).expect("read source"))
        .collect();

    let mut ids = BTreeSet::new();
    let mut dynamic_files = BTreeSet::new();
    for (path, src) in files.iter().zip(&sources) {
        if !src.contains("fn id(&self) -> &str") {
            continue;
        }
        for imp in id_impls(src, &sources) {
            match imp {
                IdImpl::Static(id) if NOT_REAL_TOOLS.contains(&id.as_str()) => {}
                IdImpl::Static(id) => {
                    ids.insert(id);
                }
                IdImpl::Dynamic => {
                    dynamic_files.insert(path.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }

    // A new dynamic family must be acknowledged, or it silently escapes both
    // this guard and the inventory's family table.
    for file in &dynamic_files {
        assert!(
            DYNAMIC_ID_IMPLS.iter().any(|known| file.ends_with(known)),
            "`{file}` implements a runtime-valued `Tool::id` (a dynamic tool \
             family) that this test doesn't know about. Add it to \
             DYNAMIC_ID_IMPLS and document the family in docs/tools-inventory.md."
        );
    }
    assert!(
        !ids.is_empty(),
        "discovered no tool ids at all — the parser broke, not the codebase"
    );
    ids
}

#[test]
fn every_tool_id_is_documented_in_the_inventory() {
    let doc = std::fs::read_to_string(repo_root().join("docs/tools-inventory.md"))
        .expect("docs/tools-inventory.md exists");
    let documented = documented_ids(&doc);

    let missing: Vec<String> = discovered_ids()
        .into_iter()
        .filter(|id| !documented.contains(id) && !UNDOCUMENTED.contains(&id.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "these tools are not in docs/tools-inventory.md: {missing:?}\n\
         Add a row for each (or allow-list it in UNDOCUMENTED with a reason)."
    );
}

#[test]
fn inventory_names_no_tool_that_stopped_existing() {
    let doc = std::fs::read_to_string(repo_root().join("docs/tools-inventory.md"))
        .expect("docs/tools-inventory.md exists");
    let real = discovered_ids();
    let stale: Vec<String> = documented_ids(&doc)
        .into_iter()
        .filter(|id| !real.contains(id) && !is_family(id))
        .collect();
    assert!(
        stale.is_empty(),
        "docs/tools-inventory.md has rows for ids that no tool implements: \
         {stale:?}\nRemove the stale rows (or fix the rename)."
    );
}

#[test]
fn every_tool_has_a_real_category() {
    let unexpected: Vec<String> = discovered_ids()
        .into_iter()
        .filter(|id| {
            catalog::category_for(id) == Category::Utility
                && !EXPECTED_UTILITY.contains(&id.as_str())
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "these tools fall through to Category::Utility: {unexpected:?}\n\
         Give each one a group in `category_for`, or add it to EXPECTED_UTILITY \
         if it really is miscellaneous. Falling through is silent — it renders \
         as an uncategorised row on /tools."
    );
}

#[test]
fn every_visible_tool_has_hand_written_display_copy() {
    let missing: Vec<String> = discovered_ids()
        .into_iter()
        .filter(|id| {
            // Hidden tools are never rendered; family members and the
            // shared-key groups get their copy from the group row instead.
            !catalog::is_hidden(id)
                && !is_family(id)
                && catalog::entry_key_for(id) == id
                && !has_display_copy(id)
        })
        .collect();
    assert!(
        missing.is_empty(),
        "these tools have no entry in `display_meta`, so /tools shows their \
         model-facing schema description instead of plain language: {missing:?}"
    );
}

#[test]
fn chat_only_tools_are_documented_as_such() {
    // The inventory's "Chat-only" column has to agree with the code, or the
    // doc teaches the wrong thing about the /v1 surface.
    let doc = std::fs::read_to_string(repo_root().join("docs/tools-inventory.md"))
        .expect("docs/tools-inventory.md exists");
    for id in discovered_ids() {
        if !requires_chat_session(&id) {
            continue;
        }
        // Find the table row for this id and require a "yes" in it.
        let row = doc
            .lines()
            .find(|line| line.starts_with(&format!("| `{id}`")))
            .unwrap_or_else(|| {
                panic!("chat-only tool `{id}` has no table row in docs/tools-inventory.md")
            });
        assert!(
            row.contains("| yes |"),
            "`{id}` is chat-only in code but its inventory row doesn't say so:\n{row}"
        );
    }
}
