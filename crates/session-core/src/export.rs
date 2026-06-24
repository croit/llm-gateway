// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Conversation export — turn a persisted chat into a self-contained
//! document the user can download and forward.
//!
//! Two formats share one pass over the turns:
//! - [`to_markdown`] emits a clean Markdown transcript (the turn
//!   content is already Markdown, so this mostly frames it with
//!   per-turn headings and rewrites attachment markers into links).
//! - [`to_typst`] emits a full `.typ` source the gateway compiles to
//!   PDF. The heavy lifting is [`md_to_typst`], a walker over the
//!   `markdown` crate's mdast that maps CommonMark/GFM to Typst markup.
//!
//! Scope (by product decision): user + assistant message text plus
//! attachments rendered as links. Reasoning blocks and tool calls are
//! intentionally left out — they're debugging material, not part of the
//! shareable result.
//!
//! Lives in `session-core` (next to the renderers and the DB types) so
//! it stays decoupled from the gateway; the only gateway-specific input
//! is the public base URL, passed in via [`ExportOpts`].

use markdown::mdast::{List, Node};
use markdown::{ParseOptions, to_mdast};

use crate::attachments::{Segment, split_markers};
use crate::db::{Session, TurnRole, TurnWithTools};

/// Knobs the exporters need that aren't carried by the turns
/// themselves.
pub struct ExportOpts<'a> {
    /// Public URL the gateway is reachable at (e.g.
    /// `https://gateway.example.com`). Used to turn relative attachment
    /// URLs (`/chat/attachment/…`) into absolute links that still
    /// resolve once the document leaves the browser. Trailing slash
    /// optional.
    pub base_url: &'a str,
}

// ---------------------------------------------------------------------------
// Markdown export

/// Render the whole conversation as a Markdown document: an H1 title, a
/// muted export date, then one `##`-headed section per turn. Turn
/// content is already Markdown so it passes through verbatim; only the
/// `[gw-attachment …]` markers are rewritten into ordinary links.
pub fn to_markdown(session: &Session, turns: &[TurnWithTools], opts: &ExportOpts<'_>) -> String {
    let title = session_title(session);
    let mut out = String::new();
    out.push_str(&format!("# {title}\n\n"));
    out.push_str(&format!("*Exported {}*\n\n", export_date(session)));

    for tw in turns {
        let turn = &tw.turn;
        let raw = turn_text(turn);
        let body = clean_to_markdown(raw, opts.base_url);
        if body.trim().is_empty() {
            continue;
        }
        out.push_str("---\n\n");
        out.push_str(&format!(
            "## {}\n\n",
            turn_label(&turn.role, turn.model.as_deref())
        ));
        out.push_str(body.trim());
        out.push_str("\n\n");
    }
    out
}

/// Walk a turn's stored text, keeping prose Markdown as-is and turning
/// each attachment marker into a Markdown link with an absolute URL.
fn clean_to_markdown(raw: &str, base_url: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for seg in split_markers(raw) {
        match seg {
            Segment::Text(t) => {
                let t = t.trim();
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
            Segment::Attachment(a) => {
                let url = absolutize(&a.url, base_url);
                parts.push(format!("[{}]({})", a.filename, url));
            }
        }
    }
    parts.join("\n\n")
}

// ---------------------------------------------------------------------------
// Typst export

/// Render the whole conversation as a standalone Typst source. The
/// gateway compiles the returned string with `typst compile` (see
/// `server::typst::compile_source`).
pub fn to_typst(session: &Session, turns: &[TurnWithTools], opts: &ExportOpts<'_>) -> String {
    let title = session_title(session);
    let mut s = String::new();

    // Document preamble. No explicit font — Typst's bundled defaults
    // (Libertinus Serif + DejaVu Sans Mono for raw) render everywhere,
    // so a stock install needs no `--font-path`.
    s.push_str(&format!("#set document(title: {})\n", typ_quoted(&title)));
    s.push_str("#set page(margin: 2cm, numbering: \"1\")\n");
    s.push_str("#set text(size: 11pt)\n");
    s.push_str("#set par(justify: true, leading: 0.65em)\n");
    s.push_str("#show link: set text(fill: rgb(\"#2563eb\"))\n\n");

    // Title block.
    s.push_str("#align(center)[\n");
    s.push_str(&format!(
        "  #text(size: 20pt, weight: \"bold\")[{}]\\\n",
        typ_str(&title)
    ));
    s.push_str(&format!(
        "  #text(size: 9pt, fill: luma(120))[Exported {}]\n",
        typ_str(&export_date(session))
    ));
    s.push_str("]\n");
    s.push_str("#v(0.5em)\n#line(length: 100%, stroke: 0.5pt + luma(200))\n#v(1em)\n\n");

    for tw in turns {
        let turn = &tw.turn;
        let raw = turn_text(turn);
        let body = clean_to_typst(raw, opts.base_url);
        if body.trim().is_empty() {
            continue;
        }
        let label = turn_label(&turn.role, turn.model.as_deref());
        // User turns get a tinted box; assistant turns stay on the page
        // background so the eye separates question from answer the way
        // the chat bubbles do.
        let open = match turn.role {
            TurnRole::User => {
                "#block(width: 100%, inset: (x: 12pt, y: 10pt), radius: 4pt, \
                 fill: luma(244), below: 1em)[\n"
            }
            TurnRole::Assistant => "#block(width: 100%, below: 1.4em)[\n",
        };
        s.push_str(open);
        s.push_str(&format!(
            "  #text(size: 8pt, weight: \"bold\", fill: luma(110))[{}]\n\n",
            typ_str(&label.to_uppercase())
        ));
        s.push_str(&body);
        s.push_str("]\n\n");
    }
    s
}

/// Walk a turn's stored text, mapping prose through [`md_to_typst`] and
/// each attachment marker into a Typst link line.
fn clean_to_typst(raw: &str, base_url: &str) -> String {
    let mut out = String::new();
    for seg in split_markers(raw) {
        match seg {
            Segment::Text(t) => {
                let t = t.trim();
                if !t.is_empty() {
                    out.push_str(&md_to_typst(t));
                }
            }
            Segment::Attachment(a) => {
                let url = absolutize(&a.url, base_url);
                out.push_str(&format!(
                    "#block(below: 0.6em)[#link({})[{}]]\n\n",
                    typ_quoted(&url),
                    typ_str(&format!("📎 {}", a.filename)),
                ));
            }
        }
    }
    out
}

/// Convert a Markdown fragment to Typst markup via the `markdown`
/// crate's mdast. On a parse error (shouldn't happen for content that
/// already round-tripped through the renderer) we fall back to emitting
/// the text verbatim so nothing is lost.
pub fn md_to_typst(md: &str) -> String {
    match to_mdast(md, &ParseOptions::gfm()) {
        Ok(node) => render_block(&node),
        Err(_) => format!("{}\n\n", typ_str(md)),
    }
}

/// Render a block-level node (or a container of them). Unknown nodes
/// fall back to rendering their children as a paragraph, so a node type
/// we don't special-case never silently drops its text.
fn render_block(node: &Node) -> String {
    match node {
        Node::Root(r) => render_blocks(&r.children),
        Node::Paragraph(p) => format!("{}\n\n", render_inline(&p.children)),
        Node::Heading(h) => {
            let level = h.depth.clamp(1, 6) as usize;
            format!("{} {}\n\n", "=".repeat(level), render_inline(&h.children))
        }
        Node::Code(c) => format!("{}\n\n", typ_raw(&c.value, c.lang.as_deref(), true)),
        Node::List(l) => render_list(l, 0),
        Node::Blockquote(b) => format!("#quote(block: true)[{}]\n\n", render_blocks(&b.children)),
        Node::ThematicBreak(_) => "#line(length: 100%, stroke: 0.5pt + luma(200))\n\n".to_string(),
        Node::Table(t) => render_table(t),
        // Anything else block-ish: descend into children if it has any,
        // otherwise drop (positions, definitions, etc. carry no display
        // text we want in the export).
        other => match other.children() {
            Some(children) => render_blocks(children),
            None => String::new(),
        },
    }
}

/// Join a sequence of block nodes.
fn render_blocks(nodes: &[Node]) -> String {
    nodes.iter().map(render_block).collect()
}

/// Render a (possibly nested) list. `indent` is the nesting depth; each
/// level adds two spaces so Typst nests the markup correctly.
fn render_list(list: &List, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    // `-` for bullets, `+` for Typst's auto-numbered enum (explicit
    // start numbers aren't expressible in markup — rare in chat).
    let marker = if list.ordered { "+ " } else { "- " };
    let mut out = String::new();
    for item in &list.children {
        let Node::ListItem(li) = item else { continue };
        let mut first = true;
        for child in &li.children {
            match child {
                Node::Paragraph(p) => {
                    let inline = render_inline(&p.children);
                    if first {
                        out.push_str(&format!("{pad}{marker}{inline}\n"));
                        first = false;
                    } else {
                        out.push_str(&format!("{pad}  {inline}\n"));
                    }
                }
                Node::List(sub) => out.push_str(&render_list(sub, indent + 1)),
                other => {
                    // Code block / quote inside an item: indent every
                    // line under the marker.
                    for line in render_block(other).lines() {
                        out.push_str(&format!("{pad}  {line}\n"));
                    }
                }
            }
        }
        if first {
            out.push_str(&format!("{pad}{marker}\n"));
        }
    }
    out.push('\n');
    out
}

/// Render a GFM table as a Typst `#table`. The first row is treated as
/// a header and bolded.
fn render_table(table: &markdown::mdast::Table) -> String {
    let cols = table
        .children
        .first()
        .and_then(|r| r.children().map(|c| c.len()))
        .unwrap_or(1)
        .max(1);
    let mut out = format!("#table(\n  columns: {cols},\n");
    for (ri, row) in table.children.iter().enumerate() {
        let Some(cells) = row.children() else {
            continue;
        };
        for cell in cells {
            let inline = cell
                .children()
                .map(|c| render_inline(c))
                .unwrap_or_default();
            if ri == 0 {
                out.push_str(&format!("  [#strong[{inline}]],\n"));
            } else {
                out.push_str(&format!("  [{inline}],\n"));
            }
        }
    }
    out.push_str(")\n\n");
    out
}

/// Render a sequence of inline nodes into one Typst markup string.
fn render_inline(nodes: &[Node]) -> String {
    nodes.iter().map(render_inline_node).collect()
}

fn render_inline_node(node: &Node) -> String {
    match node {
        Node::Text(t) => typ_str(&t.value),
        Node::Strong(s) => format!("#strong[{}]", render_inline(&s.children)),
        Node::Emphasis(e) => format!("#emph[{}]", render_inline(&e.children)),
        Node::Delete(d) => format!("#strike[{}]", render_inline(&d.children)),
        Node::InlineCode(c) => typ_raw(&c.value, None, false),
        Node::Break(_) => "#linebreak()".to_string(),
        Node::Link(l) => {
            let text = render_inline(&l.children);
            let text = if text.is_empty() {
                typ_str(&l.url)
            } else {
                text
            };
            format!("#link({})[{}]", typ_quoted(&l.url), text)
        }
        Node::Image(i) => {
            // Images aren't embedded (decision: attachments as links).
            // Degrade to a link so the URL is still reachable.
            let text = if i.alt.is_empty() {
                typ_str(&i.url)
            } else {
                typ_str(&i.alt)
            };
            format!("#link({})[{}]", typ_quoted(&i.url), text)
        }
        // Raw HTML the model typed: show it literally rather than letting
        // it act as markup.
        Node::Html(h) => typ_str(&h.value),
        other => match other.children() {
            Some(children) => render_inline(children),
            None => String::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// Shared helpers

fn session_title(session: &Session) -> String {
    session
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "Chat export".to_string())
}

/// Date portion of the session's creation timestamp (`YYYY-MM-DD`).
/// jiff's `Timestamp` Display is RFC 3339 (`2026-06-19T12:00:00Z`); we
/// take the date half rather than depend on a specific strftime API.
fn export_date(session: &Session) -> String {
    let ts = session.created_at.to_string();
    ts.split('T').next().unwrap_or(&ts).to_string()
}

/// The text to export for a turn: the user's message for user turns,
/// the model's reply for assistant turns.
fn turn_text(turn: &crate::db::Turn) -> &str {
    match turn.role {
        TurnRole::User => turn.user_content.as_deref().unwrap_or_default(),
        TurnRole::Assistant => turn.content.as_deref().unwrap_or_default(),
    }
}

/// Section label for a turn — `You` for the user, `Assistant` (with the
/// model name when known) for the reply.
fn turn_label(role: &TurnRole, model: Option<&str>) -> String {
    match role {
        TurnRole::User => "You".to_string(),
        TurnRole::Assistant => match model {
            Some(m) if !m.is_empty() => format!("Assistant · {m}"),
            _ => "Assistant".to_string(),
        },
    }
}

/// Make an attachment URL absolute. Presigned S3 URLs are already
/// absolute and pass through; gateway-relative URLs (`/chat/attachment/…`)
/// get the public base URL prefixed so the link survives leaving the app.
fn absolutize(url: &str, base_url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            url.trim_start_matches('/')
        )
    }
}

/// A Typst string literal: `"…"` with backslash, quote, and control
/// characters escaped. Newlines/tabs become their escape sequences so a
/// value can ride inside `#raw(…)` or `#link(…)` on one logical line.
fn typ_quoted(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            '\r' => {}
            '\t' => o.push_str("\\t"),
            _ => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Emit arbitrary text as Typst content via a string expression
/// (`#"…"`). Sidesteps markup escaping entirely: inside a string only
/// `"` and `\` are special, so no user character can be misread as Typst
/// markup (`*`, `#`, `=`, `@`, …).
fn typ_str(s: &str) -> String {
    format!("#{}", typ_quoted(s))
}

/// A raw (monospace, non-highlighted-escaping) block or inline. `block:
/// true` plus a `lang` gives Typst's native code highlighting.
fn typ_raw(value: &str, lang: Option<&str>, block: bool) -> String {
    let mut o = String::from("#raw(");
    if block {
        o.push_str("block: true, ");
    }
    if let Some(l) = lang.filter(|l| !l.is_empty()) {
        o.push_str(&format!("lang: {}, ", typ_quoted(l)));
    }
    o.push_str(&typ_quoted(value));
    o.push(')');
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Turn, TurnRole, TurnStatus};
    use jiff::Timestamp;

    fn session() -> Session {
        let ts: Timestamp = "2026-06-19T08:00:00Z".parse().unwrap();
        Session {
            id: "sess-1".into(),
            user_id: "user-1".into(),
            title: Some("Research on Ceph".into()),
            created_at: ts,
            updated_at: ts,
            shared: false,
            pinned: false,
        }
    }

    fn turn(role: TurnRole, text: &str, model: Option<&str>) -> TurnWithTools {
        let ts: Timestamp = "2026-06-19T08:00:00Z".parse().unwrap();
        let (user_content, content) = match role {
            TurnRole::User => (Some(text.to_string()), None),
            TurnRole::Assistant => (None, Some(text.to_string())),
        };
        TurnWithTools {
            turn: Turn {
                id: "turn-1".into(),
                session_id: "sess-1".into(),
                seq: 0,
                role,
                user_content,
                model: model.map(str::to_string),
                content,
                reasoning: None,
                reasoning_elapsed_ms: None,
                status: TurnStatus::Completed,
                error_message: None,
                created_at: ts,
                completed_at: Some(ts),
            },
            tool_calls: vec![],
        }
    }

    fn opts() -> ExportOpts<'static> {
        ExportOpts {
            base_url: "https://gw.example.com",
        }
    }

    #[test]
    fn markdown_frames_title_date_and_turns() {
        let turns = vec![
            turn(TurnRole::User, "What is RADOS?", None),
            turn(
                TurnRole::Assistant,
                "RADOS is the **object store**.",
                Some("qwen"),
            ),
        ];
        let md = to_markdown(&session(), &turns, &opts());
        assert!(md.contains("# Research on Ceph"));
        assert!(md.contains("*Exported 2026-06-19*"));
        assert!(md.contains("## You"));
        assert!(md.contains("What is RADOS?"));
        assert!(md.contains("## Assistant · qwen"));
        // Assistant Markdown passes through untouched.
        assert!(md.contains("RADOS is the **object store**."));
    }

    #[test]
    fn markdown_rewrites_attachment_marker_to_absolute_link() {
        let marker = crate::attachments::marker_line(
            "report.pdf",
            "application/pdf",
            "/chat/attachment/turn-1/report.pdf",
            1234,
        );
        let body = format!("Here is the file:\n\n{marker}");
        let turns = vec![turn(TurnRole::User, &body, None)];
        let md = to_markdown(&session(), &turns, &opts());
        assert!(
            md.contains("[report.pdf](https://gw.example.com/chat/attachment/turn-1/report.pdf)"),
            "expected an absolute Markdown link, got:\n{md}"
        );
        // The raw marker must not leak into the export.
        assert!(!md.contains("gw-attachment"));
    }

    #[test]
    fn markdown_skips_empty_turns() {
        let turns = vec![turn(TurnRole::Assistant, "   ", None)];
        let md = to_markdown(&session(), &turns, &opts());
        assert!(!md.contains("## Assistant"));
    }

    #[test]
    fn typst_has_preamble_and_turn_labels() {
        let turns = vec![
            turn(TurnRole::User, "Question?", None),
            turn(TurnRole::Assistant, "Answer.", Some("qwen")),
        ];
        let typ = to_typst(&session(), &turns, &opts());
        assert!(typ.contains("#set document(title: \"Research on Ceph\")"));
        assert!(typ.contains("#set page("));
        // Labels are uppercased in the rendered block.
        assert!(typ.contains("YOU"));
        assert!(typ.contains("ASSISTANT · QWEN"));
    }

    #[test]
    fn md_to_typst_maps_core_constructs() {
        let typ = md_to_typst("# Heading\n\nSome **bold** and `code` and a [link](https://e.x).");
        assert!(typ.contains("= "), "heading → = : {typ}");
        assert!(typ.contains("#strong["), "bold → #strong: {typ}");
        assert!(typ.contains("#raw("), "inline code → #raw: {typ}");
        assert!(
            typ.contains("#link(\"https://e.x\")"),
            "link → #link: {typ}"
        );
    }

    #[test]
    fn md_to_typst_fenced_code_block_carries_language() {
        let typ = md_to_typst("```rust\nfn main() {}\n```");
        assert!(
            typ.contains("#raw(block: true, lang: \"rust\""),
            "got: {typ}"
        );
        // The code body is preserved (newlines escaped inside the string).
        assert!(typ.contains("fn main() {}"));
    }

    #[test]
    fn md_to_typst_escapes_markup_significant_text() {
        // Characters that mean something in Typst markup must be neutralised
        // by routing text through a string expression.
        let typ = md_to_typst("use #set and = signs and * stars");
        assert!(
            typ.contains("#\""),
            "text should ride in a string expr: {typ}"
        );
        // None of the raw markup chars escape the string as bare markup.
        assert!(!typ.contains("\n= signs"));
    }

    #[test]
    fn md_to_typst_renders_lists() {
        let typ = md_to_typst("- one\n- two\n  - nested");
        assert!(typ.contains("- "), "bullet marker: {typ}");
        // One nesting level = two spaces of indent in the Typst markup.
        assert!(
            typ.contains("\n  - #\"nested\""),
            "nested item is indented: {typ}"
        );
    }

    #[test]
    fn typst_attachment_becomes_link() {
        let marker = crate::attachments::marker_line(
            "chart.png",
            "image/png",
            "/chat/attachment/turn-1/chart.png",
            10,
        );
        let turns = vec![turn(
            TurnRole::Assistant,
            &format!("see:\n\n{marker}"),
            None,
        )];
        let typ = to_typst(&session(), &turns, &opts());
        assert!(
            typ.contains("#link(\"https://gw.example.com/chat/attachment/turn-1/chart.png\")"),
            "got: {typ}"
        );
    }

    #[test]
    fn typ_quoted_escapes_quotes_and_backslashes() {
        assert_eq!(typ_quoted(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(typ_quoted("line1\nline2"), r#""line1\nline2""#);
    }

    #[test]
    fn absolutize_leaves_presigned_urls_untouched() {
        assert_eq!(
            absolutize("https://bucket/x?sig=1", "https://gw.example.com"),
            "https://bucket/x?sig=1"
        );
        assert_eq!(
            absolutize("/chat/attachment/t/x.pdf", "https://gw.example.com/"),
            "https://gw.example.com/chat/attachment/t/x.pdf"
        );
    }
}
