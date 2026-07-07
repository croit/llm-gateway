// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Fails the build if any locale under `locales/` is missing a
//! translation that `locales/en/*.ftl` (the source of truth) has, or
//! carries a stray key `en` doesn't (almost always a typo). Translations
//! aren't optional here — every UI string ships in all 6 languages by
//! construction, not by someone remembering to run a checklist.
//!
//! Deliberately dependency-free: a real Fluent parser is overkill for
//! extracting message ids, since by convention (see `session_core::i18n`
//! docs) we only use flat `key = value` messages, never `key.attr = …`
//! attributes. A message id is any line starting in column 0 with an
//! identifier followed by `=`; comments (`#`) and multi-line value
//! continuations (always indented in `.ftl`) don't match that shape.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const LANGS: [&str; 6] = ["en", "de", "fr", "es", "ru", "zh"];

fn message_keys(dir: &Path) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return keys;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ftl") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            if line.starts_with(char::is_whitespace) {
                continue; // continuation of a multi-line value
            }
            let Some((candidate, _)) = line.split_once('=') else {
                continue;
            };
            let candidate = candidate.trim();
            let is_message_id = candidate
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
                && candidate
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if is_message_id {
                keys.insert(candidate.to_string());
            }
        }
    }
    keys
}

fn main() {
    // `include_str!` (in `static_loader!`) already makes cargo track edits
    // to existing files; this also catches new/removed files, which
    // `include_str!` alone can't (see the fluent-templates#2 upstream
    // issue) — and it's what re-runs this completeness check on every
    // locale edit.
    println!("cargo:rerun-if-changed=locales");

    let locales_dir = Path::new("locales");
    let by_lang: Vec<(&str, BTreeSet<String>)> = LANGS
        .iter()
        .map(|&lang| (lang, message_keys(&locales_dir.join(lang))))
        .collect();

    let en_keys = &by_lang.iter().find(|(l, _)| *l == "en").unwrap().1;
    if en_keys.is_empty() {
        // Nothing extracted yet — don't fail an otherwise-empty checkout.
        return;
    }

    let mut report = String::new();
    for (lang, keys) in &by_lang {
        if *lang == "en" {
            continue;
        }
        let missing: Vec<&str> = en_keys.difference(keys).map(String::as_str).collect();
        if !missing.is_empty() {
            report.push_str(&format!(
                "\n  [{lang}] missing {} translation(s): {missing:?}",
                missing.len()
            ));
        }
        let stray: Vec<&str> = keys.difference(en_keys).map(String::as_str).collect();
        if !stray.is_empty() {
            report.push_str(&format!(
                "\n  [{lang}] has {} key(s) not present in en (typo, or en is missing them): {stray:?}",
                stray.len()
            ));
        }
    }

    if !report.is_empty() {
        panic!(
            "i18n: locale files are out of sync with locales/en/*.ftl (the source of truth).{report}\n\
             Every message key must be translated in all 6 languages — add the missing key(s) \
             to the listed locale's .ftl file (or remove the stray one) before this will build."
        );
    }
}
