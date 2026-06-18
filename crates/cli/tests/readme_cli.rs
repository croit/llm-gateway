// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Drift guard for the README's **CLI commands** table.
//!
//! Sibling of `gateway/tests/readme_routes.rs`, same idea for the `gw`
//! command surface. The source of truth is the clap command tree itself
//! (introspected via `CommandFactory`, so nested subcommands and clap's
//! kebab-case renaming are reflected exactly — no source parsing), compared
//! against the leaf commands listed in the README's `### CLI (`gw`)` table.
//!
//! Fails CI when they drift either way: a `gw` subcommand exists but isn't
//! in the README table, or the table lists a command the CLI no longer has.

use std::path::Path;

use clap::CommandFactory;
use cli::parser::Cli;

/// Collect the full path of every leaf command (`gw ping`, `gw auth login`,
/// …). Groups that only hold subcommands (`gw auth`) and clap's auto-injected
/// `help` subcommand are not leaves and are skipped.
fn leaf_commands(cmd: &clap::Command, prefix: &str, out: &mut Vec<String>) {
    let mut subs = cmd
        .get_subcommands()
        .filter(|s| s.get_name() != "help")
        .peekable();
    if subs.peek().is_none() {
        out.push(prefix.to_string());
        return;
    }
    for sub in subs {
        leaf_commands(sub, &format!("{prefix} {}", sub.get_name()), out);
    }
}

/// Extract the `gw …` tokens from the first column of the README's
/// `### CLI (`gw`)` table. (The "What it does" column also contains backtick
/// tokens like `--profile` — those live in later columns and are ignored.)
fn documented_commands(readme: &str) -> Vec<String> {
    let start = readme
        .find("### CLI (")
        .expect("README is missing the `### CLI (`gw`)` section");
    let mut out = Vec::new();
    for line in readme[start..].lines().skip(1) {
        let t = line.trim();
        if t.starts_with("## ") || t.starts_with("### ") {
            break; // next section
        }
        if !t.starts_with('|') {
            continue;
        }
        let cell = t.trim_matches('|').split('|').next().unwrap_or("");
        if cell.chars().all(|c| matches!(c, '-' | ':' | ' ')) {
            continue; // header separator row
        }
        for (i, part) in cell.split('`').enumerate() {
            if i % 2 == 1 && part.trim().starts_with("gw ") {
                out.push(part.trim().to_string());
            }
        }
    }
    out
}

#[test]
fn readme_cli_commands_match_clap() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(manifest.join("../../README.md")).expect("read README.md");

    let mut actual = Vec::new();
    leaf_commands(&Cli::command(), "gw", &mut actual);
    let documented = documented_commands(&readme);

    assert!(!actual.is_empty(), "clap exposed no leaf commands");
    assert!(
        !documented.is_empty(),
        "parsed zero commands from the README table"
    );

    let mut errors = Vec::new();
    for cmd in &actual {
        if !documented.contains(cmd) {
            errors.push(format!(
                "  `{cmd}` exists in the CLI but is not in the README commands table"
            ));
        }
    }
    for cmd in &documented {
        if !actual.contains(cmd) {
            errors.push(format!(
                "  README documents `{cmd}` but the CLI has no such command"
            ));
        }
    }

    assert!(
        errors.is_empty(),
        "README CLI commands table is out of sync with the `gw` clap definitions:\n{}",
        errors.join("\n")
    );
}
