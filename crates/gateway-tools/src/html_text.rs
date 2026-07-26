// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! HTML → plain-text/markdown extraction for tool results.
//!
//! `fetch_url` used to hand the model a page's raw markup. For a typical
//! documentation or news page that's 200–800 KB of `<script>` blobs, inline
//! CSS, SVG sprites, consent banners and tracking pixels wrapped around a
//! few KB of prose — the single largest token sink in the tool surface, and
//! it makes the model work to find the text at all.
//!
//! This module reduces markup to the parts that carry meaning for a reader:
//! headings, paragraphs, list items, links, tables and code. Everything
//! inside `<script>` / `<style>` / `<noscript>` / `<svg>` / `<template>` is
//! dropped wholesale, comments and doctypes disappear, character references
//! are decoded, and horizontal whitespace collapses.
//!
//! **Hand-rolled on purpose.** Per `docs/dependencies.md` every crate needs
//! a justification and stdlib is preferred; a general-purpose HTML parser
//! (html5ever + a DOM + a serializer) is a large tree for a job this narrow.
//! Same call as the base64 codec in `chat_attachments` and the JSON Patch
//! implementation in `tools::json_patch`.
//!
//! **Not a sanitizer.** The output is text for a language model, never HTML
//! for a browser. Don't reuse it as an XSS defence — chat rendering has its
//! own escaping path (`markdown` with raw HTML rejected).
//!
//! Deliberately *not* a readability/boilerplate classifier: we don't try to
//! guess which `<div>` holds "the article". Guessing wrong silently deletes
//! content the model was asked about, which is far worse than carrying some
//! nav text. We only drop what is guaranteed to be non-prose.

/// Elements whose entire subtree is discarded. `<svg>` and `<template>` can
/// nest, so the skip is depth-counted per element name.
const DROP_ELEMENTS: &[&str] = &["script", "style", "noscript", "svg", "template"];

/// Elements that start a new block. Rendered as a blank line so the model
/// sees paragraph structure rather than one run-on line.
const BLOCK_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "details",
    "dialog",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "header",
    "hgroup",
    "main",
    "nav",
    "ol",
    "p",
    "section",
    "summary",
    "table",
    "tbody",
    "tfoot",
    "thead",
    "title",
    "ul",
];

/// A parsed start/end tag.
struct Tag<'a> {
    /// Lowercased element name.
    name: String,
    /// Raw attribute text between the name and the closing `>`.
    attrs: &'a str,
    closing: bool,
    self_closing: bool,
    /// Byte index just past the tag's `>`.
    end: usize,
}

/// Convert `html` to plain text with light markdown structure.
///
/// Never fails: malformed markup degrades to "some tags were treated as
/// text" rather than an error. A tool result is not the place to reject a
/// page because a `<div>` was left open.
pub fn extract(html: &str) -> String {
    let bytes = html.as_bytes();
    // Prose is a small fraction of markup; a rough guess beats growing from
    // zero on a multi-hundred-KB page.
    let mut out = String::with_capacity(html.len() / 8 + 64);
    // Open `<a href>` elements: where their text began in `out`, plus the
    // href to append on close. A stack because anchors can (invalidly but
    // commonly) nest.
    let mut anchors: Vec<(usize, String)> = Vec::new();
    // >0 while inside `<pre>`: whitespace is preserved verbatim.
    let mut pre_depth: usize = 0;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            // Text up to the next tag. `<` is ASCII, so the slice always
            // ends on a char boundary.
            let end = match html[i..].find('<') {
                Some(off) => i + off,
                None => bytes.len(),
            };
            push_text(&mut out, &html[i..end], pre_depth > 0);
            i = end;
            continue;
        }

        // `<!-- comment -->`
        if html[i..].starts_with("<!--") {
            i = match html[i + 4..].find("-->") {
                Some(off) => i + 4 + off + 3,
                // Unterminated comment: the rest of the document is comment.
                None => bytes.len(),
            };
            continue;
        }
        // `<!doctype …>`, `<![CDATA[…]]>`, `<?xml …?>`
        if matches!(bytes.get(i + 1), Some(b'!') | Some(b'?')) {
            i = skip_past_gt(html, i + 1);
            continue;
        }

        let Some(tag) = parse_tag(html, i) else {
            // A `<` that isn't a tag (`a < b`). Emit it as text.
            push_text(&mut out, "<", pre_depth > 0);
            i += 1;
            continue;
        };

        // Whole-subtree drops come first: nothing inside is ever text.
        if !tag.closing && !tag.self_closing && DROP_ELEMENTS.contains(&tag.name.as_str()) {
            i = skip_element(html, tag.end, &tag.name);
            continue;
        }

        match tag.name.as_str() {
            "br" => out.push('\n'),
            "hr" => {
                ensure_blank_line(&mut out);
                out.push_str("---");
                ensure_blank_line(&mut out);
            }
            "pre" => {
                if tag.closing {
                    pre_depth = pre_depth.saturating_sub(1);
                    if pre_depth == 0 {
                        ensure_newline(&mut out);
                        out.push_str("```");
                        ensure_blank_line(&mut out);
                    }
                } else {
                    if pre_depth == 0 {
                        ensure_blank_line(&mut out);
                        out.push_str("```");
                        out.push('\n');
                    }
                    pre_depth += 1;
                }
            }
            // Inline code gets backticks, but not inside `<pre>` — that
            // block is already fenced and `<pre><code>` is the common pair.
            "code" | "tt" | "kbd" | "samp" if pre_depth == 0 => out.push('`'),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                ensure_blank_line(&mut out);
                if !tag.closing {
                    let level = tag.name.as_bytes()[1] - b'0';
                    for _ in 0..level {
                        out.push('#');
                    }
                    out.push(' ');
                }
            }
            "li" if !tag.closing => {
                ensure_newline(&mut out);
                out.push_str("- ");
            }
            "dt" | "dd" => ensure_newline(&mut out),
            "tr" => ensure_newline(&mut out),
            "td" | "th" if tag.closing => out.push_str(" | "),
            "a" => {
                if tag.closing {
                    close_anchor(&mut out, &mut anchors);
                } else if let Some(href) = link_target(tag.attrs) {
                    anchors.push((out.len(), href));
                }
            }
            name if BLOCK_ELEMENTS.contains(&name) => ensure_blank_line(&mut out),
            // Inline or unknown element: structurally transparent, its text
            // still flows through.
            _ => {}
        }

        i = tag.end;
    }

    // Unclosed anchors leave their text in place, unwrapped — better than
    // emitting a dangling `[`.
    out.trim().to_string()
}

/// True for content types this extractor should handle.
pub fn is_html(content_type: &str) -> bool {
    let ct = content_type.trim().to_ascii_lowercase();
    ct == "text/html" || ct == "application/xhtml+xml"
}

/// Wrap the text an `<a>` accumulated into `[text](href)`. No-op when the
/// anchor turned out to be empty (icon-only links) — an empty `[](url)`
/// carries nothing but tokens.
fn close_anchor(out: &mut String, anchors: &mut Vec<(usize, String)>) {
    let Some((start, href)) = anchors.pop() else {
        return;
    };
    if start > out.len() {
        // Shouldn't happen — `out` only grows — but never panic on a slice.
        return;
    }
    if out[start..].trim().is_empty() {
        return;
    }
    out.insert(start, '[');
    out.push_str("](");
    out.push_str(&href);
    out.push(')');
}

/// Extract a usable `href` from an anchor's attribute text. Fragment-only,
/// empty, and script URLs are dropped: they'd cost tokens and lead nowhere.
fn link_target(attrs: &str) -> Option<String> {
    let href = attr_value(attrs, "href")?;
    let trimmed = href.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("javascript:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("data:")
    {
        return None;
    }
    // A URL containing markdown link punctuation would break the `[](…)`
    // shape; those are rare enough to just skip the wrapping.
    if trimmed.contains(')') || trimmed.contains('(') {
        return None;
    }
    Some(decode_entities(trimmed))
}

/// Find `name="value"` / `name='value'` / `name=value` in raw attribute
/// text. Case-insensitive on the name, as HTML requires.
fn attr_value(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(off) = lower[from..].find(name) {
        let at = from + off;
        // Must be preceded by whitespace or start-of-string, so `data-href`
        // doesn't match `href`.
        let boundary_ok = at == 0
            || lower.as_bytes()[at - 1].is_ascii_whitespace()
            || lower.as_bytes()[at - 1] == b'/';
        let after = at + name.len();
        if boundary_ok {
            let rest = lower[after..].trim_start();
            if let Some(eq) = rest.strip_prefix('=') {
                // Recover the byte offset of the value in the original
                // string: lowercasing ASCII preserves byte lengths, and a
                // non-ASCII attribute name can't match `name` anyway.
                let value_start = attrs.len() - eq.len();
                return Some(read_attr_value(&attrs[value_start..]));
            }
        }
        from = at + name.len();
    }
    None
}

/// Read one attribute value starting at (optional) whitespace before it.
fn read_attr_value(s: &str) -> String {
    let s = s.trim_start();
    let mut chars = s.chars();
    match chars.next() {
        Some(q @ ('"' | '\'')) => {
            let rest = &s[q.len_utf8()..];
            match rest.find(q) {
                Some(end) => rest[..end].to_string(),
                // Unterminated quote: take what's there.
                None => rest.to_string(),
            }
        }
        Some(_) => s
            .split(|c: char| c.is_whitespace() || c == '>')
            .next()
            .unwrap_or("")
            .to_string(),
        None => String::new(),
    }
}

/// Parse the tag starting at `start` (which must index a `<`). Returns
/// `None` when this isn't a tag at all, so the caller can treat the `<` as
/// literal text.
fn parse_tag(html: &str, start: usize) -> Option<Tag<'_>> {
    let bytes = html.as_bytes();
    let mut p = start + 1;
    let closing = bytes.get(p) == Some(&b'/');
    if closing {
        p += 1;
    }
    let name_start = p;
    while p < bytes.len() && (bytes[p].is_ascii_alphanumeric() || bytes[p] == b'-') {
        p += 1;
    }
    if p == name_start {
        return None;
    }
    let name = html[name_start..p].to_ascii_lowercase();

    // Scan to the closing `>`, honouring quoted attribute values so a `>`
    // inside `title="a > b"` doesn't end the tag early.
    let attrs_start = p;
    let mut quote: Option<u8> = None;
    while p < bytes.len() {
        let c = bytes[p];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' => quote = Some(c),
            None if c == b'>' => break,
            None => {}
        }
        p += 1;
    }
    let attrs = &html[attrs_start..p.min(bytes.len())];
    Some(Tag {
        name,
        attrs,
        closing,
        self_closing: attrs.trim_end().ends_with('/'),
        end: (p + 1).min(bytes.len()),
    })
}

/// Skip everything up to and including the matching `</name>`, counting
/// nested `<name>` opens. `from` is the index just past the opening tag.
fn skip_element(html: &str, from: usize, name: &str) -> usize {
    let mut depth = 1usize;
    let mut i = from;
    while i < html.len() {
        let Some(off) = html[i..].find('<') else {
            return html.len();
        };
        let at = i + off;
        match parse_tag(html, at) {
            Some(tag) if tag.name == name => {
                if tag.closing {
                    depth -= 1;
                    if depth == 0 {
                        return tag.end;
                    }
                } else if !tag.self_closing {
                    depth += 1;
                }
                i = tag.end;
            }
            Some(tag) => i = tag.end,
            None => i = at + 1,
        }
    }
    // Unterminated element (a `<script>` that never closes): the remainder
    // of the document is markup, not prose.
    html.len()
}

/// Byte index just past the next `>` at or after `from`.
fn skip_past_gt(html: &str, from: usize) -> usize {
    match html[from..].find('>') {
        Some(off) => from + off + 1,
        None => html.len(),
    }
}

/// Append text, decoding character references. Outside `<pre>` any run of
/// whitespace collapses to a single space and never starts a line.
fn push_text(out: &mut String, text: &str, preserve: bool) {
    let mut rest = text;
    while !rest.is_empty() {
        let (chunk, next) = match rest.find('&') {
            Some(0) => match decode_one_entity(rest) {
                Some((decoded, consumed)) => {
                    push_chars(out, &decoded, preserve);
                    rest = &rest[consumed..];
                    continue;
                }
                // A bare `&` that isn't a reference.
                None => ("&", &rest[1..]),
            },
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, ""),
        };
        push_chars(out, chunk, preserve);
        rest = next;
    }
}

fn push_chars(out: &mut String, s: &str, preserve: bool) {
    if preserve {
        out.push_str(s);
        return;
    }
    for ch in s.chars() {
        if ch.is_whitespace() {
            // Collapse runs, and don't indent a fresh line.
            if !out.is_empty() && !out.ends_with(' ') && !out.ends_with('\n') {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
    }
}

/// Decode every character reference in `s`. Used for attribute values,
/// where the whitespace rules of [`push_text`] don't apply.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        match rest.find('&') {
            Some(0) => match decode_one_entity(rest) {
                Some((decoded, consumed)) => {
                    out.push_str(&decoded);
                    rest = &rest[consumed..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            },
            Some(at) => {
                out.push_str(&rest[..at]);
                rest = &rest[at..];
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

/// Named references worth carrying. Deliberately short: the long tail of
/// HTML5's 2000+ names costs binary size for glyphs a model will never miss,
/// and anything unrecognised survives as its literal `&name;` text.
const NAMED_ENTITIES: &[(&str, &str)] = &[
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("apos", "'"),
    ("nbsp", " "),
    ("ensp", " "),
    ("emsp", " "),
    ("thinsp", " "),
    ("shy", ""),
    ("zwnj", ""),
    ("zwj", ""),
    ("ndash", "–"),
    ("mdash", "—"),
    ("hellip", "…"),
    ("lsquo", "‘"),
    ("rsquo", "’"),
    ("ldquo", "“"),
    ("rdquo", "”"),
    ("sbquo", "‚"),
    ("bdquo", "„"),
    ("laquo", "«"),
    ("raquo", "»"),
    ("lsaquo", "‹"),
    ("rsaquo", "›"),
    ("bull", "•"),
    ("middot", "·"),
    ("dagger", "†"),
    ("copy", "©"),
    ("reg", "®"),
    ("trade", "™"),
    ("deg", "°"),
    ("plusmn", "±"),
    ("times", "×"),
    ("divide", "÷"),
    ("frac12", "½"),
    ("frac14", "¼"),
    ("frac34", "¾"),
    ("sup2", "²"),
    ("sup3", "³"),
    ("micro", "µ"),
    ("euro", "€"),
    ("pound", "£"),
    ("yen", "¥"),
    ("cent", "¢"),
    ("curren", "¤"),
    ("sect", "§"),
    ("para", "¶"),
    ("permil", "‰"),
    ("prime", "′"),
    ("Prime", "″"),
    ("larr", "←"),
    ("rarr", "→"),
    ("uarr", "↑"),
    ("darr", "↓"),
    ("harr", "↔"),
    ("ne", "≠"),
    ("le", "≤"),
    ("ge", "≥"),
    ("asymp", "≈"),
    ("infin", "∞"),
    ("radic", "√"),
    ("sum", "∑"),
    ("prod", "∏"),
    ("minus", "−"),
    ("alpha", "α"),
    ("beta", "β"),
    ("gamma", "γ"),
    ("delta", "δ"),
    ("pi", "π"),
    ("sigma", "σ"),
    ("mu", "μ"),
    ("Omega", "Ω"),
    ("auml", "ä"),
    ("ouml", "ö"),
    ("uuml", "ü"),
    ("Auml", "Ä"),
    ("Ouml", "Ö"),
    ("Uuml", "Ü"),
    ("szlig", "ß"),
    ("eacute", "é"),
    ("egrave", "è"),
    ("agrave", "à"),
    ("ccedil", "ç"),
    ("ntilde", "ñ"),
];

/// Longest possible reference we'll consider, so a stray `&` in prose
/// doesn't trigger a scan across the whole document.
const MAX_ENTITY_LEN: usize = 32;

/// Decode the reference at the start of `s` (which begins with `&`).
/// Returns the replacement text and how many bytes it consumed, or `None`
/// when this isn't a well-formed reference.
fn decode_one_entity(s: &str) -> Option<(String, usize)> {
    debug_assert!(s.starts_with('&'));
    let limit = s.len().min(MAX_ENTITY_LEN);
    let semi = s[..limit].find(';')?;
    let body = &s[1..semi];
    if body.is_empty() {
        return None;
    }
    let consumed = semi + 1;

    if let Some(num) = body.strip_prefix('#') {
        let cp = if let Some(hex) = num.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse::<u32>().ok()?
        };
        // Reject surrogates / out-of-range: `from_u32` already does, and a
        // malformed numeric reference should survive as literal text.
        let ch = char::from_u32(cp)?;
        return Some((ch.to_string(), consumed));
    }

    // Named references are case-sensitive in HTML (`&Auml;` ≠ `&auml;`).
    NAMED_ENTITIES
        .iter()
        .find(|(name, _)| *name == body)
        .map(|(_, replacement)| (replacement.to_string(), consumed))
}

/// Ensure `out` ends with a newline (trailing spaces trimmed first).
fn ensure_newline(out: &mut String) {
    if out.is_empty() {
        return;
    }
    while out.ends_with(' ') || out.ends_with('\t') {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Ensure `out` ends with exactly one blank line. Caps consecutive newlines
/// at two, which is what keeps the output free of long vertical gaps without
/// a post-processing pass.
fn ensure_blank_line(out: &mut String) {
    ensure_newline(out);
    if out.is_empty() || out.ends_with("\n\n") {
        return;
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_script_style_and_svg_subtrees() {
        let html = "<html><head><style>body{color:red}</style>\
                    <script>var x = 1 < 2; alert('hi')</script></head>\
                    <body><svg><path d=\"M0 0\"/></svg><p>Real prose.</p></body></html>";
        let out = extract(html);
        assert_eq!(out, "Real prose.");
        assert!(!out.contains("color"), "{out}");
        assert!(!out.contains("alert"), "{out}");
        assert!(!out.contains("M0 0"), "{out}");
    }

    #[test]
    fn nested_svg_does_not_end_the_skip_early() {
        // A depth-1 skip would resume inside the outer <svg> and leak the
        // path data as text.
        let html = "<svg><svg><title>icon</title></svg><path d=\"leak\"/></svg><p>Text.</p>";
        assert_eq!(extract(html), "Text.");
    }

    #[test]
    fn unterminated_script_swallows_the_rest() {
        // Better to lose the tail than to emit a page of JavaScript.
        let html = "<p>Before.</p><script>var a = 1;";
        assert_eq!(extract(html), "Before.");
    }

    #[test]
    fn headings_become_markdown() {
        let html = "<h1>Title</h1><h3>Sub</h3><p>Body.</p>";
        assert_eq!(extract(html), "# Title\n\n### Sub\n\nBody.");
    }

    #[test]
    fn list_items_become_bullets() {
        let html = "<ul><li>one</li><li>two</li></ul>";
        assert_eq!(extract(html), "- one\n- two");
    }

    #[test]
    fn links_become_markdown_links() {
        let html = r#"<p>See <a href="https://example.com/docs">the docs</a>.</p>"#;
        assert_eq!(extract(html), "See [the docs](https://example.com/docs).");
    }

    #[test]
    fn empty_and_fragment_links_are_not_wrapped() {
        let html = r##"<a href="#top"><span></span></a><a href="/x"></a><p>Text.</p>"##;
        assert_eq!(extract(html), "Text.");
    }

    #[test]
    fn script_urls_are_dropped_but_text_survives() {
        let html = r#"<a href="javascript:steal()">click me</a>"#;
        assert_eq!(extract(html), "click me");
    }

    #[test]
    fn data_href_does_not_match_href() {
        // Attribute-name boundary check: a `data-href` must not be picked up
        // as the link target.
        let html = r#"<a data-href="/wrong" href="/right">x</a>"#;
        assert_eq!(extract(html), "[x](/right)");
    }

    #[test]
    fn pre_block_preserves_whitespace_and_gets_fenced() {
        let html = "<p>Example:</p><pre><code>fn main() {\n    let x = 1;\n}</code></pre>";
        let out = extract(html);
        assert!(out.contains("```\nfn main() {\n    let x = 1;\n}"), "{out}");
    }

    #[test]
    fn inline_code_gets_backticks() {
        assert_eq!(
            extract("<p>Call <code>run()</code> now.</p>"),
            "Call `run()` now."
        );
    }

    #[test]
    fn collapses_whitespace_outside_pre() {
        let html = "<p>a   \n\t  b</p>";
        assert_eq!(extract(html), "a b");
    }

    #[test]
    fn comments_and_doctype_disappear() {
        let html = "<!DOCTYPE html><!-- tracking pixel --><p>Hi</p><!-- end -->";
        assert_eq!(extract(html), "Hi");
    }

    #[test]
    fn unterminated_comment_does_not_panic() {
        assert_eq!(extract("<p>a</p><!-- never closed"), "a");
    }

    #[test]
    fn decodes_named_and_numeric_entities() {
        let html = "<p>caf&eacute; &amp; bar &#8212; 5&nbsp;&euro; &#x2713;</p>";
        assert_eq!(extract(html), "café & bar — 5 € ✓");
    }

    #[test]
    fn unknown_entity_survives_as_text() {
        assert_eq!(extract("<p>&notareal; x</p>"), "&notareal; x");
    }

    #[test]
    fn bare_ampersand_is_kept() {
        assert_eq!(extract("<p>Tom & Jerry</p>"), "Tom & Jerry");
    }

    #[test]
    fn stray_less_than_is_treated_as_text() {
        assert_eq!(extract("<p>if a < b then</p>"), "if a < b then");
    }

    #[test]
    fn quoted_gt_inside_attribute_does_not_end_the_tag() {
        let html = r#"<p title="a > b">Text.</p>"#;
        assert_eq!(extract(html), "Text.");
    }

    #[test]
    fn table_rows_are_separated() {
        let html = "<table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>";
        let out = extract(html);
        assert!(out.contains("a | b"), "{out}");
        assert!(out.contains("c | d"), "{out}");
        assert!(out.lines().count() >= 2, "{out}");
    }

    #[test]
    fn br_and_hr_produce_breaks() {
        let out = extract("<p>a<br>b</p><hr><p>c</p>");
        assert_eq!(out, "a\nb\n\n---\n\nc");
    }

    #[test]
    fn no_runs_of_more_than_two_newlines() {
        let html = "<div><section><article><p>x</p></article></section></div>\
                    <div><p>y</p></div>";
        let out = extract(html);
        assert!(!out.contains("\n\n\n"), "{out:?}");
        assert_eq!(out, "x\n\ny");
    }

    #[test]
    fn realistic_page_shrinks_dramatically() {
        // The actual point of the module: a page whose markup dwarfs its
        // prose must come back mostly prose.
        let mut html = String::from("<!DOCTYPE html><html><head><title>Docs</title>");
        html.push_str("<style>");
        html.push_str(&".cls-1{fill:#fff}".repeat(2000));
        html.push_str("</style><script>");
        html.push_str(&"function f(){return 1}".repeat(2000));
        html.push_str("</script></head><body><nav><a href=\"/a\">Home</a></nav>");
        html.push_str("<main><h1>Install</h1><p>Run the installer.</p></main></body></html>");

        let out = extract(&html);
        assert!(out.contains("# Install"), "{out}");
        assert!(out.contains("Run the installer."), "{out}");
        assert!(!out.contains("fill:#fff"), "{out}");
        assert!(!out.contains("function f"), "{out}");
        // ~80 KB of markup down to well under 1 KB.
        assert!(
            out.len() < 200,
            "expected tiny output, got {} bytes",
            out.len()
        );
    }

    #[test]
    fn empty_and_text_only_inputs() {
        assert_eq!(extract(""), "");
        assert_eq!(extract("   \n  "), "");
        assert_eq!(extract("just text"), "just text");
    }

    #[test]
    fn is_html_matches_only_html_content_types() {
        assert!(is_html("text/html"));
        assert!(is_html("TEXT/HTML"));
        assert!(is_html("application/xhtml+xml"));
        assert!(!is_html("application/json"));
        assert!(!is_html("text/plain"));
        assert!(!is_html("text/markdown"));
    }

    #[test]
    fn multibyte_text_is_not_split_mid_char() {
        // Byte-index scanning must never slice inside a multi-byte char.
        let html = "<p>日本語のテキスト</p><p>Ünïcödé</p>";
        assert_eq!(extract(html), "日本語のテキスト\n\nÜnïcödé");
    }
}
