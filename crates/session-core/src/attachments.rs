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
    // fixed. The trailing ` link="…"` is optional — older markers
    // (and every non-typst upload) omit it, so it must not be
    // required or those stop parsing.
    Regex::new(
        r#"\[gw-attachment file="([^"]*)" mime="([^"]*)" url="([^"]*)" size=(\d+)(?: link="([^"]*)")?\]"#,
    )
    .expect("attachment regex compiles")
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAttachment {
    pub filename: String,
    pub mime: String,
    pub url: String,
    pub size: u64,
    /// Optional click-through target distinct from `url`. Set when
    /// the attachment's bytes are a *preview* of a different file —
    /// e.g. a typst render's PNG (shown via `url`) whose click should
    /// open the PDF (`link`). `None` for ordinary attachments, where
    /// the click-through is `url` itself.
    pub link: Option<String>,
}

impl ParsedAttachment {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

/// Build the canonical marker line (no separate click-through link).
pub fn marker_line(filename: &str, mime: &str, url: &str, size: u64) -> String {
    marker_line_linked(filename, mime, url, size, None)
}

/// Build a marker line whose preview (`url`) clicks through to a
/// different `link` target. When `link` is `None` this is byte-for-byte
/// the plain [`marker_line`] output, so unlinked attachments keep their
/// exact prior marker text.
pub fn marker_line_linked(
    filename: &str,
    mime: &str,
    url: &str,
    size: u64,
    link: Option<&str>,
) -> String {
    let filename = filename.replace('"', "");
    let mime = mime.replace('"', "");
    let url = url.replace('"', "");
    let mut out =
        format!("[gw-attachment file=\"{filename}\" mime=\"{mime}\" url=\"{url}\" size={size}");
    if let Some(link) = link {
        let link = link.replace('"', "");
        out.push_str(&format!(" link=\"{link}\""));
    }
    out.push(']');
    out
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
                link: caps.get(5).map(|m| m.as_str().to_string()),
            })
        })
        .collect()
}

/// Whether a marker sitting in `turn_id`'s row may be trusted.
///
/// Every marker the gateway writes points at the turn that carries
/// it: uploads land under the user turn, tool output under the
/// assistant turn (`marker_line(turn_id, …)` everywhere). A marker
/// whose proxy URL names a *different* turn was therefore not written
/// by us — it's a model that copied a marker line out of replayed
/// history and passed it off as a fresh attachment. Those point at
/// turns that may be long deleted (the proxy answers `no such turn`)
/// and at bytes this turn never produced, so every consumer drops
/// them. Non-proxy URLs (legacy presigned links, external URLs) carry
/// no turn id and are left alone.
pub fn marker_url_owned_by(url: &str, turn_id: &str) -> bool {
    proxy_url_turn_id(url).is_none_or(|t| t == turn_id)
}

/// [`parse_markers`], keeping only the markers `turn_id` actually
/// owns — see [`marker_url_owned_by`].
pub fn parse_markers_for_turn(text: &str, turn_id: &str) -> Vec<ParsedAttachment> {
    parse_markers(text)
        .into_iter()
        .filter(|a| marker_url_owned_by(&a.url, turn_id))
        .collect()
}

/// Walk the marker regex over `text` and yield segments alternating
/// between unparsed prose and a parsed attachment. Used by the chat
/// renderer to splice attachment HTML into the user bubble while
/// keeping the surrounding text intact.
pub fn split_markers(text: &str) -> Vec<Segment<'_>> {
    split_markers_owned(text, None)
}

/// [`split_markers`] restricted to the markers `turn_id` owns: a
/// marker whose proxy URL names another turn yields no segment at all
/// (not even prose) — see [`marker_url_owned_by`]. The renderer uses
/// this so a model-forged marker line neither draws a chip pointing at
/// bytes that don't exist nor leaks its raw text into the bubble.
pub fn split_markers_for_turn<'a>(text: &'a str, turn_id: &str) -> Vec<Segment<'a>> {
    split_markers_owned(text, Some(turn_id))
}

fn split_markers_owned<'a>(text: &'a str, owner: Option<&str>) -> Vec<Segment<'a>> {
    let mut out: Vec<Segment<'a>> = Vec::new();
    let mut cursor = 0;
    for caps in MARKER_RE.captures_iter(text) {
        let whole = caps.get(0).unwrap();
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
            link: caps.get(5).map(|m| m.as_str().to_string()),
        };
        let keep = owner.is_none_or(|t| marker_url_owned_by(&att.url, t));
        if whole.start() > cursor {
            let lead = trim_marker_lead(&text[cursor..whole.start()]);
            // A dropped marker must not leave an empty prose segment
            // behind — the bubble would render a blank block where the
            // forged chip used to be.
            if keep || !lead.is_empty() {
                out.push(Segment::Text(lead));
            }
        }
        if keep {
            out.push(Segment::Attachment(att));
        }
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
            link: caps.get(5).map(|m| m.as_str().to_string()),
        };
        let url = new_url_for(&att).unwrap_or_else(|| att.url.clone());
        // A preview's `link` is the same flavour of URL as `url` (a
        // gateway proxy URL), so it gets the identical rewrite — else a
        // forked deck's PNG would still click through to the source
        // turn's PDF (a 404 after the fork remaps turn ids).
        let link = att.link.as_ref().map(|l| {
            let probe = ParsedAttachment {
                url: l.clone(),
                ..att.clone()
            };
            new_url_for(&probe).unwrap_or_else(|| l.clone())
        });
        out.push_str(&marker_line_linked(
            &att.filename,
            &att.mime,
            &url,
            att.size,
            link.as_deref(),
        ));
        cursor = whole.end();
    }
    out.push_str(&text[cursor..]);
    out
}

/// Rebuild `text` with every attachment marker for which `remove`
/// returns `true` dropped (along with the single newline that followed
/// it, so removing a chip doesn't leave a widening run of blank lines),
/// while leaving prose and the surviving markers byte-for-byte intact.
/// Used to supersede an earlier same-turn render's chips when a
/// re-render replaces them, so a chat turn shows only the latest
/// deliverable rather than every intermediate variant.
pub fn remove_markers_where<F>(text: &str, mut remove: F) -> String
where
    F: FnMut(&ParsedAttachment) -> bool,
{
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for caps in MARKER_RE.captures_iter(text) {
        let whole = caps.get(0).unwrap();
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
            link: caps.get(5).map(|m| m.as_str().to_string()),
        };
        // Prose preceding this marker is always preserved.
        out.push_str(&text[cursor..whole.start()]);
        cursor = whole.end();
        if remove(&att) {
            // Swallow one trailing newline so the gap left behind doesn't
            // grow each time a chip is superseded.
            if text[cursor..].starts_with('\n') {
                cursor += 1;
            }
        } else {
            out.push_str(whole.as_str());
        }
    }
    out.push_str(&text[cursor..]);
    out
}

/// Pull the `<turn_id>` segment out of a gateway attachment proxy URL
/// of the form `/chat/attachment/<turn_id>/<filename>`. Returns `None`
/// for any other URL shape (legacy presigned links, external URLs) so
/// callers leave those untouched. The filename segment may be
/// percent-encoded; this only looks at the turn-id segment, which is a
/// UUID and never encoded.
pub fn proxy_url_turn_id(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("/chat/attachment/")?;
    let slash = rest.find('/')?;
    Some(&rest[..slash])
}

/// Rewrite every attachment marker's proxy URL so its `<turn_id>`
/// segment is replaced by `map[<turn_id>]`, leaving the (possibly
/// percent-encoded) filename segment and every other field untouched.
/// Markers whose turn id isn't in `map`, or whose URL isn't a proxy
/// URL, are left as-is. Used by the fork path so a copied conversation's
/// bubbles point at the new owner's turn-scoped attachment keys.
pub fn remap_attachment_turn_ids(
    text: &str,
    map: &std::collections::HashMap<String, String>,
) -> String {
    rewrite_marker_urls(text, |att| {
        let rest = att.url.strip_prefix("/chat/attachment/")?;
        let slash = rest.find('/')?;
        let old = &rest[..slash];
        let new = map.get(old)?;
        let file_seg = &rest[slash + 1..];
        Some(format!("/chat/attachment/{new}/{file_seg}"))
    })
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
        let url = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let size = caps.get(4).map(|m| m.as_str()).unwrap_or("0");
        // A marker this turn doesn't own is a forgery (see
        // `marker_url_owned_by`) — replay nothing for it, so the model
        // isn't handed an id that resolves to no bytes.
        if marker_url_owned_by(url, turn_id) {
            out.push_str(&replay_stub(turn_id, filename, mime, size));
        }
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

/// Matches a [`replay_stub`] line — including one a model wrote itself.
///
/// Deliberately looser than the emitted format (any field tail, optional
/// parenthetical) because what we are detecting is *imitation*, and an
/// imitation is approximate by nature.
static REPLAY_STUB_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)[ \t]*\[attached file="[^"\n]*"[^\]\n]*\](?:[ \t]*\(call the fetch_attachment tool[^)\n]*\))?"#,
    )
    .expect("replay stub regex compiles")
});

/// Whether `text` contains something shaped like a replay stub.
pub fn has_replay_stub(text: &str) -> bool {
    REPLAY_STUB_RE.is_match(text)
}

/// Remove replay stubs from *assistant* text before it is rendered.
///
/// The stub exists only in the upstream payload — [`strip_markers_for_replay`]
/// synthesises it per attachment on the way to the model, and it is never
/// persisted. So a stub sitting in stored assistant content is always something
/// the model typed after seeing its own history, and it is the second
/// generation of one specific failure: models used to copy the
/// `[gw-attachment …]` marker (which `split_markers_for_turn` now refuses on
/// ownership grounds), and having been given the stub instead, they copy that.
///
/// A copied stub renders as literal text that reads, to a user, exactly like an
/// attachment the interface failed to draw — so they hunt for a download that
/// was never uploaded, and every "it's not there" is answered with another
/// forgery. Dropping it is the same call already made for markdown images in
/// `render::md`: the model has no legitimate way to attach a file through prose,
/// so prose that looks like an attachment is never worth rendering.
pub fn strip_replay_stubs(text: &str) -> std::borrow::Cow<'_, str> {
    REPLAY_STUB_RE.replace_all(text, "")
}

/// Replace replay stubs in assistant text with a flat contradiction, for the
/// history replayed *upstream*.
///
/// The render-side [`strip_replay_stubs`] protects the user; this protects the
/// conversation. Left alone, a forged stub is indistinguishable — to the model
/// reading its own prior turn — from a real one, so it believes it already
/// delivered the file and keeps insisting the interface is broken. Handing it
/// back a denial is the only signal in the loop that says otherwise.
pub fn contradict_replay_stubs(text: &str) -> std::borrow::Cow<'_, str> {
    REPLAY_STUB_RE.replace_all(
        text,
        "[no file was attached here: the text above was typed into the reply and was \
         NOT produced by a tool, so the user received nothing. Attach a file only by \
         calling `upload_attachment` or `offer_download`, and never by writing an \
         `[attached file=…]` line yourself.]",
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
                link: None,
            }]
        );
    }

    #[test]
    fn linked_marker_round_trips_through_parse() {
        // A preview marker carries a click-through `link` distinct from
        // its `url`; both survive a parse.
        let line = marker_line_linked(
            "deck.png",
            "image/png",
            "/chat/attachment/t-1/deck.png",
            99,
            Some("/chat/attachment/t-1/deck.pdf"),
        );
        let parsed = parse_markers(&line);
        assert_eq!(
            parsed,
            vec![ParsedAttachment {
                filename: "deck.png".into(),
                mime: "image/png".into(),
                url: "/chat/attachment/t-1/deck.png".into(),
                size: 99,
                link: Some("/chat/attachment/t-1/deck.pdf".into()),
            }]
        );
    }

    #[test]
    fn remove_markers_where_drops_only_matching_chips() {
        // Two renders' chips in one turn (the second supersedes the
        // first): stripping the `-` (first) PDF+PNG leaves the prose and
        // the latest render's chips intact, with no widening blank gap.
        let pdf1 = marker_line(
            "deck.pdf",
            "application/pdf",
            "/chat/attachment/t/deck.pdf",
            10,
        );
        let png1 = marker_line_linked(
            "deck.png",
            "image/png",
            "/chat/attachment/t/deck.png",
            5,
            Some("/chat/attachment/t/deck.pdf"),
        );
        let pdf2 = marker_line(
            "deck-2.pdf",
            "application/pdf",
            "/chat/attachment/t/deck-2.pdf",
            12,
        );
        let text = format!("Here you go:\n\n{pdf1}\n{png1}\n\nFixed it:\n\n{pdf2}\n\n");
        let drop = ["deck.pdf", "deck.png"];
        let out = remove_markers_where(&text, |a| drop.contains(&a.filename.as_str()));
        // The superseded chips are gone; the latest one and all prose stay.
        assert!(!out.contains("deck.pdf\""), "old pdf chip survived: {out}");
        assert!(!out.contains("deck.png\""), "old png chip survived: {out}");
        assert!(out.contains("deck-2.pdf"), "latest chip dropped: {out}");
        assert!(out.contains("Here you go:"));
        assert!(out.contains("Fixed it:"));
        // Removal must not pile up blank lines where the chips were.
        assert!(
            !out.contains("\n\n\n\n"),
            "blank-line run accumulated: {out:?}"
        );
    }

    #[test]
    fn remove_markers_where_keeps_everything_when_predicate_never_matches() {
        let line = marker_line("a.pdf", "application/pdf", "/chat/attachment/t/a.pdf", 1);
        let text = format!("prose\n\n{line}\n\n");
        assert_eq!(remove_markers_where(&text, |_| false), text);
    }

    #[test]
    fn unlinked_marker_text_is_byte_identical_to_legacy() {
        // marker_line must still emit the exact pre-`link` text so
        // existing persisted rows and other call sites are unaffected.
        assert_eq!(
            marker_line("x.png", "image/png", "https://e/x.png", 7),
            r#"[gw-attachment file="x.png" mime="image/png" url="https://e/x.png" size=7]"#
        );
    }

    #[test]
    fn remap_turn_ids_rewrites_link_segment_too() {
        use std::collections::HashMap;
        // A forked deck's PNG preview must point its click-through at the
        // NEW turn's PDF, not the source turn's (which 404s post-fork).
        let m = marker_line_linked(
            "deck.png",
            "image/png",
            "/chat/attachment/old/deck.png",
            5,
            Some("/chat/attachment/old/deck.pdf"),
        );
        let map = HashMap::from([("old".to_string(), "new".to_string())]);
        let out = remap_attachment_turn_ids(&m, &map);
        assert!(
            out.contains(r#"url="/chat/attachment/new/deck.png""#),
            "{out}"
        );
        assert!(
            out.contains(r#"link="/chat/attachment/new/deck.pdf""#),
            "{out}"
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

    /// A marker line the model copied out of replayed history: it
    /// names an *older* turn, so this turn never produced those bytes.
    /// Every consumer must ignore it — otherwise the bubble shows a
    /// chip whose download answers `no such turn` once that turn has
    /// been retried away.
    #[test]
    fn split_for_turn_drops_markers_owned_by_another_turn() {
        let mine = marker_line(
            "mine.pdf",
            "application/pdf",
            "/chat/attachment/t-1/mine.pdf",
            3,
        );
        let forged = marker_line(
            "stolen.pdf",
            "application/pdf",
            "/chat/attachment/t-0/stolen.pdf",
            9,
        );
        let input = format!("here you go\n\n{forged}\n\n{mine}\n\ndone");
        let segs = split_markers_for_turn(&input, "t-1");
        let files: Vec<&str> = segs
            .iter()
            .filter_map(|s| match s {
                Segment::Attachment(a) => Some(a.filename.as_str()),
                Segment::Text(_) => None,
            })
            .collect();
        assert_eq!(files, ["mine.pdf"], "forged marker must not render");
        // …and its raw text must not leak into the prose either.
        assert!(
            !segs
                .iter()
                .any(|s| matches!(s, Segment::Text(t) if t.contains("gw-attachment"))),
            "forged marker text leaked into prose: {segs:?}"
        );
        // Unfiltered parsing still sees both — the rule is turn-scoped,
        // not a change to the marker format.
        assert_eq!(parse_markers(&input).len(), 2);
        assert_eq!(parse_markers_for_turn(&input, "t-1").len(), 1);
    }

    #[test]
    fn non_proxy_marker_urls_are_always_owned() {
        // Legacy presigned / external URLs carry no turn id; they must
        // keep rendering wherever they sit.
        let line = marker_line("x.png", "image/png", "https://e.invalid/x.png", 1);
        assert_eq!(parse_markers_for_turn(&line, "whatever").len(), 1);
        assert!(marker_url_owned_by("https://e.invalid/x.png", "t-1"));
    }

    /// The exact text a model produced after seeing replay stubs in its own
    /// history: it wrote one itself, complete with a plausible id, having
    /// called no tool at all.
    fn forged_stub() -> String {
        "[attached file=\"croit-cowork-context.zip\" mime=\"application/zip\" size=40709 \
         id=\"4be8a013-2dd4-41f5-9ea7-9a210b2f4ab1/croit-cowork-context.zip\"] \
         (call the fetch_attachment tool with this id to read its contents)"
            .to_string()
    }

    #[test]
    fn a_real_replay_stub_is_recognised_as_the_forgeable_shape() {
        // Whatever `strip_markers_for_replay` emits is by definition what a
        // model can copy, so the detector is pinned to the emitter rather than
        // to a hand-written sample that could drift away from it.
        let marker = marker_line("d.pdf", "application/pdf", "/chat/attachment/t-1/d.pdf", 4);
        let replayed = strip_markers_for_replay(&marker, "t-1");
        assert!(has_replay_stub(&replayed), "{replayed}");
        assert!(has_replay_stub(&forged_stub()));
        assert!(!has_replay_stub("here is the file you asked for"));
    }

    #[test]
    fn forged_stubs_are_stripped_from_rendered_assistant_text() {
        // Rendered as prose, a copied stub reads like an attachment the UI
        // failed to draw — so the user hunts for a download that was never
        // uploaded. It must leave no trace in the bubble.
        let text = format!("Above is the bundle:\n\n{}\n\nUnzip it.", forged_stub());
        let out = strip_replay_stubs(&text);
        assert!(!out.contains("attached file="), "{out}");
        assert!(!out.contains("4be8a013"), "{out}");
        assert!(!out.contains("fetch_attachment"), "{out}");
        assert!(out.contains("Above is the bundle:"), "{out}");
        assert!(out.contains("Unzip it."), "{out}");
    }

    #[test]
    fn forged_stubs_are_contradicted_in_replayed_history() {
        // The model must not be able to read its own forgery back as evidence
        // that it already delivered the file.
        let forged = forged_stub();
        let out = contradict_replay_stubs(&forged);
        assert!(
            !out.contains("4be8a013"),
            "the fake id must not survive: {out}"
        );
        assert!(out.contains("no file was attached here"), "{out}");
        assert!(out.contains("upload_attachment"), "{out}");
    }

    #[test]
    fn text_without_a_stub_is_returned_untouched() {
        // Every assistant bubble goes through this on render; the common case
        // must not allocate or alter a byte.
        let text = "Just prose, with a [markdown link](https://example.com/a).";
        assert!(matches!(
            strip_replay_stubs(text),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(strip_replay_stubs(text), text);
    }

    #[test]
    fn strip_for_replay_omits_markers_owned_by_another_turn() {
        // The model must never be handed an id for bytes this turn
        // doesn't own — that id resolves to nothing.
        let forged = marker_line("d.pdf", "application/pdf", "/chat/attachment/t-0/d.pdf", 4);
        let out = strip_markers_for_replay(&format!("text\n{forged}\ntail"), "t-1");
        assert!(!out.contains("gw-attachment"), "{out}");
        assert!(!out.contains("t-1/d.pdf"), "{out}");
        assert!(!out.contains("fetch_attachment"), "{out}");
        assert!(out.contains("text") && out.contains("tail"), "{out}");
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
    fn proxy_url_turn_id_extracts_turn_segment() {
        assert_eq!(
            proxy_url_turn_id("/chat/attachment/t-1/chart.png"),
            Some("t-1")
        );
        // Percent-encoded filename segment is irrelevant to the turn id.
        assert_eq!(
            proxy_url_turn_id("/chat/attachment/t-1/my%20file.png"),
            Some("t-1")
        );
        // Non-proxy URLs (legacy presigned, external) yield None.
        assert_eq!(proxy_url_turn_id("https://bucket/x.png"), None);
        assert_eq!(proxy_url_turn_id("/chat/attachment/t-1"), None);
    }

    #[test]
    fn remap_turn_ids_rewrites_only_mapped_markers() {
        use std::collections::HashMap;
        let a = marker_line("a.png", "image/png", "/chat/attachment/old-1/a.png", 1);
        let b = marker_line("b.png", "image/png", "/chat/attachment/old-2/b.png", 2);
        // Only old-1 is in the map; old-2's marker must survive untouched.
        let map = HashMap::from([("old-1".to_string(), "new-1".to_string())]);
        let out = remap_attachment_turn_ids(&format!("{a}\n{b}"), &map);
        assert!(
            out.contains(r#"url="/chat/attachment/new-1/a.png""#),
            "{out}"
        );
        assert!(
            out.contains(r#"url="/chat/attachment/old-2/b.png""#),
            "{out}"
        );
    }

    #[test]
    fn remap_turn_ids_preserves_encoded_filename_segment() {
        use std::collections::HashMap;
        let m = marker_line(
            "my file.png",
            "image/png",
            "/chat/attachment/old/my%20file.png",
            3,
        );
        let map = HashMap::from([("old".to_string(), "new".to_string())]);
        let out = remap_attachment_turn_ids(&m, &map);
        assert!(
            out.contains(r#"url="/chat/attachment/new/my%20file.png""#),
            "{out}"
        );
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
