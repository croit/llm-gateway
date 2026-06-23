// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The user-facing tool catalog: turns the flat `ToolRegistry` into the
//! grouped, de-noised list the `/tools` page renders, and provides the
//! mapping that the request path uses to honour a user's on/off
//! choices.
//!
//! Two concerns live here so they can't drift apart:
//!   - **Display**: [`entries`] groups tools into [`Category`]s, hides
//!     smoke-test-only tools, and folds the per-template `typst_<id>`
//!     family into a single "Document rendering" toggle.
//!   - **Enforcement**: [`entry_key_for`] / [`retain_enabled`] map a
//!     registered tool id to its toggle key and drop the ids whose key
//!     the user disabled. The page and the proxy therefore agree on
//!     exactly what one toggle controls.

use std::collections::HashSet;

use super::ToolRegistry;

/// All `typst_<id>` tools share this single toggle key — one switch
/// governs the whole document-rendering family rather than one per
/// template (templates come and go; the capability is what the user
/// reasons about).
const TYPST_PREFIX: &str = "typst_";
const TYPST_KEY: &str = "typst";

/// The `remember` + `recall` tools are two halves of one capability, so
/// they collapse to a single "memory" toggle — one switch turns
/// per-user memory on or off as a whole.
const MEMORY_IDS: &[&str] = &["remember", "recall"];
const MEMORY_KEY: &str = "memory";

/// Tools that exist for smoke tests / internal plumbing and shouldn't
/// clutter a user's tool list. They stay granted via RBAC; they're just
/// not presented as a toggle.
const HIDDEN: &[&str] = &["company_echo"];

/// Display grouping for the tool list. Ordered by [`Category::order`]
/// so the page renders sections deterministically.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    Web,
    Documents,
    /// Semantic search over operator-indexed knowledge bases (`rag_*`) —
    /// the user's own repositories and documents.
    Knowledge,
    /// Sandboxed code execution (`run_in_sandbox` + its presets).
    Code,
    Memory,
    /// Tools bridged from an external MCP server (`mcp__*`).
    Integrations,
    Utility,
}

impl Category {
    /// Section heading shown on the page.
    pub fn label(self) -> &'static str {
        match self {
            Category::Web => "Web & Network",
            Category::Documents => "Attachments & Documents",
            Category::Knowledge => "Knowledge base",
            Category::Code => "Code & Sandbox",
            Category::Memory => "Memory",
            Category::Integrations => "Integrations",
            Category::Utility => "Utility",
        }
    }

    /// Render order — lower sorts first.
    pub fn order(self) -> u8 {
        match self {
            Category::Web => 0,
            Category::Documents => 1,
            Category::Knowledge => 2,
            Category::Code => 3,
            Category::Memory => 4,
            Category::Integrations => 5,
            Category::Utility => 6,
        }
    }
}

/// One row on the `/tools` page. `key` is the stable toggle identity
/// persisted in `user_tool_prefs`; `title` is the human-readable name,
/// `tech` the underlying function name shown as a subtle mono badge,
/// and `description` the plain-language "what + how".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolEntry {
    pub key: String,
    pub title: String,
    pub tech: String,
    pub description: String,
    pub category: Category,
}

/// Tool id of the lone always-on bootstrap. It can't itself be enabled
/// via the per-conversation overlay (chicken-and-egg), so it's the one
/// exception baked into [`allowed_tools_for_session`]. Every other tool —
/// including memory, time, location, web — is lazy and turned on by the
/// model calling this one with the relevant key.
pub const BOOTSTRAP_TOOL_ID: &str = "enable_tools";

/// The toggle key that governs a registered tool id. `typst_<id>` all
/// collapse to `typst`; every other tool is its own key. Pure + cheap
/// so the request path can call it per id without touching the DB.
pub fn entry_key_for(tool_id: &str) -> &str {
    if tool_id.starts_with(TYPST_PREFIX) {
        TYPST_KEY
    } else if MEMORY_IDS.contains(&tool_id) {
        MEMORY_KEY
    } else if tool_id.starts_with(crate::server::tools::mcp::MCP_ID_PREFIX) {
        // All of one MCP server's tools collapse to a single toggle, so a
        // user enables/disables the whole integration at once.
        mcp_server_key(tool_id)
    } else {
        tool_id
    }
}

/// Whether a tool id is hidden from every user-facing surface — both the
/// `/tools` settings page and the model-facing `enable_tools` catalog.
/// Smoke-test / internal-plumbing tools stay granted via RBAC; they're just
/// never advertised as a toggle. Single source of truth so the page and the
/// bootstrap tool can't drift on what counts as hidden.
pub fn is_hidden(tool_id: &str) -> bool {
    HIDDEN.contains(&tool_id)
}

/// `mcp__<server>__<tool>` → `mcp__<server>` (the per-server toggle key).
/// Falls back to the whole id if the shape is unexpected.
fn mcp_server_key(tool_id: &str) -> &str {
    let prefix = crate::server::tools::mcp::MCP_ID_PREFIX;
    let after = &tool_id[prefix.len()..];
    match after.find("__") {
        Some(i) => &tool_id[..prefix.len() + i],
        None => tool_id,
    }
}

/// Display category for a tool id. Unknown / future tools fall into
/// `Utility` and render as their own 1:1 row — a graceful default that
/// never hides a newly added tool.
fn category_for(tool_id: &str) -> Category {
    match tool_id {
        "search_web" | "fetch_url" | "lookup_ip" | "dns_lookup" | "whois_lookup" | "tls_cert"
        | "wikipedia" => Category::Web,
        "fetch_attachment" | "upload_attachment" | "read_skill" => Category::Documents,
        "rag_search" | "rag_list_collections" => Category::Knowledge,
        "run_in_sandbox" | "generate_document" | "capture_webpage" | "read_sandbox_output" => {
            Category::Code
        }
        _ if tool_id.starts_with(TYPST_PREFIX) => Category::Documents,
        "remember" | "recall" => Category::Memory,
        _ if tool_id.starts_with(crate::server::tools::mcp::MCP_ID_PREFIX) => {
            Category::Integrations
        }
        // `get_user_location` and any future tool fall here.
        _ => Category::Utility,
    }
}

/// First sentence of a model-facing tool description, for a compact UI
/// label. Falls back to the whole string when there's no sentence
/// break.
fn short_description(full: &str) -> String {
    match full.find(". ") {
        Some(end) => full[..=end].trim().to_string(),
        None => full.trim().to_string(),
    }
}

/// Curated, user-facing `(title, description)` for a known tool id. The
/// model-facing `schema().description` is written for the LLM and reads
/// terse / jargon-y in a settings list, so we hand-write plain-language
/// copy here. Unknown / future tools fall back to their schema text
/// (see [`entries`]).
fn display_meta(tool_id: &str) -> Option<(&'static str, &'static str)> {
    let meta = match tool_id {
        "search_web" => (
            "Web search",
            "Searches the web and returns a short list of results — each with a title, link, \
             and snippet. For current events, niche facts, or anything newer than the model's \
             training data.",
        ),
        "fetch_url" => (
            "Fetch a web page",
            "Loads a specific http(s) URL and returns its text (or image) so the assistant can \
             read and quote the actual page or file — instead of guessing.",
        ),
        "fetch_attachment" => (
            "Read an attachment",
            "Opens a file you attached to the chat and reads its contents, so the assistant can \
             summarise, quote, or work from it.",
        ),
        "upload_attachment" => (
            "Attach a file to replies",
            "Lets the assistant attach a file it generated — a document, image, or data export — \
             to its answer so you can download it.",
        ),
        "get_current_timestamp" => (
            "Current date & time",
            "Gives the assistant today's date and the current time in your timezone — for \
             questions like \"what's due today\" or scheduling.",
        ),
        "get_user_location" => (
            "Your location",
            "Lets the assistant figure out where you are — for \"weather here\", \"near me\", \
             and similar — from a precise location you share or, failing that, an approximate \
             one from your IP address.",
        ),
        "lookup_ip" => (
            "IP / host location",
            "Looks up where any IP address or hostname is — country, region, city, and rough \
             coordinates — from the gateway's offline IP2Location database, so the assistant \
             can answer \"where is this IP?\" without searching the web.",
        ),
        "dns_lookup" => (
            "DNS lookup",
            "Resolves DNS records for a hostname (addresses, mail servers, TXT, etc.) so the \
             assistant can answer \"what does this domain resolve to?\" with live data.",
        ),
        "whois_lookup" => (
            "Domain WHOIS",
            "Looks up a domain's registration details — registrar, creation/expiry dates, \
             status, nameservers — via RDAP (the modern WHOIS).",
        ),
        "tls_cert" => (
            "TLS certificate check",
            "Inspects the TLS certificate a site presents — issuer, validity dates, days until \
             expiry, and covered hostnames — so the assistant can answer \"is this cert about \
             to expire?\".",
        ),
        "wikipedia" => (
            "Wikipedia lookup",
            "Fetches the summary of the best-matching Wikipedia article for encyclopedic \
             \"who/what/where is X\" questions, with a link to the full page.",
        ),
        "convert_currency" => (
            "Currency conversion",
            "Converts an amount between currencies using daily ECB reference rates, so the \
             assistant gives a real figure instead of guessing the exchange rate.",
        ),
        "rag_search" => (
            "Knowledge-base search",
            "Lets the assistant semantically search the repositories and documents indexed \
             into this gateway — your own codebase, docs, or data — and quote the matching \
             passages, instead of guessing or searching the public web.",
        ),
        "rag_list_collections" => (
            "List knowledge bases",
            "Lets the assistant discover which indexed collections exist before searching \
             them, so it queries the right repository or document set.",
        ),
        "read_skill" => (
            "Skills",
            "Lets the assistant load an operator-installed skill — brand guidelines, house \
             style, domain playbooks — and apply it to what it writes or builds for you.",
        ),
        "run_in_sandbox" => (
            "Code sandbox",
            "Lets the assistant run Python or shell in a secure throwaway sandbox — for data \
             analysis, charts, calculations, running command-line tools, and generating \
             files it returns to you. Each run is isolated and starts clean.",
        ),
        "generate_document" => (
            "Document generation",
            "Lets the assistant turn its writing into a finished PDF, Word, or PowerPoint \
             file you can download — built in the sandbox from Markdown.",
        ),
        "capture_webpage" => (
            "Web page capture",
            "Lets the assistant open a web page in a headless browser and hand you a \
             full-page screenshot, a PDF, or its extracted text. Needs operator-enabled \
             network access.",
        ),
        "read_sandbox_output" => (
            "Read large sandbox output",
            "Lets the assistant grep or page through a large result a previous sandbox \
             run produced, without pulling the whole thing back into the conversation.",
        ),
        _ => return None,
    };
    Some(meta)
}

/// The distinct high-level capability *areas* the registry actually offers,
/// as user-facing category labels in display order. Drives the one-line
/// "here's what you can turn on" hint in the model's system context: domains,
/// not individual tools, so it stays cheap and the model still calls
/// `enable_tools` for the exact keys. Derived from the live registry, so a
/// deployment without (say) a sandbox or an indexer never advertises that
/// area. MCP integrations are excluded — they're listed per-user in the
/// system context already; hidden, bootstrap, and the skill loader don't
/// count toward an area on their own.
pub fn capability_domains(registry: &ToolRegistry) -> Vec<&'static str> {
    let mut seen: Vec<Category> = Vec::new();
    for id in registry.ids() {
        if is_hidden(id) || id == BOOTSTRAP_TOOL_ID || id == "read_skill" {
            continue;
        }
        if id.starts_with(crate::server::tools::mcp::MCP_ID_PREFIX) {
            continue;
        }
        let cat = category_for(id);
        if cat == Category::Integrations {
            continue;
        }
        if !seen.contains(&cat) {
            seen.push(cat);
        }
    }
    seen.sort_by_key(|c| c.order());
    seen.into_iter().map(Category::label).collect()
}

/// Build the grouped, de-noised toggle list from the tool ids the
/// user's roles grant. Hidden tools are dropped; the `typst_<id>`
/// family is folded into a single entry. Sorted by category then key
/// so the page is stable across requests.
pub fn entries(registry: &ToolRegistry, allowed: &[String]) -> Vec<ToolEntry> {
    let mut out: Vec<ToolEntry> = Vec::new();
    let mut typst_seen = false;
    let mut memory_seen = false;
    let mut mcp_servers_seen: HashSet<String> = HashSet::new();

    for id in allowed {
        if is_hidden(id) {
            continue;
        }
        if MEMORY_IDS.contains(&id.as_str()) {
            if !memory_seen {
                memory_seen = true;
                out.push(ToolEntry {
                    key: MEMORY_KEY.to_string(),
                    title: "Memory".to_string(),
                    tech: "remember + recall".to_string(),
                    description: "Lets the assistant remember durable facts about you \
                                  (preferences, ongoing projects) and recall them in later \
                                  conversations. Stored only for your account."
                        .to_string(),
                    category: Category::Memory,
                });
            }
            continue;
        }
        if id.starts_with(TYPST_PREFIX) {
            if !typst_seen {
                typst_seen = true;
                out.push(ToolEntry {
                    key: TYPST_KEY.to_string(),
                    title: "Document rendering".to_string(),
                    tech: format!("{TYPST_PREFIX}*"),
                    description: "Fills a Typst document template (e.g. invoice, letter, report) \
                                  and returns a finished PDF and PNG to download."
                        .to_string(),
                    category: Category::Documents,
                });
            }
            continue;
        }
        // All of one MCP server's tools collapse to a single toggle, keyed
        // `mcp__<server>` — the integration is what the user reasons about,
        // and a server can expose a dozen+ tools. The key matches
        // `entry_key_for`, so the toggle actually governs every tool.
        if id.starts_with(crate::server::tools::mcp::MCP_ID_PREFIX) {
            let key = entry_key_for(id);
            if mcp_servers_seen.insert(key.to_string()) {
                let server = key
                    .strip_prefix(crate::server::tools::mcp::MCP_ID_PREFIX)
                    .unwrap_or(key);
                out.push(ToolEntry {
                    key: key.to_string(),
                    title: format!("{server} (MCP)"),
                    tech: format!("{key}__*"),
                    description: format!(
                        "Tools bridged from the \"{server}\" MCP server (Model Context \
                         Protocol). One switch enables or disables the whole integration."
                    ),
                    category: Category::Integrations,
                });
            }
            continue;
        }
        let Some(tool) = registry.get(id) else {
            continue;
        };
        let def = tool.schema();
        let (title, description) = match display_meta(id) {
            Some((t, d)) => (t.to_string(), d.to_string()),
            None => (
                def.function.name.clone(),
                short_description(&def.function.description),
            ),
        };
        out.push(ToolEntry {
            key: id.clone(),
            title,
            tech: def.function.name,
            description,
            category: category_for(id),
        });
    }

    out.sort_by(|a, b| {
        a.category
            .order()
            .cmp(&b.category.order())
            .then_with(|| a.key.cmp(&b.key))
    });
    out
}

/// Drop every granted tool id whose toggle key the user disabled.
/// Honours the `typst` collapse: disabling `typst` removes all
/// `typst_<id>` ids at once.
pub fn retain_enabled(allowed: &mut Vec<String>, disabled_keys: &HashSet<String>) {
    allowed.retain(|id| !disabled_keys.contains(entry_key_for(id)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tools::echo::Echo;
    use crate::server::tools::search_web::SearchWeb;
    use crate::server::tools::time::CurrentTimestamp;

    #[test]
    fn typst_ids_share_one_key_others_are_one_to_one() {
        assert_eq!(entry_key_for("typst_invoice"), "typst");
        assert_eq!(entry_key_for("typst_report"), "typst");
        assert_eq!(entry_key_for("search_web"), "search_web");
    }

    #[test]
    fn capability_domains_derive_from_registry_and_skip_hidden() {
        use crate::server::tools::rag::RagSearch;
        let reg = ToolRegistry::new()
            .with(SearchWeb)
            .with(RagSearch)
            .with(Echo);
        // Echo (`company_echo`) is hidden → its area never shows. Order is by
        // Category::order: Web before Knowledge.
        assert_eq!(
            capability_domains(&reg),
            vec!["Web & Network", "Knowledge base"]
        );
    }

    #[test]
    fn rag_search_gets_a_curated_entry_in_the_knowledge_area() {
        use crate::server::tools::rag::RagSearch;
        let reg = ToolRegistry::new().with(RagSearch);
        let entries = entries(&reg, &["rag_search".to_string()]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].category, Category::Knowledge);
        // Curated title from `display_meta`, not the raw schema function name.
        assert_eq!(entries[0].title, "Knowledge-base search");
    }

    #[test]
    fn entries_hide_smoke_test_tools() {
        let reg = ToolRegistry::new().with(Echo).with(SearchWeb);
        let allowed = vec!["company_echo".to_string(), "search_web".to_string()];
        let entries = entries(&reg, &allowed);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "search_web");
        assert_eq!(entries[0].category, Category::Web);
    }

    #[test]
    fn entries_are_grouped_then_sorted_by_key() {
        let reg = ToolRegistry::new().with(SearchWeb).with(CurrentTimestamp);
        let allowed = vec![
            "get_current_timestamp".to_string(),
            "search_web".to_string(),
        ];
        let entries = entries(&reg, &allowed);
        // Web (search_web) sorts before Utility (get_current_timestamp).
        assert_eq!(entries[0].key, "search_web");
        assert_eq!(entries[1].key, "get_current_timestamp");
        assert_eq!(entries[1].category, Category::Utility);
    }

    #[test]
    fn retain_enabled_drops_disabled_and_keeps_rest() {
        let mut allowed = vec![
            "search_web".to_string(),
            "fetch_url".to_string(),
            "typst_invoice".to_string(),
        ];
        let disabled: HashSet<String> = ["search_web".to_string()].into_iter().collect();
        retain_enabled(&mut allowed, &disabled);
        assert_eq!(allowed, vec!["fetch_url", "typst_invoice"]);
    }

    #[test]
    fn remember_and_recall_collapse_to_one_memory_entry() {
        use crate::server::tools::memory::{Recall, Remember};
        let reg = ToolRegistry::new().with(Remember).with(Recall);
        let allowed = vec!["remember".to_string(), "recall".to_string()];
        let entries = entries(&reg, &allowed);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "memory");
        assert_eq!(entries[0].category, Category::Memory);
    }

    #[test]
    fn both_memory_ids_map_to_the_memory_key() {
        assert_eq!(entry_key_for("remember"), "memory");
        assert_eq!(entry_key_for("recall"), "memory");
    }

    #[test]
    fn disabling_memory_key_drops_both_tools() {
        let mut allowed = vec![
            "remember".to_string(),
            "recall".to_string(),
            "fetch_url".to_string(),
        ];
        let disabled: HashSet<String> = ["memory".to_string()].into_iter().collect();
        retain_enabled(&mut allowed, &disabled);
        assert_eq!(allowed, vec!["fetch_url"]);
    }

    #[test]
    fn disabling_typst_key_drops_all_templates() {
        let mut allowed = vec![
            "typst_invoice".to_string(),
            "typst_report".to_string(),
            "fetch_url".to_string(),
        ];
        let disabled: HashSet<String> = ["typst".to_string()].into_iter().collect();
        retain_enabled(&mut allowed, &disabled);
        assert_eq!(allowed, vec!["fetch_url"]);
    }

    #[test]
    fn mcp_tools_collapse_to_one_entry_per_server() {
        // The MCP branch builds its entry from the id alone (no registry
        // lookup), so an empty registry is fine here.
        let reg = ToolRegistry::new();
        let allowed = vec![
            "mcp__demo__echo".to_string(),
            "mcp__demo__get-sum".to_string(),
            "mcp__other__ping".to_string(),
        ];
        let mcp: Vec<_> = entries(&reg, &allowed)
            .into_iter()
            .filter(|e| e.category == Category::Integrations)
            .collect();
        assert_eq!(mcp.len(), 2, "two servers → two entries: {mcp:?}");
        assert!(mcp.iter().any(|e| e.key == "mcp__demo"));
        assert!(mcp.iter().any(|e| e.key == "mcp__other"));
    }

    #[test]
    fn disabling_an_mcp_server_drops_all_its_tools() {
        // Guards the display-key (`entry_key_for`) ↔ toggle-key consistency:
        // the `/tools` row is keyed `mcp__demo`, so disabling it must drop
        // every `mcp__demo__*` id.
        let mut allowed = vec![
            "mcp__demo__echo".to_string(),
            "mcp__demo__get-sum".to_string(),
            "search_web".to_string(),
        ];
        let disabled: HashSet<String> = ["mcp__demo".to_string()].into_iter().collect();
        retain_enabled(&mut allowed, &disabled);
        assert_eq!(allowed, vec!["search_web"]);
    }
}
