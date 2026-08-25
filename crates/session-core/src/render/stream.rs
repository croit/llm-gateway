// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Delta protocol for streaming an in-progress assistant turn over
//! SSE without re-sending accumulated content on every tick.
//!
//! The pre-delta design patched `#turn-<id>` (`mode outer`) with a
//! full re-render after every persisted upstream chunk, making wire
//! cost quadratic in turn length — a single long reply could ship
//! hundreds of MB of repeated prefix. This module computes, per
//! subscriber, the *minimal* set of element patches that bring the
//! turn bubble from its last-sent state to the current DB row:
//!
//! - **Sealed markdown blocks** — content is split at safe block
//!   boundaries (blank lines outside fenced code, respecting indented
//!   continuations). A block is rendered exactly once and appended to
//!   `#turn-<id>-text` in a stable wrapper (`tu-<turn>-<n>`).
//! - **The open trailing block** — the only unit re-rendered per
//!   tick. Its patch fragment is the wrapper itself, so it must
//!   REPLACE the wrapper (`mode outer` on `#tu-<turn>-<n>`);
//!   `mode inner` would nest the wrapper inside itself and
//!   duplicate content on every subsequent tick.
//! - **Tool calls** — the `#turn-<id>-tools` container is inner-
//!   patched only when its rendered HTML actually changed (the
//!   fragment there is the container's children, without the
//!   wrapper).
//! - **The shell** — the whole turn is outer-patched only when its
//!   *phase signature* changes (reasoning appears, first content
//!   lands, error), not per delta.
//! - **Thinking gate** — while the turn is in progress, streamed
//!   shells carry an *empty* `thinking-body`; the trace ships only
//!   through the on-demand `/thinking` sub-stream the client opens
//!   by expanding the `<details>`. The completed trace travels the
//!   main stream exactly once, in the settled render.
//!
//! Safety net: any settled (non-in-progress) row, and any unexpected
//! content shrink, falls back to one authoritative full render, so a
//! mid-stream splitter artefact can never outlive the turn.

use plait::{Html, ToHtml, html};

use crate::attachments::{ParsedAttachment, Segment, split_markers_for_turn, strip_replay_stubs};
use crate::db::{ToolCall, Turn, TurnStatus, TurnWithTools};
use crate::i18n::Lang;

use super::CopyLabels;
use super::assistant::{assemble_assistant_turn, render_thinking_block};
use super::attachments::{BodyPiece, media_kind, render_body};
use super::{render_assistant_turn, render_markdown_with_copy, render_tool_call_list};

/// One element patch to put on the wire: a datastar
/// `patch-elements` event body (selector + mode + fragment).
#[derive(Debug, PartialEq)]
pub struct StreamPatch {
    pub selector: String,
    pub mode: &'static str,
    pub html: String,
}

/// One renderable slice of an in-progress turn's content. Markdown
/// blocks and attachment chips, in document order.
#[derive(Debug, PartialEq)]
enum StreamUnit {
    Md(String),
    Attachment(ParsedAttachment),
}

/// Per-subscriber diff state for one assistant turn. Feed it the
/// current DB row on every tick; it emits only what changed since the
/// patches it produced last.
pub struct TurnStream {
    turn_id: String,
    actions: Option<String>,
    lang: Lang,
    /// Phase signature the on-the-wire shell reflects. `None` until
    /// the first diff (or after a settled render) forces a full paint.
    sig: Option<[bool; 4]>,
    settled_sent: bool,
    /// Rendered HTML of every content unit currently on the wire
    /// (wrapper included), in unit order.
    sent_units: Vec<String>,
    tools_html: String,
}

impl TurnStream {
    pub fn new(turn_id: &str, actions: Option<&str>, lang: Lang) -> Self {
        Self {
            turn_id: turn_id.to_string(),
            actions: actions.map(str::to_string),
            lang,
            sig: None,
            settled_sent: false,
            sent_units: Vec::new(),
            tools_html: String::new(),
        }
    }

    /// Compute the patches that bring the wire from its last-sent
    /// state to `tw`. Cheap to call repeatedly: an unchanged row
    /// yields no events.
    pub fn diff(&mut self, tw: &TurnWithTools) -> Vec<StreamPatch> {
        let turn = &tw.turn;
        if turn.status != TurnStatus::InProgress {
            return self.diff_settled(tw);
        }
        let sig = [
            turn.reasoning.as_deref().is_some_and(|r| !r.is_empty()),
            turn.content.as_deref().is_some_and(|c| !c.is_empty()),
            turn.reasoning_elapsed_ms.is_some(),
            turn.status == TurnStatus::Errored,
        ];
        if self.sig != Some(sig) {
            self.sig = Some(sig);
            self.sent_units = self.unit_htmls(turn);
            self.tools_html = self.tools_html(&tw.tool_calls);
            let shell = self.render_shell(turn, &tw.tool_calls);
            return vec![StreamPatch {
                selector: format!("#turn-{}", self.turn_id),
                mode: "outer",
                html: shell,
            }];
        }

        let mut events = Vec::new();
        let tools_html = self.tools_html(&tw.tool_calls);
        if tools_html != self.tools_html {
            self.tools_html = tools_html.clone();
            events.push(StreamPatch {
                selector: format!("#turn-{}-tools", self.turn_id),
                mode: "inner",
                html: tools_html,
            });
        }

        let units = self.unit_htmls(turn);
        if units.len() < self.sent_units.len() {
            // Content shrank (a writer we don't model, e.g. an edit
            // racing the stream). Resync the whole text container.
            events.push(StreamPatch {
                selector: format!("#turn-{}-text", self.turn_id),
                mode: "inner",
                html: units.to_vec().join(""),
            });
        } else {
            for (i, html) in units.iter().enumerate().take(self.sent_units.len()) {
                if &self.sent_units[i] != html {
                    // The fragment is the wrapper itself, so this must
                    // REPLACE the wrapper (outer) — inner would nest
                    // `#tu-…-<i>` inside itself and duplicate content
                    // on every subsequent tick.
                    events.push(StreamPatch {
                        selector: format!("#tu-{}-{i}", self.turn_id),
                        mode: "outer",
                        html: html.clone(),
                    });
                }
            }
            if units.len() > self.sent_units.len() {
                let fresh: String = units[self.sent_units.len()..].concat();
                events.push(StreamPatch {
                    selector: format!("#turn-{}-text", self.turn_id),
                    mode: "append",
                    html: fresh,
                });
            }
        }
        self.sent_units = units;
        events
    }

    fn diff_settled(&mut self, tw: &TurnWithTools) -> Vec<StreamPatch> {
        if self.settled_sent {
            return Vec::new();
        }
        self.settled_sent = true;
        self.sig = None;
        vec![StreamPatch {
            selector: format!("#turn-{}", self.turn_id),
            mode: "outer",
            html: render_assistant_turn(tw, self.actions.as_deref(), self.lang).to_string(),
        }]
    }

    /// The in-progress shell: identical structure to a settled render
    /// except the thinking body is gated (empty while reasoning still
    /// arrives; final once frozen) and the text container carries the
    /// stream wrappers instead of a one-shot segment render.
    fn render_shell(&self, turn: &Turn, tools: &[ToolCall]) -> String {
        let thinking: Html = if turn.reasoning.as_deref().is_some_and(|r| !r.is_empty()) {
            // While the turn is in progress the trace is gated: empty
            // body + the toggle that opens the on-demand sub-stream —
            // even after reasoning froze, since a fresh shell patch
            // would otherwise re-ship the whole trace per phase
            // change. The settled render is the single place it
            // travels on the main stream.
            render_thinking_block(turn, Some(&self.thinking_stream_url(turn)), self.lang)
        } else {
            let slot_id = format!("turn-{}-thinking", turn.id);
            html! { div(id: (slot_id), class: "thinking-block-slot") {} }.to_html()
        };

        let text_children: Html = if self.sent_units.is_empty() {
            html! {}.to_html()
        } else {
            html! {
                for u in self.sent_units.iter() { #(u.clone()) }
            }
            .to_html()
        };

        assemble_assistant_turn(
            turn,
            thinking,
            tools,
            text_children,
            self.actions.as_deref(),
            self.lang,
        )
        .to_string()
    }

    fn thinking_stream_url(&self, turn: &Turn) -> String {
        format!(
            "{}/{}/turns/{}/thinking",
            self.actions.as_deref().unwrap_or_default(),
            turn.session_id,
            turn.id
        )
    }

    fn tools_html(&self, tools: &[ToolCall]) -> String {
        render_tool_call_list(tools, &self.turn_id, self.lang).to_string()
    }

    /// Render every content unit (wrapper included) in document
    /// order. Unit indexes are stable while content only grows, which
    /// is the streaming invariant; the shrink case resyncs above.
    fn unit_htmls(&self, turn: &Turn) -> Vec<String> {
        let content = turn.content.clone().unwrap_or_default();
        let content = strip_replay_stubs(&content);
        let segments = split_markers_for_turn(content.as_ref(), &self.turn_id);
        let mut units: Vec<StreamUnit> = Vec::new();
        let last = segments.len().saturating_sub(1);
        for (idx, seg) in segments.iter().enumerate() {
            match seg {
                Segment::Text(t) => {
                    let (mut sealed, open) = split_blocks(t);
                    if idx != last {
                        sealed.extend(open);
                    } else if let Some(open) = open {
                        sealed.push(open);
                    }
                    units.extend(sealed.into_iter().map(|b| StreamUnit::Md(b.to_string())));
                }
                Segment::Attachment(a) => units.push(StreamUnit::Attachment(a.clone())),
            }
        }
        units
            .iter()
            .enumerate()
            .map(|(i, u)| self.unit_html(turn, i, u))
            .collect()
    }

    fn unit_html(&self, turn: &Turn, idx: usize, unit: &StreamUnit) -> String {
        let id = format!("tu-{}-{idx}", self.turn_id);
        match unit {
            StreamUnit::Md(text) => {
                let labels = CopyLabels::for_lang(self.lang);
                let rendered = render_markdown_with_copy(text, &labels);
                html! { div(id: (id), class: "tu") { #(rendered) } }
                    .to_html()
                    .to_string()
            }
            StreamUnit::Attachment(att) => {
                let piece = match media_kind(att) {
                    Some(_) => BodyPiece::Media(att.clone()),
                    None => BodyPiece::File(att.clone()),
                };
                let remove_prefix = self
                    .actions
                    .as_ref()
                    .map(|base| format!("{base}/{}/turns/{}/attachment", turn.session_id, turn.id));
                let items = render_body(&[piece], remove_prefix.as_deref(), self.lang);
                html! { div(id: (id), class: "tu") { (items.first().cloned().unwrap_or_else(|| html! {}.to_html())) } }.to_html().to_string()
            }
        }
    }
}

/// Split one prose segment into sealed markdown blocks plus the open
/// trailing block. A block is sealed at a blank line that is outside
/// a fenced code block and not followed by an indented (continuation)
/// line. Fence interior blank lines stay in the open block so code is
/// never split mid-fence.
pub(crate) fn split_blocks(text: &str) -> (Vec<&str>, Option<&str>) {
    let mut sealed: Vec<&str> = Vec::new();
    let mut block_start: Option<usize> = None;
    let mut fence: Option<(char, usize)> = None;
    let lines: Vec<(usize, &str)> = line_spans(text).collect();

    for (i, (start, line)) in lines.iter().enumerate() {
        let (start, line) = (*start, *line);
        let rest = &line[rest_offset(line)..];
        let trimmed = rest.trim();
        if let Some((fc, flen)) = fence {
            if trimmed.starts_with(&std::iter::repeat_n(fc, flen).collect::<String>())
                && trimmed.chars().all(|c| c == fc)
            {
                fence = None;
            }
            continue;
        }
        let fence_open = if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let fc = trimmed.chars().next().unwrap();
            let flen = trimmed.chars().take_while(|&c| c == fc).count();
            Some((fc, flen))
        } else {
            None
        };
        if let Some(f) = fence_open {
            fence = Some(f);
            block_start.get_or_insert(start);
            continue;
        }
        if !trimmed.is_empty() {
            block_start.get_or_insert(start);
            continue;
        }
        // Blank line outside a fence: seal only if the next non-blank
        // line is not an indented continuation (lazy paragraph /
        // list-item continuation, indented code). Indentation is
        // measured on the raw line — stripping the ≤3 leading spaces
        // `rest_offset` removes would disguise a 4-space code indent.
        let blank_end = start + line.len();
        let mut continuation = false;
        let mut any_next = false;
        for (_, next) in &lines[i + 1..] {
            if next.trim().is_empty() {
                continue;
            }
            any_next = true;
            continuation = next.starts_with("    ") || next.starts_with('\t');
            break;
        }
        if let Some(bs) = block_start {
            if !any_next {
                sealed.push(&text[bs..text.len()]);
                block_start = None;
            } else if !continuation {
                sealed.push(&text[bs..blank_end]);
                block_start = None;
            }
        }
    }
    let open = block_start.map(|bs| &text[bs..]);
    (sealed, open)
}

/// Byte span iterator: yields (byte_offset_of_line_start, line_including_newline).
fn line_spans(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    std::iter::from_fn(move || {
        if offset >= text.len() {
            return None;
        }
        let start = offset;
        let end = text[offset..]
            .find('\n')
            .map(|i| offset + i + 1)
            .unwrap_or(text.len());
        offset = end;
        Some((start, &text[start..end]))
    })
}

/// Byte offset of the line content after up-to-three leading spaces
/// (fences may be indented; more indentation means code block).
fn rest_offset(line: &str) -> usize {
    let mut spaces = 0;
    for (i, c) in line.char_indices() {
        if c == ' ' && spaces < 3 {
            spaces += 1;
        } else {
            return i;
        }
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Turn, TurnRole, TurnStatus};

    fn turn_with(content: &str, reasoning: Option<&str>) -> TurnWithTools {
        let now = jiff::Timestamp::now();
        TurnWithTools {
            turn: Turn {
                id: "t1".into(),
                session_id: "s1".into(),
                seq: 1,
                role: TurnRole::Assistant,
                user_content: None,
                model: Some("m".into()),
                content: Some(content.to_string()),
                reasoning: reasoning.map(str::to_string),
                reasoning_elapsed_ms: None,
                reasoning_started_at: reasoning.map(|_| jiff::Timestamp::now()),
                status: TurnStatus::InProgress,
                error_message: None,
                created_at: now,
                completed_at: None,
            },
            tool_calls: vec![],
        }
    }

    fn stream() -> TurnStream {
        TurnStream::new("t1", Some("/chat"), Lang::En)
    }

    #[test]
    fn split_blocks_seals_at_blank_lines() {
        let (sealed, open) = split_blocks("one\n\ntwo\n\nthree");
        assert_eq!(sealed, vec!["one\n\n", "two\n\n"]);
        assert_eq!(open, Some("three"));
    }

    #[test]
    fn split_blocks_keeps_blank_lines_inside_fences() {
        let md = "```rust\nfn a() {}\n\nfn b() {}\n```\n";
        let (sealed, open) = split_blocks(md);
        assert!(sealed.is_empty());
        assert_eq!(open, Some(md));
    }

    #[test]
    fn split_blocks_seals_a_closed_fence_when_more_follows() {
        let md = "```rust\nfn a() {}\n```\n\nnext paragraph";
        let (sealed, open) = split_blocks(md);
        assert_eq!(sealed.len(), 1);
        assert_eq!(open, Some("next paragraph"));
    }

    #[test]
    fn split_blocks_does_not_seal_indented_continuations() {
        // A blank line followed by an indented line is a lazy/indented
        // continuation, not a block boundary.
        let (sealed, open) = split_blocks("- item\n\n    continued code\n\n- after");
        assert_eq!(sealed.len(), 1);
        assert_eq!(open, Some("- after"));
    }

    #[test]
    fn split_blocks_no_trailing_blank_means_open_tail() {
        let (sealed, open) = split_blocks("a\n\nb");
        assert_eq!(sealed, vec!["a\n\n"]);
        assert_eq!(open, Some("b"));
    }

    #[test]
    fn split_blocks_trailing_blank_line_seals_everything() {
        let (sealed, open) = split_blocks("a\n\nb\n\n");
        assert_eq!(sealed, vec!["a\n\n", "b\n\n"]);
        assert_eq!(open, None);
    }

    #[test]
    fn split_blocks_empty_text_yields_nothing() {
        let (sealed, open) = split_blocks("");
        assert!(sealed.is_empty());
        assert_eq!(open, None);
    }

    #[test]
    fn first_diff_paints_the_shell_once() {
        let tw = turn_with("", None);
        let events = stream().diff(&tw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].selector, "#turn-t1");
        assert_eq!(events[0].mode, "outer");
        assert!(events[0].html.contains("chat-msg--assistant"));
    }

    #[test]
    fn unchanged_rows_emit_nothing() {
        let mut s = stream();
        let tw = turn_with("Hel", None);
        s.diff(&tw);
        assert!(s.diff(&tw).is_empty(), "same row must be a no-op");
    }

    #[test]
    fn growing_the_open_block_replaces_only_its_wrapper() {
        let mut s = stream();
        s.diff(&turn_with("Hel", None));
        let events = s.diff(&turn_with("Hello", None));
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].selector, "#tu-t1-0");
        assert_eq!(events[0].mode, "outer");
        assert!(events[0].html.contains("Hello"));
    }

    /// The wire-linearity property the whole module exists for: once
    /// a block sealed and a later unit appeared, the earlier unit is
    /// frozen — no later event may carry it again. And the whole
    /// stream stays linear in content size, not quadratic.
    #[test]
    fn sealed_paragraphs_travel_exactly_once() {
        let mut s = stream();
        // 30 paragraphs × 80 chars — big enough that per-patch wrapper
        // overhead doesn't dominate, so a linear bound discriminates.
        let full = (0..30)
            .map(|i| format!("paragraph number {i} padded to roughly eighty characters wide\n\n"))
            .collect::<String>();
        let full = &full[..full.len() - 2];
        let mut sent: Vec<StreamPatch> = Vec::new();
        for len in (40..=full.len()).step_by(40) {
            sent.extend(s.diff(&turn_with(&full[..len], None)));
        }
        sent.extend(s.diff(&turn_with(full, None)));
        let mut superseded = false;
        for e in &sent {
            let mentions_1 = e.selector == "#tu-t1-1" || e.html.contains("tu-t1-1");
            let mentions_0 = e.selector == "#tu-t1-0" || e.html.contains("tu-t1-0");
            if mentions_1 {
                superseded = true;
            }
            assert!(
                !(superseded && mentions_0),
                "a sealed unit re-shipped after a later unit appeared: {e:?}"
            );
        }
        let total: usize = sent.iter().map(|e| e.html.len()).sum();
        assert!(
            total < full.len() * 10,
            "wire must stay linear in content size: {total} bytes for {} of content",
            full.len()
        );
    }

    #[test]
    fn reasoning_while_arriving_is_never_on_the_main_wire() {
        let mut s = stream();
        let mut tw = turn_with("", Some("step one"));
        let events = s.diff(&tw);
        let wire: String = events.iter().map(|e| e.html.clone()).collect();
        assert!(
            !wire.contains("step one"),
            "live reasoning must not ship in the main stream:\n{wire}"
        );
        assert!(wire.contains("thinking-timer"), "timer still shows: {wire}");
        // The details carries the opt-in sub-stream trigger.
        assert!(
            wire.contains("data-on:toggle"),
            "expanding the trace must be able to open the sub-stream:\n{wire}"
        );

        tw.turn.reasoning = Some("step one step two".into());
        let events = s.diff(&tw);
        assert!(
            events.is_empty(),
            "reasoning growth alone must not produce patches:\n{events:?}"
        );
    }

    #[test]
    fn frozen_reasoning_waits_for_the_settled_render() {
        let mut s = stream();
        let mut tw = turn_with("", Some("deep thought"));
        s.diff(&tw);
        // First content delta freezes the reasoning timer — the label
        // settles, but the trace itself still doesn't ride the main
        // stream while the turn is in progress.
        tw.turn.content = Some("answer".into());
        tw.turn.reasoning_elapsed_ms = Some(1200);
        let events = s.diff(&tw);
        assert_eq!(events.len(), 1);
        assert!(
            !events[0].html.contains("deep thought"),
            "frozen reasoning must wait for the settled render:\n{}",
            events[0].html
        );
        assert!(events[0].html.contains("Thought for"));
        assert!(
            events[0].html.contains("data-on:toggle"),
            "expanding the trace mid-turn must still fetch it on demand"
        );

        // And never re-ships while content keeps streaming.
        tw.turn.content = Some("answer grows".into());
        let events = s.diff(&tw);
        assert!(
            !events.iter().any(|e| e.html.contains("deep thought")),
            "frozen reasoning must not re-ship per tick:\n{events:?}"
        );

        // Settle: the one authoritative carrier.
        tw.turn.status = TurnStatus::Completed;
        tw.turn.completed_at = Some(jiff::Timestamp::now());
        let events = s.diff(&tw);
        assert_eq!(events.len(), 1);
        assert!(events[0].html.contains("deep thought"));
    }

    #[test]
    fn settled_thinking_block_has_no_toggle_attribute() {
        // `data-on:toggle=""` makes datastar throw ValueRequired on
        // every mutation observation of the details element — the
        // console error that broke patch application at finalize.
        let mut tw = turn_with("answer", Some("reasoning text"));
        tw.turn.reasoning_elapsed_ms = Some(900);
        tw.turn.status = TurnStatus::Completed;
        tw.turn.completed_at = Some(jiff::Timestamp::now());
        let html = render_assistant_turn(&tw, Some("/chat"), Lang::En).to_string();
        assert!(
            !html.contains("data-on:toggle"),
            "the settled trace must carry no toggle directive (empty value breaks datastar):\n{html}"
        );
    }

    #[test]
    fn changed_units_patch_the_wrapper_outer_not_inner() {
        // Regression (word salad): inner-patching `#tu-<turn>-<n>` with
        // the full wrapper div nested the wrapper inside itself
        // (`#tu-0 > #tu-0 > …`), duplicating content on every tick and
        // driving idiomorph into `moveBefore` hierarchy errors. The
        // fragment for a changed unit is the wrapper itself, so the
        // patch must replace it (outer), not fill it (inner).
        let mut s = stream();
        s.diff(&turn_with("Hel", None));
        let events = s.diff(&turn_with("Hello", None));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].selector, "#tu-t1-0");
        assert_eq!(events[0].mode, "outer", "{events:?}");
        assert!(events[0].html.contains(r#"id="tu-t1-0""#));
    }

    #[test]
    fn settled_turns_render_the_authoritative_full_bubble_once() {
        let mut s = stream();
        s.diff(&turn_with("partial", None));
        let mut tw = turn_with("the full answer", None);
        tw.turn.status = TurnStatus::Completed;
        tw.turn.completed_at = Some(jiff::Timestamp::now());
        let events = s.diff(&tw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].mode, "outer");
        assert!(events[0].html.contains("the full answer"));
        assert!(s.diff(&tw).is_empty());
    }

    #[test]
    fn content_shrink_resyncs_the_text_container() {
        let mut s = stream();
        s.diff(&turn_with("one\n\ntwo\n\nthree", None));
        let events = s.diff(&turn_with("short", None));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].selector, "#turn-t1-text");
        assert_eq!(events[0].mode, "inner");
        assert!(events[0].html.contains("short"));
    }

    #[test]
    fn attachment_markers_append_chip_units() {
        let marker = crate::attachments::marker_line(
            "pic.png",
            "image/png",
            "/chat/attachment/t1/pic.png",
            10,
        );
        let mut s = stream();
        let body = format!("here\n\n{marker}");
        let events = s.diff(&turn_with(&body, None));
        assert_eq!(events.len(), 1);
        assert!(events[0].html.contains("chat-msg__attachment-image"));
    }

    #[test]
    fn tool_rows_patch_only_when_they_change() {
        use crate::db::{ToolCall, ToolCallStatus};
        let mut s = stream();
        let tw = turn_with("", None);
        s.diff(&tw);
        assert!(s.diff(&tw).is_empty());

        let tc = ToolCall {
            id: "a".into(),
            turn_id: "t1".into(),
            seq: 0,
            name: "echo".into(),
            arguments_json: "{}".into(),
            output_json: None,
            status: ToolCallStatus::Running,
            created_at: jiff::Timestamp::now(),
            completed_at: None,
        };
        let mut tw2 = turn_with("", None);
        tw2.tool_calls = vec![tc.clone()];
        let events = s.diff(&tw2);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].selector, "#turn-t1-tools");

        // Same tool, same state → no re-send.
        assert!(s.diff(&tw2).is_empty());
    }
}
