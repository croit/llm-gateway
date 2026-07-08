// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Text → spoken-prose sanitiser for the voice-conversation TTS path.
//!
//! In voice mode the model is *told* (via the injected brevity directive) to
//! answer in one or two plain spoken sentences. But models don't always comply,
//! so before any text is sent to the TTS backend we run it through
//! [`to_spoken`], which turns residual Markdown into something worth hearing:
//!
//! - fenced code blocks and Markdown tables → a short spoken marker (reading
//!   `asterisk asterisk` or a 10-row table aloud is useless);
//! - links `[text](url)` → just `text` (the URL is unspeakable);
//! - inline emphasis / backticks / heading & list markers → stripped;
//! - whitespace collapsed.
//!
//! The markers are passed in by the caller so they can be localised to the
//! spoken language (see the `voice-*-marker` i18n keys). This is applied ONLY
//! on the voice-mode `POST /api/v0/speech` path — the raw `POST /v1/audio/speech`
//! proxy forwards `input` verbatim, preserving OpenAI 1:1 semantics.

/// Markers substituted for non-speakable blocks, localised by the caller.
pub struct SpokenMarkers<'a> {
    /// Spoken in place of a fenced code block, e.g. "The code is shown on screen."
    pub code: &'a str,
    /// Spoken in place of a Markdown table, e.g. "A table is shown on screen."
    pub table: &'a str,
}

/// Convert a chunk of assistant Markdown into speakable plain prose. Safe on a
/// whole reply or a single streamed sentence. Pure — unit-tested below.
pub fn to_spoken(text: &str, markers: &SpokenMarkers<'_>) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_code = false;
    let mut code_emitted = false;
    let mut prev_was_table = false;

    for line in text.lines() {
        let trimmed = line.trim();

        // Fenced code block: ``` or ~~~ toggles. While inside, drop content;
        // emit the marker once per block on the opening fence.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            if !in_code {
                in_code = true;
                if !code_emitted {
                    out.push(markers.code.to_string());
                    code_emitted = true;
                }
            } else {
                in_code = false;
                code_emitted = false;
            }
            continue;
        }
        if in_code {
            continue;
        }

        // Markdown table row (`| a | b |` or the `|---|` separator). Collapse a
        // contiguous run of table lines into one spoken marker.
        if is_table_row(trimmed) {
            if !prev_was_table {
                out.push(markers.table.to_string());
                prev_was_table = true;
            }
            continue;
        }
        prev_was_table = false;

        let spoken = strip_inline(trimmed);
        if !spoken.trim().is_empty() {
            out.push(spoken);
        }
    }

    // Join and collapse whitespace so the TTS gets clean prose.
    out.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A line that is part of a Markdown table: a pipe-delimited row, or the
/// `|---|:--:|` header separator. Requires at least one interior `|` so a lone
/// sentence containing a pipe isn't misread as a table.
fn is_table_row(line: &str) -> bool {
    if !line.starts_with('|') {
        return false;
    }
    line.matches('|').count() >= 2
}

/// Strip inline Markdown from a single (non-code, non-table) line: heading /
/// blockquote / list markers at the start, link syntax, and emphasis/backtick
/// characters. Conservative — it only removes markup, never words.
fn strip_inline(line: &str) -> String {
    // Leading block markers: headings (`#`), blockquotes (`>`), list bullets
    // (`-`, `*`, `+`), and ordered-list numbers (`1.`).
    let mut s = line.trim_start();
    loop {
        let before = s;
        s = s.trim_start_matches(['#', '>', '-', '*', '+', ' ']);
        if let Some(rest) = strip_ordered_list_marker(s) {
            s = rest;
        }
        if s == before {
            break;
        }
    }

    let without_links = replace_links(s);
    // Drop the emphasis/inline-code characters; keep their textual content.
    without_links
        .chars()
        .filter(|c| !matches!(c, '*' | '_' | '`' | '~'))
        .collect::<String>()
}

/// If `s` starts with an ordered-list marker like `12. `, return the rest.
fn strip_ordered_list_marker(s: &str) -> Option<&str> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &s[digits.len()..];
    rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))
}

/// Replace Markdown links `[text](url)` and images `![alt](url)` with just the
/// visible `text`/`alt`. Simple left-to-right scan; leaves malformed syntax
/// untouched.
fn replace_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip a leading `!` for images so `![alt](url)` → `alt`.
        let img_start = bytes[i] == b'!' && i + 1 < bytes.len() && bytes[i + 1] == b'[';
        let bracket = if img_start { i + 1 } else { i };
        if bytes[bracket] == b'['
            && let Some((text, next)) = parse_link(&s[bracket..])
        {
            out.push_str(&text);
            i = bracket + next;
            continue;
        }
        // Not a link; copy this char (respecting UTF-8 boundaries).
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Parse `[text](url)` starting at `s[0] == '['`. Returns the link text and the
/// byte offset just past the closing `)`. `None` if it isn't a well-formed link.
fn parse_link(s: &str) -> Option<(String, usize)> {
    let close_br = s.find(']')?;
    let rest = &s[close_br + 1..];
    if !rest.starts_with('(') {
        return None;
    }
    let close_paren = rest.find(')')?;
    let text = s[1..close_br].to_string();
    Some((text, close_br + 1 + close_paren + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> SpokenMarkers<'static> {
        SpokenMarkers {
            code: "Code is shown on screen.",
            table: "A table is shown on screen.",
        }
    }

    #[test]
    fn plain_prose_passes_through() {
        assert_eq!(
            to_spoken("Hello there. How are you?", &m()),
            "Hello there. How are you?"
        );
    }

    #[test]
    fn strips_emphasis_and_inline_code() {
        assert_eq!(
            to_spoken("Use **bold** and `code` and _italic_.", &m()),
            "Use bold and code and italic."
        );
    }

    #[test]
    fn headings_and_bullets_become_prose() {
        assert_eq!(
            to_spoken("## Title\n- first\n- second", &m()),
            "Title first second"
        );
    }

    #[test]
    fn ordered_list_markers_stripped() {
        assert_eq!(
            to_spoken("1. do this\n2. then that", &m()),
            "do this then that"
        );
    }

    #[test]
    fn links_reduced_to_text() {
        assert_eq!(
            to_spoken("See [the docs](https://example.com/x) now.", &m()),
            "See the docs now."
        );
        assert_eq!(to_spoken("![a diagram](/img.png)", &m()), "a diagram");
    }

    #[test]
    fn code_block_becomes_marker() {
        let input = "Here you go:\n```rust\nfn main() {}\n```\nDone.";
        assert_eq!(
            to_spoken(input, &m()),
            "Here you go: Code is shown on screen. Done."
        );
    }

    #[test]
    fn table_becomes_single_marker() {
        let input = "Results:\n| a | b |\n|---|---|\n| 1 | 2 |\nThat's it.";
        assert_eq!(
            to_spoken(input, &m()),
            "Results: A table is shown on screen. That's it."
        );
    }

    #[test]
    fn multibyte_text_is_safe() {
        // Link scan must not split a UTF-8 sequence.
        assert_eq!(
            to_spoken("Grüße [hier](http://x) — schön", &m()),
            "Grüße hier — schön"
        );
    }

    #[test]
    fn empty_input_is_empty() {
        assert_eq!(to_spoken("", &m()), "");
        assert_eq!(
            to_spoken("```\ncode\n```", &m()),
            "Code is shown on screen."
        );
    }
}
