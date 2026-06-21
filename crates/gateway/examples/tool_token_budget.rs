// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Phase 0 measurement for tool-context-optimization: how many bytes /
//! (estimated) tokens does the always-on `tools` block actually cost?
//!
//! Builds a registry with every statically-constructible built-in tool
//! (plus `lookup_ip` and any discovered `typst_*` templates), serialises
//! each tool's `schema()` to the *exact* compact JSON the proxy injects
//! (`runner::inject_tools` → `defs_for` → `serde_json::to_vec`), and
//! prints a per-tool / per-group / total breakdown.
//!
//! Token figure is an estimate (`chars / CHARS_PER_TOKEN`); the byte and
//! char counts are exact. The full block is also dumped to
//! `/tmp/tool_block.json` so it can be tokenised with the real model
//! tokenizer for a precise number.
//!
//! Run: `cargo run -p gateway --example tool_token_budget`

use std::path::Path;

use gateway::server::tools::{ToolRegistry, catalog};

/// Rough BPE density for dense JSON (lots of short tokens: braces,
/// quotes, field names). Qwen/GPT-class tokenizers land ~3.5–4 chars
/// per token on this kind of text; 3.7 is a defensible midpoint.
const CHARS_PER_TOKEN: f64 = 3.7;

fn main() {
    let mut registry = ToolRegistry::new()
        .with(gateway::server::tools::echo::Echo)
        .with(gateway::server::tools::time::CurrentTimestamp)
        .with(gateway::server::tools::fetch_url::FetchUrl)
        .with(gateway::server::tools::fetch_attachment::FetchAttachment)
        .with(gateway::server::tools::upload_attachment::UploadAttachment)
        .with(gateway::server::tools::search_web::SearchWeb)
        .with(gateway::server::tools::location::GetUserLocation)
        .with(gateway::server::tools::memory::Remember)
        .with(gateway::server::tools::memory::Recall)
        .with(gateway::server::tools::netcheck::DnsLookup)
        .with(gateway::server::tools::netcheck::WhoisLookup)
        .with(gateway::server::tools::netcheck::TlsCert)
        .with(gateway::server::tools::wikipedia::Wikipedia)
        .with(gateway::server::tools::currency::ConvertCurrency)
        .with(gateway::server::tools::lookup_ip::LookupIp);

    // Discover typst templates the same way main.rs does, if present.
    let templates_dir = Path::new("examples/typst-templates");
    match gateway::server::typst::discover_templates(templates_dir) {
        Ok(templates) => {
            for t in templates {
                let t = std::sync::Arc::new(t);
                // Token-budget report only needs the schemas; pptx export
                // (sandbox) is irrelevant here, so pass None.
                registry = registry
                    .with(gateway::server::tools::typst_render::TypstRenderTool::new(
                        t.clone(),
                        None,
                    ))
                    .with(gateway::server::tools::typst_render::TypstEditTool::new(
                        t, None,
                    ));
            }
        }
        Err(e) => eprintln!("(no typst templates discovered at {templates_dir:?}: {e})"),
    }

    // Stable id order for a deterministic report.
    let mut ids: Vec<String> = registry.ids().map(|s| s.to_string()).collect();
    ids.sort();

    // Per-tool sizes from the exact injected schema JSON.
    struct Row {
        id: String,
        group: String,
        chars: usize,
        bytes: usize,
    }
    let mut rows: Vec<Row> = Vec::new();
    for id in &ids {
        let def = &registry.defs_for(std::slice::from_ref(id))[0];
        let json = serde_json::to_string(def).unwrap();
        rows.push(Row {
            id: id.clone(),
            group: catalog::entry_key_for(id).to_string(),
            chars: json.chars().count(),
            bytes: json.len(),
        });
    }

    // Full block as actually injected (array of ToolDef, compact).
    let all_defs = registry.defs_for(&ids);
    let block_json = serde_json::to_string(&all_defs).unwrap();
    std::fs::write("/tmp/tool_block.json", &block_json).ok();

    let tok = |chars: usize| (chars as f64 / CHARS_PER_TOKEN).round() as usize;

    println!("\n=== Per-tool tool-block cost (exact JSON, est. tokens) ===\n");
    println!(
        "{:<26} {:<14} {:>7} {:>7} {:>8}",
        "tool id", "group", "chars", "bytes", "~tokens"
    );
    println!("{}", "-".repeat(66));
    for r in &rows {
        println!(
            "{:<26} {:<14} {:>7} {:>7} {:>8}",
            r.id,
            r.group,
            r.chars,
            r.bytes,
            tok(r.chars)
        );
    }

    // Aggregate by catalog group (the enablement unit).
    use std::collections::BTreeMap;
    let mut by_group: BTreeMap<String, (usize, usize)> = BTreeMap::new(); // group -> (chars, count)
    for r in &rows {
        let e = by_group.entry(r.group.clone()).or_default();
        e.0 += r.chars;
        e.1 += 1;
    }
    println!("\n=== Per-group (catalog toggle key) ===\n");
    println!(
        "{:<16} {:>5} {:>8} {:>8}",
        "group", "tools", "chars", "~tokens"
    );
    println!("{}", "-".repeat(40));
    for (g, (chars, count)) in &by_group {
        println!("{:<16} {:>5} {:>8} {:>8}", g, count, chars, tok(*chars));
    }

    let total_chars: usize = rows.iter().map(|r| r.chars).sum();
    let total_bytes: usize = rows.iter().map(|r| r.bytes).sum();
    println!("\n=== Total ===");
    println!("tools:        {}", rows.len());
    println!("total chars:  {total_chars}");
    println!("total bytes:  {total_bytes}");
    println!(
        "full block:   {} chars (array w/ separators)",
        block_json.chars().count()
    );
    println!(
        "~tokens:      {} (at {CHARS_PER_TOKEN} chars/token)",
        tok(block_json.chars().count())
    );
    println!("\nFull injected block written to /tmp/tool_block.json for precise tokenisation.");
}
