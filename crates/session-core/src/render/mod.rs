// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! HTML renderers for the chat-style session UI.
//!
//! Driver-agnostic. The gateway uses these for OpenAI-backed turns;
//! a future consumer could reuse them for its own driver. The
//! POST/tail URLs and a few cosmetic toggles are parameterised
//! through `ComposerOpts` / `render_conversation`'s
//! `in_flight_tail_url`.
//!
//! All renderers are pure functions of their inputs — same shape on
//! the initial server render and on every SSE patch the worker
//! drives, so the morphdom-style diff datastar runs across patches
//! preserves per-element interactive state (collapsed `<details>`,
//! scroll position) automatically.
//!
//! Per-turn DOM ids carry the turn UUID so two concurrent stream
//! attaches (multiple tabs, retry-on-recover) can't cross-write.

use plait::{Html, ToHtml, html};

use crate::db::{ToolCall, ToolCallStatus, Turn, TurnRole, TurnStatus, TurnWithTools};
use crate::i18n::{self, Lang, t, t_args};
use crate::icons;

mod assistant;
mod attachments;
mod canvas;
mod composer;
mod md;
mod tool_calls;
mod turns;

pub use assistant::*;
pub use attachments::*;
pub use canvas::*;
pub use composer::*;
pub use md::*;
pub use tool_calls::*;
pub use turns::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn composer(streaming: bool) -> String {
        render_composer(ComposerOpts {
            post_url: "/chat/s1/messages",
            cancel_url: "/chat/s1/cancel",
            placeholder: "msg",
            has_voice: false,
            voice_out: false,
            streaming,
            toolbar: None,
            lang: Lang::En,
        })
        .to_string()
    }

    #[test]
    fn composer_arms_stop_when_a_turn_is_in_flight() {
        // Server seeds $chatStreaming=true so the Stop control shows on
        // load/reload — the fix for "I reloaded and there's no stop button".
        assert!(
            composer(true).contains("chatStreaming: true"),
            "an in-flight turn must seed the streaming signal true"
        );
    }

    #[test]
    fn composer_is_idle_by_default() {
        assert!(
            composer(false).contains("chatStreaming: false"),
            "an idle composer must not show the streaming/stop state"
        );
    }

    fn conv_turn(seq: i64, role: TurnRole, text: &str) -> TurnWithTools {
        let now = jiff::Timestamp::now();
        let (user_content, content) = match role {
            TurnRole::User => (Some(text.to_string()), None),
            TurnRole::Assistant => (None, Some(text.to_string())),
        };
        TurnWithTools {
            turn: Turn {
                id: format!("t{seq}"),
                session_id: "s1".into(),
                seq,
                role,
                user_content,
                model: None,
                content,
                reasoning: None,
                reasoning_elapsed_ms: None,
                reasoning_started_at: None,
                status: TurnStatus::Completed,
                error_message: None,
                created_at: now,
                completed_at: Some(now),
            },
            tool_calls: vec![],
        }
    }

    /// An assistant turn carrying reasoning, for the thinking-timer tests.
    fn reasoning_turn(
        content: Option<&str>,
        reasoning_elapsed_ms: Option<i64>,
        status: TurnStatus,
    ) -> TurnWithTools {
        let now = jiff::Timestamp::now();
        TurnWithTools {
            turn: Turn {
                id: "t1".into(),
                session_id: "s1".into(),
                seq: 0,
                role: TurnRole::Assistant,
                user_content: None,
                model: None,
                content: content.map(str::to_string),
                reasoning: Some("let me think".into()),
                reasoning_elapsed_ms,
                reasoning_started_at: Some(now),
                status,
                error_message: None,
                created_at: now,
                completed_at: None,
            },
            tool_calls: vec![],
        }
    }

    #[test]
    fn thinking_timer_ticks_client_side_while_reasoning_streams() {
        // Reasoning in progress (no content, elapsed not yet frozen): the
        // block must emit the client-driven <thinking-timer>, not a settled
        // label.
        let tw = reasoning_turn(None, None, TurnStatus::InProgress);
        let html = render_assistant_turn(&tw, None, Lang::En).to_string();
        assert!(
            html.contains("<thinking-timer"),
            "live client timer expected while reasoning streams: {html}"
        );
        assert!(
            html.contains("data-label-template="),
            "timer needs its template: {html}"
        );
        assert!(
            !html.contains("Thought for"),
            "must not read as finalized while still reasoning: {html}"
        );
    }

    #[test]
    fn thinking_timer_finalizes_when_reasoning_freezes_even_mid_stream() {
        // Regression (code-review): the timer must stop when reasoning ends —
        // i.e. when `reasoning_elapsed_ms` is frozen on the first content
        // delta — NOT when the whole turn finalizes. A still-InProgress turn
        // that has produced content and a frozen elapsed must show the settled
        // "Thought for X.Ys" label and drop the live <thinking-timer>, so the
        // count can't keep climbing through the answer stream.
        let tw = reasoning_turn(
            Some("answer, still streaming…"),
            Some(3000),
            TurnStatus::InProgress,
        );
        let html = render_assistant_turn(&tw, None, Lang::En).to_string();
        assert!(
            html.contains("Thought for"),
            "reasoning must read as finalized once its elapsed is frozen: {html}"
        );
        assert!(
            !html.contains("<thinking-timer"),
            "no live client timer once reasoning is frozen, even mid content-stream: {html}"
        );
    }

    #[test]
    fn compaction_divider_marks_boundary_when_compacted() {
        let turns = vec![
            conv_turn(0, TurnRole::User, "q1"),
            conv_turn(1, TurnRole::Assistant, "a1"),
            conv_turn(2, TurnRole::User, "q2"),
        ];
        // Compacted up to seq 1 → divider appears before the seq-2 turn.
        let html = render_conversation(&turns, None, Some("/chat"), Some(1), Lang::En).to_string();
        assert!(
            html.contains("Earlier messages condensed to save context"),
            "divider must render when the session is compacted"
        );
    }

    #[test]
    fn no_compaction_divider_without_compaction() {
        let turns = vec![
            conv_turn(0, TurnRole::User, "q1"),
            conv_turn(1, TurnRole::Assistant, "a1"),
        ];
        let html = render_conversation(&turns, None, Some("/chat"), None, Lang::En).to_string();
        assert!(
            !html.contains("Earlier messages condensed"),
            "no divider when the session was never compacted"
        );
    }

    #[test]
    fn no_compaction_divider_when_cutoff_precedes_all_visible_turns() {
        // Every visible turn is already past the cutoff (folded turns aren't in
        // this slice) → no divider, since there's nothing summarised above it.
        let turns = vec![
            conv_turn(4, TurnRole::User, "q3"),
            conv_turn(5, TurnRole::Assistant, "a3"),
        ];
        let html = render_conversation(&turns, None, Some("/chat"), Some(1), Lang::En).to_string();
        assert!(!html.contains("Earlier messages condensed"));
    }

    #[test]
    fn every_turn_carries_a_localizable_timestamp() {
        // Each message renders a `<time class="chat-msg__time">` whose
        // `datetime` is the RFC3339 UTC instant (so scroll.ts can
        // localize it to the viewer's zone) with a UTC `HH:MM` fallback
        // as the visible text for the no-JS / pre-mount case.
        let fixed = "2026-07-10T14:32:07Z".parse::<jiff::Timestamp>().unwrap();
        let mut user = conv_turn(0, TurnRole::User, "q1");
        let mut assistant = conv_turn(1, TurnRole::Assistant, "a1");
        user.turn.created_at = fixed;
        assistant.turn.created_at = fixed;

        let html = render_conversation(&[user, assistant], None, Some("/chat"), None, Lang::En)
            .to_string();

        // One timestamp per message.
        assert_eq!(
            html.matches(r#"class="chat-msg__time""#).count(),
            2,
            "both the user and assistant bubbles must carry a timestamp: {html}"
        );
        // The machine-readable instant scroll.ts localizes from.
        assert!(
            html.contains(r#"datetime="2026-07-10T14:32:07Z""#),
            "the <time> must expose the RFC3339 UTC instant: {html}"
        );
        // The UTC fallback shown before (and without) JS localization.
        assert!(
            html.contains(">14:32<"),
            "the <time> must render a UTC HH:MM fallback: {html}"
        );
    }

    fn tool_call(id: &str, name: &str, status: ToolCallStatus) -> ToolCall {
        ToolCall {
            id: id.into(),
            turn_id: "t1".into(),
            seq: 0,
            name: name.into(),
            arguments_json: "{}".into(),
            output_json: Some("{}".into()),
            status,
            created_at: jiff::Timestamp::now(),
            completed_at: None,
        }
    }

    #[test]
    fn few_tool_calls_render_flat_without_a_group() {
        let calls: Vec<ToolCall> = (0..TOOL_GROUP_THRESHOLD)
            .map(|i| tool_call(&format!("c{i}"), "rag_search", ToolCallStatus::Completed))
            .collect();
        let html = render_tool_call_list(&calls, "t1", Lang::En).to_string();
        assert!(
            !html.contains("tool-calls-group"),
            "at/below the threshold the rows stay flat: {html}"
        );
        // Each individual row is still present.
        assert_eq!(
            html.matches("tool-call__name").count(),
            TOOL_GROUP_THRESHOLD
        );
    }

    #[test]
    fn many_tool_calls_collapse_into_one_group_with_a_tally() {
        let calls: Vec<ToolCall> = (0..13)
            .map(|i| tool_call(&format!("c{i}"), "rag_search", ToolCallStatus::Completed))
            .collect();
        let html = render_tool_call_list(&calls, "t1", Lang::En).to_string();
        assert!(
            html.contains("tool-calls-group"),
            "expected a group wrapper"
        );
        // Summary tallies them by name so the reader sees the count
        // without unfolding.
        assert!(
            html.contains("13 calls"),
            "summary should show the count: {html}"
        );
        assert!(
            html.contains("rag_search ×13"),
            "summary should tally by name: {html}"
        );
        // The individual rows still live inside (unfold on click). DOM ids are
        // scoped by turn (`tc-<turn_id>-<id>`).
        assert!(html.contains("tc-t1-c0") && html.contains("tc-t1-c12"));
        // Stable group id so morph preserves the open/close toggle.
        assert!(html.contains("turn-t1-tools-group"));
    }

    #[test]
    fn group_summary_reflects_mixed_names_and_running_state() {
        let mut calls = vec![
            tool_call("a", "rag_search", ToolCallStatus::Completed),
            tool_call("b", "rag_search", ToolCallStatus::Completed),
            tool_call("c", "fetch_url", ToolCallStatus::Completed),
            tool_call("d", "rag_search", ToolCallStatus::Running),
        ];
        calls[3].status = ToolCallStatus::Running;
        let html = render_tool_call_list(&calls, "t9", Lang::En).to_string();
        assert!(html.contains("rag_search ×3"), "tally per name: {html}");
        assert!(html.contains("fetch_url"), "all names listed: {html}");
        assert!(
            html.contains("Running tools"),
            "any running call flips the group label to running: {html}"
        );
    }

    #[test]
    fn truncate_for_display_passes_through_small_payloads() {
        let small = "x".repeat(128);
        assert_eq!(truncate_for_display(small.clone(), Lang::En), small);
    }

    #[test]
    fn truncate_for_display_caps_oversized_payloads_with_footer() {
        let huge = "x".repeat(TOOL_CALL_RENDER_CAP * 4);
        let out = truncate_for_display(huge.clone(), Lang::En);
        assert!(
            out.len() < huge.len() / 2,
            "expected significant truncation"
        );
        assert!(
            out.contains("truncated for display"),
            "expected footer note, got: {}",
            &out[out.len().saturating_sub(200)..]
        );
        assert!(
            out.contains(&huge.len().to_string()),
            "footer should report original byte count"
        );
    }

    #[test]
    fn truncate_for_display_doesnt_split_utf8() {
        // Build a payload that crosses the cap with multi-byte
        // chars so a naive byte-slice would corrupt the last char.
        let prefix = "x".repeat(TOOL_CALL_RENDER_CAP - 1);
        let payload = format!("{prefix}\u{1F600}\u{1F600}");
        let out = truncate_for_display(payload, Lang::En);
        // If we sliced mid-codepoint, this would panic.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn render_markdown_highlights_fenced_rust() {
        let md = "```rust\nfn main() { println!(\"hi\"); }\n```";
        let out = render_markdown(md);
        // lumis emits inline-styled spans for highlighted tokens.
        assert!(
            out.contains("<span style="),
            "expected lumis spans in output, got:\n{out}"
        );
        // Multi-theme mode wraps every colour in `light-dark(<day>,
        // <night>)` so the browser can switch theme without a
        // re-render.
        assert!(
            out.contains("light-dark("),
            "expected `light-dark()` styles for theme switching: {out}"
        );
        // The post-pass replaces the markdown wrapper with lumis's
        // own `<pre class="lumis lumis-themes …"><code …>`.
        assert!(
            out.contains(r#"class="lumis lumis-themes"#),
            "missing lumis multi-themes <pre> wrapper: {out}"
        );
        assert!(
            out.contains(">println<"),
            "expected `println` as its own token: {out}"
        );
    }

    #[test]
    fn render_markdown_strips_lumis_pre_inline_style() {
        let md = "```rust\nfn main() {}\n```";
        let out = render_markdown(md);
        let pre_open = out
            .split_once("</pre>")
            .map(|(head, _)| head)
            .unwrap_or(&out);
        let first_pre = pre_open
            .split_once('>')
            .map(|(open, _)| open)
            .unwrap_or(pre_open);
        assert!(
            !first_pre.contains(" style="),
            "lumis <pre> kept its inline style attr (theme bg leaks): {first_pre}"
        );
        assert!(out.contains("<span style="));
    }

    #[test]
    fn render_markdown_passes_through_unknown_language() {
        let md = "```neverheardofit\nblah blah\n```";
        let out = render_markdown(md);
        assert!(!out.contains("<span style="));
        assert!(out.contains("<code class=\"language-neverheardofit\">"));
        assert!(out.contains("blah blah"));
    }

    #[test]
    fn render_markdown_leaves_plain_text_alone() {
        let out = render_markdown("just some **bold** text");
        assert!(out.contains("<strong>bold</strong>"));
        assert!(!out.contains("<pre>"));
    }

    #[test]
    fn render_markdown_never_emits_img_tags() {
        // The model echoes/hallucinates image links pointing at relative
        // or placeholder URLs; rendered as live <img src> they resolve
        // against the /chat/<id> page and 404/429-flood. Every one must
        // degrade to text/link, never a fetched <img>.
        for md in [
            "![preview](preview_url)",
            "![](image_url)",
            "![letter](5c858cd7-12b3-439e-9b31-c2cef4b65116/letter.png)",
            "see ![the chart](./png_url) here",
        ] {
            let out = render_markdown(md);
            assert!(
                !out.contains("<img"),
                "markdown image leaked a live <img> for {md:?}: {out}"
            );
        }
        // Real attachments don't come through markdown — they're spliced
        // as [gw-attachment …] markers and rendered by render_attachment,
        // so disabling the construct can't break a legitimate image.
        assert!(render_markdown("plain **text**").contains("<strong>text</strong>"));
    }

    #[test]
    fn render_markdown_normalises_lang_aliases() {
        let md = "```py\nprint('hi')\n```";
        let out = render_markdown(md);
        assert!(
            out.contains("<span style="),
            "py alias should have routed to python: {out}"
        );
    }

    #[test]
    fn assistant_segments_fast_path_returns_one_prose_block() {
        let segs = assistant_segments("plain text with **bold**");
        assert_eq!(segs.len(), 1);
        assert!(
            matches!(&segs[0], AssistantSegment::Prose(s) if s.contains("<strong>bold</strong>"))
        );
    }

    #[test]
    fn assistant_segments_splices_uploaded_attachment() {
        let marker = crate::attachments::marker_line(
            "chart.png",
            "image/png",
            "https://example.invalid/x.png",
            42,
        );
        let body = format!(
            "Here is the chart you asked for:\n\n{marker}\n\nLet me know if you want adjustments."
        );
        let segs = assistant_segments(&body);
        // Three segments: prose, attachment, prose. Each prose chunk
        // gets its own markdown pass so links/bold/etc. still work
        // around the attachment.
        assert_eq!(segs.len(), 3);
        assert!(matches!(&segs[0], AssistantSegment::Prose(s) if s.contains("Here is the chart")));
        match &segs[1] {
            AssistantSegment::Attachment(a) => {
                assert_eq!(a.filename, "chart.png");
                assert!(a.is_image());
            }
            _ => panic!("expected attachment in middle slot"),
        }
        assert!(matches!(&segs[2], AssistantSegment::Prose(s) if s.contains("adjustments")));
    }

    fn media_marker(filename: &str, mime: &str) -> String {
        crate::attachments::marker_line(
            filename,
            mime,
            &format!("/chat/attachment/t0/{filename}"),
            10,
        )
    }

    #[test]
    fn edit_form_carries_attachment_affordances() {
        // The inline edit form must accept pasted/dropped/picked files just
        // like the main composer: multipart enctype, the paste handler, a
        // named file input, and a chip strip. Regression guard for the
        // "can't paste an image while editing" bug.
        let turn = conv_turn(0, TurnRole::User, "hello").turn;
        let html = render_user_turn(&turn, Some("/chat"), Lang::En).to_string();
        assert!(
            html.contains("multipart/form-data"),
            "edit form must be multipart: {html}"
        );
        assert!(
            html.contains("window.chatActions.editPaste"),
            "edit form must wire the paste handler: {html}"
        );
        assert!(
            html.contains(r#"name="attachment""#),
            "edit form needs the attachment file input: {html}"
        );
        assert!(
            html.contains("chat-msg__edit-chips"),
            "edit form needs the chip strip hook: {html}"
        );
    }

    #[test]
    fn owner_gets_per_attachment_remove_control() {
        // A user viewing their own message can remove an attachment; the ×
        // posts to the per-attachment endpoint the router serves.
        let content = format!("look\n\n{}\n\n", media_marker("pic.png", "image/png"));
        let turn = conv_turn(0, TurnRole::User, &content).turn;
        let html = render_user_turn(&turn, Some("/chat"), Lang::En).to_string();
        assert!(
            html.contains("chat-msg__attachment-remove"),
            "remove control expected: {html}"
        );
        assert!(
            html.contains("/chat/s1/turns/t0/attachment/pic.png/remove"),
            "remove must post to the per-attachment endpoint: {html}"
        );
        assert!(
            html.contains("@post("),
            "removal uses datastar @post: {html}"
        );
    }

    #[test]
    fn readonly_view_has_no_remove_control() {
        // Shared / read-only view (no actions) must not offer removal.
        let content = format!("look\n\n{}\n\n", media_marker("pic.png", "image/png"));
        let turn = conv_turn(0, TurnRole::User, &content).turn;
        let html = render_user_turn(&turn, None, Lang::En).to_string();
        assert!(
            !html.contains("chat-msg__attachment-remove"),
            "no remove control for read-only viewers: {html}"
        );
    }

    #[test]
    fn generated_image_on_assistant_turn_is_removable() {
        // Removal also covers model-produced files (e.g. generate_image),
        // which live on the assistant turn's `content`.
        let content = format!("done\n\n{}\n\n", media_marker("gen.png", "image/png"));
        let tw = conv_turn(0, TurnRole::Assistant, &content);
        let html = render_assistant_turn(&tw, Some("/chat"), Lang::En).to_string();
        assert!(
            html.contains("/chat/s1/turns/t0/attachment/gen.png/remove"),
            "assistant-generated attachment must be removable: {html}"
        );
    }

    #[test]
    fn multiple_media_render_as_numbered_gallery() {
        // Three generated images in one reply must lay out side by side
        // (one `chat-media-gallery`) with per-kind "Image N" captions so the
        // user can reference them ("turn the 2nd image into a video").
        let content = format!(
            "Here you go:\n\n{}\n\n{}\n\n{}\n\n",
            media_marker("a.png", "image/png"),
            media_marker("b.png", "image/png"),
            media_marker("c.png", "image/png"),
        );
        let tw = conv_turn(0, TurnRole::Assistant, &content);
        let html = render_assistant_turn(&tw, None, Lang::En).to_string();
        assert!(
            html.matches("chat-media-gallery").count() == 1,
            "consecutive media collapse into exactly one gallery: {html}"
        );
        for label in ["Image 1", "Image 2", "Image 3"] {
            assert!(html.contains(label), "missing caption {label}: {html}");
        }
    }

    #[test]
    fn single_media_stays_inline_without_label() {
        // A lone image must render exactly as before — no gallery wrapper,
        // no "Image 1" caption — so single-generation replies stay clean.
        let content = format!(
            "One image:\n\n{}\n\n",
            media_marker("only.png", "image/png")
        );
        let tw = conv_turn(0, TurnRole::Assistant, &content);
        let html = render_assistant_turn(&tw, None, Lang::En).to_string();
        assert!(
            !html.contains("chat-media-gallery"),
            "a lone image must not be grouped into a gallery: {html}"
        );
        assert!(
            !html.contains("chat-media__label"),
            "a lone image must not be captioned: {html}"
        );
        assert!(
            html.contains("chat-msg__attachment-image"),
            "the image itself still renders: {html}"
        );
    }

    #[test]
    fn mixed_media_are_numbered_per_kind() {
        // An image + a video (2 media) group into a gallery, each numbered
        // within its own kind so "the 2nd image" / "the video" map cleanly.
        let content = format!(
            "{}\n\n{}\n\n",
            media_marker("pic.png", "image/png"),
            media_marker("clip.mp4", "video/mp4"),
        );
        let tw = conv_turn(0, TurnRole::Assistant, &content);
        let html = render_assistant_turn(&tw, None, Lang::En).to_string();
        assert!(html.contains("chat-media-gallery"), "2 media group: {html}");
        assert!(html.contains("Image 1"), "image labelled per kind: {html}");
        assert!(html.contains("Video 1"), "video labelled per kind: {html}");
    }

    #[test]
    fn render_attachment_keeps_proxy_url_when_marker_turn_differs() {
        let pdf = crate::attachments::ParsedAttachment {
            filename: "letter.pdf".into(),
            mime: "application/pdf".into(),
            url: "/chat/attachment/turn-A/letter.pdf".into(),
            size: 19600,
            link: None,
        };
        let rendered = render_attachment(&pdf, None, Lang::En).to_string();
        assert!(
            rendered.contains("/chat/attachment/turn-A/letter.pdf"),
            "expected the real download link: {rendered}"
        );
        assert!(
            !rendered.contains("unavailable"),
            "a valid session attachment should not be a placeholder: {rendered}"
        );
        assert!(
            rendered.contains("letter.pdf"),
            "filename should still be shown: {rendered}"
        );
        let png = crate::attachments::ParsedAttachment {
            filename: "preview.png".into(),
            mime: "image/png".into(),
            url: "/chat/attachment/turn-A/preview.png".into(),
            size: 1000,
            link: None,
        };
        let rendered_img = render_attachment(&png, None, Lang::En).to_string();
        assert!(
            rendered_img.contains("<img"),
            "a valid session image should be rendered inline: {rendered_img}"
        );
    }

    #[test]
    fn render_attachment_uses_native_players_for_audio_and_video() {
        let video = crate::attachments::ParsedAttachment {
            filename: "clip.mp4".into(),
            mime: "video/mp4".into(),
            url: "/chat/attachment/turn-A/clip.mp4".into(),
            size: 209_000,
            link: None,
        };
        let audio = crate::attachments::ParsedAttachment {
            filename: "song.mp3".into(),
            mime: "audio/mpeg".into(),
            url: "/chat/attachment/turn-A/song.mp3".into(),
            size: 2_400_000,
            link: None,
        };

        let video_html = render_attachment(&video, None, Lang::En).to_string();
        let audio_html = render_attachment(&audio, None, Lang::En).to_string();

        assert!(
            video_html.contains("<video"),
            "video player missing: {video_html}"
        );
        assert!(
            video_html.contains("controls"),
            "video controls missing: {video_html}"
        );
        assert!(
            video_html.contains("clip.mp4"),
            "video filename missing: {video_html}"
        );
        assert!(
            audio_html.contains("<audio"),
            "audio player missing: {audio_html}"
        );
        assert!(
            audio_html.contains("controls"),
            "audio controls missing: {audio_html}"
        );
        assert!(
            audio_html.contains("song.mp3"),
            "audio filename missing: {audio_html}"
        );
    }

    #[test]
    fn html_unescape_decodes_markdown_entity_set() {
        assert_eq!(
            html_unescape("if x &lt; 5 &amp;&amp; y &gt; 0"),
            "if x < 5 && y > 0"
        );
        assert_eq!(html_unescape("&quot;hello&quot;"), "\"hello\"");
        assert_eq!(html_unescape("don&#39;t"), "don't");
    }
}
