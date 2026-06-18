// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use std::collections::HashMap;
use std::sync::Arc;

use shared::api::{ToolDef, ToolSummary};

use super::Tool;

/// Holds every registered tool in an Arc so handlers can fan out to N tools
/// concurrently without cloning each impl.
#[derive(Default)]
pub struct ToolRegistry {
    // Keyed on an owned `String` rather than `&'static str`: built-in tools
    // return string literals from `id()`, but MCP-bridged tools (see
    // `tools::mcp`) carry runtime-built ids (`mcp__<server>__<tool>`), so the
    // key can't be `'static`.
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tool. Panics if a tool with the same id has already been
    /// registered — IDs are a stable contract; collisions are a bug,
    /// not a runtime decision.
    ///
    /// Also asserts the id matches OpenAI's function-name regex
    /// (`^[a-zA-Z0-9_-]{1,64}$`). Tools whose id violates it (most
    /// commonly: a `.` in a dotted-namespace id like `company.echo`)
    /// silently break against strict tool-call parsers — qwen3-coder
    /// is the one that bit us — because the parser either drops the
    /// call entirely or rewrites the name before emitting `tool_calls`.
    /// Catching this at registration means we can't ship a tool that
    /// randomly fails on some upstreams. Use `_` for namespacing
    /// (`company_echo`).
    pub fn with<T: Tool>(mut self, tool: T) -> Self {
        let id = tool.id().to_string();
        assert!(
            is_openai_function_name(&id),
            "tool id `{id}` is not a valid OpenAI function name \
             (`^[a-zA-Z0-9_-]{{1,64}}$`); replace `.` with `_`"
        );
        assert!(
            !self.tools.contains_key(&id),
            "duplicate tool id `{id}` in ToolRegistry"
        );
        self.tools.insert(id, Arc::new(tool));
        self
    }

    pub fn get(&self, id: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.tools.contains_key(id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn Tool>)> {
        self.tools.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Collects the OpenAI-shaped definitions for every tool whose id appears
    /// in `allowed`. Order matches the iteration order of `allowed` so callers
    /// can present them deterministically.
    pub fn defs_for(&self, allowed: &[String]) -> Vec<ToolDef> {
        allowed
            .iter()
            .filter_map(|id| self.tools.get(id.as_str()))
            .map(|t| t.schema())
            .collect()
    }

    /// UI-friendly view: id + display name + description for every tool whose
    /// id is in `allowed`. The name comes from the OpenAI schema; for the
    /// majority of tools `id` and `schema().function.name` are the same
    /// string.
    pub fn summaries_for(&self, allowed: &[String]) -> Vec<ToolSummary> {
        allowed
            .iter()
            .filter_map(|id| self.tools.get(id.as_str()).map(|t| (id, t.schema())))
            .map(|(id, def)| ToolSummary {
                id: id.clone(),
                name: def.function.name,
                description: def.function.description,
            })
            .collect()
    }
}

/// The character class OpenAI allows in a function name (and thus a tool
/// id): ASCII alphanumerics, `_`, `-`. Shared with the MCP id sanitizer
/// (`tools::mcp::sanitize_tool_id`) so a sanitized id can never fail the
/// validation below.
pub(crate) fn is_openai_function_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// True if `id` matches OpenAI's `^[a-zA-Z0-9_-]{1,64}$` for tool /
/// function names. Hand-rolled instead of pulling `regex` — the rule
/// is one line and the comparison runs at registration only.
fn is_openai_function_name(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(is_openai_function_name_char)
}

#[cfg(test)]
mod tests {
    use super::super::echo::Echo;
    use super::super::time::CurrentTimestamp;
    use super::*;

    #[test]
    fn empty_registry() {
        let r = ToolRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.get("anything").is_none());
    }

    #[test]
    fn with_registers_and_lookup_works() {
        let r = ToolRegistry::new().with(Echo).with(CurrentTimestamp);
        assert_eq!(r.len(), 2);
        assert!(r.contains(Echo.id()));
        assert!(r.contains(CurrentTimestamp.id()));
        assert!(!r.contains("nope"));
    }

    #[test]
    #[should_panic(expected = "duplicate tool id")]
    fn duplicate_id_panics() {
        let _ = ToolRegistry::new().with(Echo).with(Echo);
    }

    #[test]
    fn defs_for_returns_only_allowed_in_order() {
        let r = ToolRegistry::new().with(Echo).with(CurrentTimestamp);
        let defs = r.defs_for(&["get_current_timestamp".into(), "company_echo".into()]);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].function.name, "get_current_timestamp");
        assert_eq!(defs[1].function.name, "company_echo");
    }

    #[test]
    fn defs_for_silently_skips_unknown_ids() {
        let r = ToolRegistry::new().with(Echo);
        let defs = r.defs_for(&["company_echo".into(), "nope".into()]);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "company_echo");
    }

    #[test]
    fn summaries_carry_description() {
        let r = ToolRegistry::new().with(Echo);
        let summaries = r.summaries_for(&["company_echo".into()]);
        assert_eq!(summaries.len(), 1);
        assert!(!summaries[0].description.is_empty());
    }
}
