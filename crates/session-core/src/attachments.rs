// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Shared attachment-marker parsing for the chat surface.
//!
//! User messages with attachments carry one `[gw-attachment …]`
//! marker line per file in `chat_turns.user_text`. The marker
//! captures filename + mime + URL + byte size; both the gateway
//! (when building OpenAI's content parts) and the renderer (when
//! drawing the user bubble) walk the same regex over the stored
//! text.
//!
//! Lives in `session-core` rather than the gateway because the
//! chat renderer here needs it to inline images / file chips, and
//! `session-core` is the dep both binaries already share.

use std::sync::LazyLock;

use regex::Regex;

static MARKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Non-greedy quoted strings + a numeric size. The
    // `gw-attachment` prefix gates the match; field order is
    // fixed for now.
    Regex::new(r#"\[gw-attachment file="([^"]*)" mime="([^"]*)" url="([^"]*)" size=(\d+)\]"#)
        .expect("attachment regex compiles")
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAttachment {
    pub filename: String,
    pub mime: String,
    pub url: String,
    pub size: u64,
}

impl ParsedAttachment {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

/// Build the canonical marker line.
pub fn marker_line(filename: &str, mime: &str, url: &str, size: u64) -> String {
    let filename = filename.replace('"', "");
    let mime = mime.replace('"', "");
    let url = url.replace('"', "");
    format!("[gw-attachment file=\"{filename}\" mime=\"{mime}\" url=\"{url}\" size={size}]")
}

/// Filenames already claimed by attachment markers in `text`. Used
/// by the dedup helpers below + by callers that need to combine a
/// text-side set with an in-flight reservation set (concurrent tool
/// calls in one turn) before picking a free name.
pub fn existing_filenames(text: &str) -> std::collections::HashSet<String> {
    parse_markers(text)
        .into_iter()
        .map(|a| a.filename)
        .collect()
}

/// Pick a filename that doesn't collide with any name in `used`.
/// Returns `desired` if free, else appends `-2`, `-3`, … before the
/// extension. Pure — same suffix algorithm as [`dedupe_filename`],
/// exposed against a precomputed set so callers can fold in extra
/// "reserved but not yet committed" names atomically.
pub fn dedupe_filename_against(used: &std::collections::HashSet<String>, desired: &str) -> String {
    if !used.contains(desired) {
        return desired.to_string();
    }
    let (stem, ext) = split_extension(desired);
    let mut n: u32 = 2;
    loop {
        let candidate = match ext {
            "" => format!("{stem}-{n}"),
            ext => format!("{stem}-{n}.{ext}"),
        };
        if !used.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Trio-aware sibling of [`dedupe_filename_against`]: pick a stem
/// such that `{stem}.{ext}` is free for *every* `ext` in `exts`. Used
/// by tools that write a group of related files (the typst render
/// writes `.pdf` + `.png` + `.typ` together; they must share a stem
/// or the model sees a mismatched trio like `foo-2.pdf` /
/// `foo-3.png`).
pub fn dedupe_basename_against(
    used: &std::collections::HashSet<String>,
    base: &str,
    exts: &[&str],
) -> String {
    let any_taken = |stem: &str| {
        exts.iter()
            .any(|ext| used.contains(&format!("{stem}.{ext}")))
    };
    if !any_taken(base) {
        return base.to_string();
    }
    let mut n: u32 = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !any_taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Pick a filename that doesn't collide with any attachment marker
/// already in `existing`. If `desired` is unused, returns it as-is;
/// otherwise inserts `-2`, `-3`, … before the extension until a free
/// name is found. Pure over the inputs — same-turn dedup, no I/O.
///
/// Solves the *sequential* case (tool calls in different rounds, or
/// the same tool retrying after a write has already landed). For
/// *concurrent* tool calls in one round neither has appended yet, so
/// callers must combine [`existing_filenames`] with an external
/// reservation set and call [`dedupe_filename_against`] under a lock.
pub fn dedupe_filename(existing: &str, desired: &str) -> String {
    dedupe_filename_against(&existing_filenames(existing), desired)
}

/// Split a filename into `(stem, ext)` for suffixing. A leading dot
/// (dotfile) stays with the stem; only a non-leading rightmost `.`
/// counts as an extension separator.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

/// Pull every `[gw-attachment …]` marker out of a user-message
/// text. Returns them in document order.
pub fn parse_markers(text: &str) -> Vec<ParsedAttachment> {
    MARKER_RE
        .captures_iter(text)
        .filter_map(|caps| {
            let size = caps.get(4)?.as_str().parse::<u64>().ok()?;
            Some(ParsedAttachment {
                filename: caps.get(1)?.as_str().to_string(),
                mime: caps.get(2)?.as_str().to_string(),
                url: caps.get(3)?.as_str().to_string(),
                size,
            })
        })
        .collect()
}

/// Walk the marker regex over `text` and yield segments alternating
/// between unparsed prose and a parsed attachment. Used by the chat
/// renderer to splice attachment HTML into the user bubble while
/// keeping the surrounding text intact.
pub fn split_markers(text: &str) -> Vec<Segment<'_>> {
    let mut out: Vec<Segment<'_>> = Vec::new();
    let mut cursor = 0;
    for caps in MARKER_RE.captures_iter(text) {
        let whole = caps.get(0).unwrap();
        if whole.start() > cursor {
            out.push(Segment::Text(trim_marker_lead(
                &text[cursor..whole.start()],
            )));
        }
        let size = caps
            .get(4)
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(0);
        out.push(Segment::Attachment(ParsedAttachment {
            filename: caps
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
            mime: caps
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
            url: caps
                .get(3)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
            size,
        }));
        cursor = whole.end();
        // For text/* attachments the marker is followed by an
        // inlined fenced block carrying the bytes — we don't want
        // to render those bytes in the user bubble (the model gets
        // them on send; the user already knows what they typed).
        // Skip the fence if present.
        let tail = &text[cursor..];
        if let Some(fenced_end) = skip_fence(tail) {
            cursor += fenced_end;
        }
    }
    if cursor < text.len() {
        let trailing = trim_marker_lead(&text[cursor..]);
        if !trailing.is_empty() {
            out.push(Segment::Text(trailing));
        }
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum Segment<'a> {
    Text(&'a str),
    Attachment(ParsedAttachment),
}

/// Drop leading/trailing newlines around a prose segment so the
/// rendered bubble doesn't carry the blank lines we insert between
/// marker entries when building user_text. Whitespace inside the
/// prose stays put — only the marker-boundary newlines go.
fn trim_marker_lead(s: &str) -> &str {
    s.trim_matches(|c: char| c == '\n' || c == '\r' || c == ' ' || c == '\t')
}

fn skip_fence(tail: &str) -> Option<usize> {
    let trimmed_start = tail.find(|c: char| !c.is_whitespace())?;
    let after_ws = &tail[trimmed_start..];
    if !after_ws.starts_with("```") {
        return None;
    }
    let mut idx = trimmed_start + 3;
    let nl = tail[idx..].find('\n')?;
    idx += nl + 1;
    while idx < tail.len() {
        let rest = &tail[idx..];
        let close = rest.find("```")?;
        let abs = idx + close;
        let at_line_start = abs == 0 || tail.as_bytes()[abs - 1] == b'\n';
        if at_line_start {
            let mut end = abs + 3;
            if tail[end..].starts_with('\n') {
                end += 1;
            }
            return Some(end);
        }
        idx = abs + 3;
    }
    None
}

/// Walk markers in `text` and rebuild it with each marker's `url`
/// field replaced by whatever `new_url_for(att)` returns. Callers
/// that return `None` leave the original URL untouched. Used by
/// the chat-page render path to splice freshly-presigned S3 URLs
/// over the upload-time URLs stored in `chat_turns.user_text` so
/// the bubble's `<img src>` never serves a stale signature.
pub fn rewrite_marker_urls<F>(text: &str, mut new_url_for: F) -> String
where
    F: FnMut(&ParsedAttachment) -> Option<String>,
{
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for caps in MARKER_RE.captures_iter(text) {
        let whole = caps.get(0).unwrap();
        out.push_str(&text[cursor..whole.start()]);
        let size = caps
            .get(4)
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(0);
        let att = ParsedAttachment {
            filename: caps
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
            mime: caps
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
            url: caps
                .get(3)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
            size,
        };
        let url = new_url_for(&att).unwrap_or_else(|| att.url.clone());
        out.push_str(&marker_line(&att.filename, &att.mime, &url, att.size));
        cursor = whole.end();
    }
    out.push_str(&text[cursor..]);
    out
}

/// Replace each `[gw-attachment …]` marker (and any immediately
/// following fenced block left over from older persisted rows that
/// inlined text content) with a stub naming the file and an opaque
/// `id` the model can pass to the `fetch_attachment` tool to read
/// the bytes on demand. Used by the gateway's driver on *every*
/// user-role message in the upstream payload — current turn and
/// past turns alike — so the presigned URL never leaks to the LLM
/// provider, TTL expiry stops mattering, and the model uses tokens
/// only on the attachments it actually needs.
///
/// The id format is intentionally identical to the S3 object key
/// (sans the configurable `key_prefix`) so the gateway's tool can
/// resolve it server-side via the same `chat_attachments` helpers
/// the upload path uses.
pub fn strip_markers_for_replay(text: &str, turn_id: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for caps in MARKER_RE.captures_iter(text) {
        let whole = caps.get(0).unwrap();
        out.push_str(&text[cursor..whole.start()]);
        let filename = caps.get(1).map(|m| m.as_str()).unwrap_or("attachment");
        let mime = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let size = caps.get(4).map(|m| m.as_str()).unwrap_or("0");
        out.push_str(&replay_stub(turn_id, filename, mime, size));
        cursor = whole.end();
        let tail = &text[cursor..];
        if let Some(fenced_end) = skip_fence(tail) {
            cursor += fenced_end;
        }
    }
    out.push_str(&text[cursor..]);
    out
}

/// The single-line stub `strip_markers_for_replay` emits per
/// attachment. Factored out so the stub format lives in one place
/// rather than embedded in the strip loop.
fn replay_stub(turn_id: &str, filename: &str, mime: &str, size: &str) -> String {
    let id = format!("{turn_id}/{filename}");
    format!(
        "[attached file=\"{filename}\" mime=\"{mime}\" size={size} id=\"{id}\"] \
         (call the fetch_attachment tool with this id to read its contents)"
    )
}

/// Files whose bytes get inlined alongside the marker as a fenced
/// code block. Mirrors the gateway's receive-side logic so callers
/// in any crate can ask "is this attachment going to be rendered
/// as text-content or as a binary reference?"
pub fn is_inline_text(mime: &str, filename: &str) -> bool {
    if mime.starts_with("text/") {
        return true;
    }
    matches!(
        mime,
        "application/json"
            | "application/xml"
            | "application/x-yaml"
            | "application/yaml"
            | "application/csv"
            | "application/javascript"
            | "application/typescript"
            | "application/toml"
            | "application/sql"
    ) || matches!(
        std::path::Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(""),
        "csv"
            | "tsv"
            | "json"
            | "jsonl"
            | "ndjson"
            | "yaml"
            | "yml"
            | "toml"
            | "xml"
            | "md"
            | "markdown"
            | "rst"
            | "txt"
            | "log"
            | "sql"
            | "sh"
            | "bash"
            | "zsh"
            | "py"
            | "rs"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "go"
            | "java"
            | "kt"
            | "swift"
            | "rb"
            | "php"
            | "c"
            | "h"
            | "cpp"
            | "cc"
            | "hpp"
            | "css"
            | "html"
            | "htm"
            | "ini"
            | "cfg"
            | "conf"
    )
}

/// True when an attachment should be treated as a PDF — the
/// `fetch_attachment` tool routes these through its dedicated
/// text-extraction / page-rendering tiers instead of the generic
/// binary "ask the user to re-upload" stub. Mirrors `is_inline_text`'s
/// mime-first, extension-fallback shape so a PDF served as
/// `application/octet-stream` (some buckets do this) is still caught.
pub fn is_pdf(mime: &str, filename: &str) -> bool {
    if mime.eq_ignore_ascii_case("application/pdf")
        || mime.eq_ignore_ascii_case("application/x-pdf")
    {
        return true;
    }
    std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_pdf_matches_mime_and_extension() {
        assert!(is_pdf("application/pdf", "sponsor.pdf"));
        assert!(is_pdf("APPLICATION/PDF", "sponsor.pdf"));
        assert!(is_pdf("application/x-pdf", "sponsor.pdf"));
        // Buckets that serve a generic octet-stream still get caught by ext.
        assert!(is_pdf("application/octet-stream", "sponsor.PDF"));
        assert!(!is_pdf("application/octet-stream", "data.bin"));
        assert!(!is_pdf("text/csv", "data.csv"));
        // A PDF is never inline-text — the two classifiers must not overlap.
        assert!(!is_inline_text("application/pdf", "sponsor.pdf"));
    }

    #[test]
    fn parse_returns_attachment_struct() {
        let line = marker_line("x.png", "image/png", "https://e/x.png", 4321);
        let parsed = parse_markers(&format!("hi\n{line}\nbye"));
        assert_eq!(
            parsed,
            vec![ParsedAttachment {
                filename: "x.png".into(),
                mime: "image/png".into(),
                url: "https://e/x.png".into(),
                size: 4321,
            }]
        );
    }

    #[test]
    fn split_yields_text_attachment_text() {
        let line = marker_line("x.png", "image/png", "https://e/x.png", 1);
        let input = format!("hello\n\n{line}\n\nworld");
        let segs = split_markers(&input);
        assert_eq!(segs.len(), 3);
        assert!(matches!(segs[0], Segment::Text("hello")));
        assert!(matches!(segs[1], Segment::Attachment(_)));
        assert!(matches!(segs[2], Segment::Text("world")));
    }

    #[test]
    fn split_drops_inlined_fence_for_text_attachments() {
        let line = marker_line("d.csv", "text/csv", "https://e/d.csv", 4);
        let input = format!("look at this\n{line}\n```csv\na,b\n1,2\n```\nthoughts?");
        let segs = split_markers(&input);
        assert_eq!(segs.len(), 3);
        assert!(matches!(segs[0], Segment::Text("look at this")));
        match &segs[1] {
            Segment::Attachment(a) => assert_eq!(a.filename, "d.csv"),
            _ => panic!("expected attachment"),
        }
        assert!(matches!(&segs[2], Segment::Text(s) if s.contains("thoughts?")));
    }

    #[test]
    fn strip_for_replay_collapses_marker_and_fence() {
        let line = marker_line("d.csv", "text/csv", "https://e/d.csv", 4);
        let input = format!("{line}\n```\nx\n```\ntail");
        let out = strip_markers_for_replay(&input, "t-9");
        assert!(out.contains("file=\"d.csv\""));
        assert!(out.contains("mime=\"text/csv\""));
        assert!(out.contains("id=\"t-9/d.csv\""));
        assert!(out.contains("fetch_attachment"));
        assert!(!out.contains("```"));
        assert!(out.ends_with("tail"));
    }

    #[test]
    fn strip_for_replay_handles_multiple_markers() {
        let a = marker_line("a.csv", "text/csv", "https://e/a", 1);
        let b = marker_line("b.png", "image/png", "https://e/b", 2);
        let out = strip_markers_for_replay(&format!("{a}\n{b}"), "t-7");
        assert!(out.contains("id=\"t-7/a.csv\""));
        assert!(out.contains("id=\"t-7/b.png\""));
        // Each marker produced its own stub; no marker survived.
        assert!(!out.contains("gw-attachment"));
    }

    #[test]
    fn dedupe_returns_desired_when_unused() {
        let line = marker_line("other.png", "image/png", "https://e/other.png", 1);
        assert_eq!(dedupe_filename(&line, "chart.png"), "chart.png");
    }

    #[test]
    fn dedupe_suffixes_before_extension_on_collision() {
        let a = marker_line("chart.png", "image/png", "https://e/a", 1);
        assert_eq!(dedupe_filename(&a, "chart.png"), "chart-2.png");
    }

    #[test]
    fn dedupe_walks_past_existing_suffixes() {
        let a = marker_line("chart.png", "image/png", "https://e/a", 1);
        let b = marker_line("chart-2.png", "image/png", "https://e/b", 1);
        let c = marker_line("chart-3.png", "image/png", "https://e/c", 1);
        let text = format!("{a}\n{b}\n{c}");
        assert_eq!(dedupe_filename(&text, "chart.png"), "chart-4.png");
    }

    #[test]
    fn dedupe_handles_no_extension() {
        let a = marker_line("notes", "text/plain", "https://e/a", 1);
        assert_eq!(dedupe_filename(&a, "notes"), "notes-2");
    }

    #[test]
    fn dedupe_basename_returns_base_when_all_slots_free() {
        let used = existing_filenames("");
        assert_eq!(
            dedupe_basename_against(&used, "chart", &["pdf", "png"]),
            "chart"
        );
    }

    #[test]
    fn dedupe_basename_skips_when_any_extension_collides() {
        let marker = marker_line("chart.png", "image/png", "/e/chart.png", 1);
        let used = existing_filenames(&marker);
        assert_eq!(
            dedupe_basename_against(&used, "chart", &["pdf", "png", "typ"]),
            "chart-2"
        );
    }

    #[test]
    fn dedupe_basename_keeps_trio_in_sync_across_renders() {
        // Two prior renders → next must be `chart-3` for the WHOLE
        // trio even when only chart-2.png is recorded.
        let m1 = marker_line("chart.pdf", "application/pdf", "/e/chart.pdf", 1);
        let m2 = marker_line("chart-2.png", "image/png", "/e/chart-2.png", 1);
        let used = existing_filenames(&format!("{m1}\n{m2}"));
        assert_eq!(
            dedupe_basename_against(&used, "chart", &["pdf", "png", "typ"]),
            "chart-3"
        );
    }

    #[test]
    fn dedupe_treats_leading_dot_as_part_of_stem() {
        // `.env` is a dotfile, not a "" stem with `env` extension.
        let a = marker_line(".env", "text/plain", "https://e/a", 1);
        assert_eq!(dedupe_filename(&a, ".env"), ".env-2");
    }

    #[test]
    fn strip_for_replay_drops_presigned_url() {
        // The signed S3 URL must not leak into replay context — the
        // whole point of the opaque-id design is that the LLM provider
        // never sees a credentialed URL for past turns.
        let line = marker_line(
            "x.png",
            "image/png",
            "https://bucket.example/x.png?X-Amz-Signature=deadbeef",
            42,
        );
        let out = strip_markers_for_replay(&line, "t-1");
        assert!(!out.contains("X-Amz-Signature"));
        assert!(!out.contains("https://"));
    }
}
