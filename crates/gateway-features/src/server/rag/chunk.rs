// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Sliding-window chunker.
//!
//! Pure function: given a file's content (UTF-8 string) and the
//! collection's `chunk_size` + `chunk_overlap`, produce a sequence of
//! [`Chunk`]s — `chunk_size` characters with `chunk_overlap` characters
//! of carry-over between consecutive windows. Lines are 1-based and
//! computed from `\n` positions in the source so the tool can surface
//! `path:start-end` provenance even though chunking itself is
//! line-agnostic.
//!
//! Character-aware, not byte-aware: an emoji counts as one character so
//! windowing can never split a multi-byte sequence. This is a code-RAG
//! pipeline; the small constant cost of decoding to `char`s up front is
//! irrelevant against embedding-call latency.
//!
//! V1 deliberately does not understand language structure. Tree-sitter
//! chunking is on the roadmap; the carry-over here softens the
//! "function split across two chunks" failure mode well enough for
//! retrieval to recall both windows on a hit anywhere inside.

/// One chunk emitted by [`chunk_text`]. Line numbers are inclusive,
/// 1-based, and refer back to the source string the chunker was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Position of this chunk within its file, 0-based.
    pub chunk_index: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
}

/// Slide a `chunk_size`-character window across `text` with
/// `chunk_overlap` characters of overlap between adjacent windows.
///
/// Constraints:
///   * `chunk_size` must be > 0; we clamp to 1 to defuse a misconfigured
///     collection rather than panic.
///   * `chunk_overlap` is clamped to `chunk_size - 1` so each window
///     advances by at least one character (otherwise we'd loop forever).
///
/// Empty `text` → empty output. A single-window file (text shorter than
/// `chunk_size`) emits one chunk spanning the whole file.
pub fn chunk_text(text: &str, chunk_size: usize, chunk_overlap: usize) -> Vec<Chunk> {
    if text.is_empty() {
        return Vec::new();
    }
    let chunk_size = chunk_size.max(1);
    let overlap = chunk_overlap.min(chunk_size.saturating_sub(1));
    let advance = chunk_size - overlap;

    // Walk characters once and remember the byte offset of every char start
    // + a parallel array of "line at this char". A second pass slices that
    // up — keeps both content and line lookup O(text len) overall.
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut line_of_char: Vec<usize> = Vec::with_capacity(chars.len());
    let mut line = 1usize;
    for &(_, c) in &chars {
        line_of_char.push(line);
        if c == '\n' {
            line += 1;
        }
    }

    let total = chars.len();
    let mut out = Vec::new();
    let mut idx = 0usize;
    let mut start = 0usize;
    while start < total {
        let end = (start + chunk_size).min(total);
        let byte_start = chars[start].0;
        let byte_end = if end < total {
            chars[end].0
        } else {
            text.len()
        };
        let content = text[byte_start..byte_end].to_string();
        let start_line = line_of_char[start];
        let end_line = line_of_char[end - 1];
        out.push(Chunk {
            chunk_index: idx,
            start_line,
            end_line,
            content,
        });
        idx += 1;
        if end == total {
            break;
        }
        start += advance;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_emits_no_chunks() {
        assert!(chunk_text("", 100, 10).is_empty());
    }

    #[test]
    fn single_window_for_short_text() {
        let out = chunk_text("hello world", 100, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content, "hello world");
        assert_eq!(out[0].start_line, 1);
        assert_eq!(out[0].end_line, 1);
        assert_eq!(out[0].chunk_index, 0);
    }

    #[test]
    fn multi_window_uses_overlap_and_keeps_indices() {
        // "abcdefghij" with size=4, overlap=1 → advance=3 → windows
        // [abcd][defg][ghij]
        let out = chunk_text("abcdefghij", 4, 1);
        assert_eq!(
            out.iter().map(|c| c.content.as_str()).collect::<Vec<_>>(),
            vec!["abcd", "defg", "ghij"]
        );
        assert_eq!(
            out.iter().map(|c| c.chunk_index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn windows_terminate_when_overlap_equals_size_minus_one() {
        // overlap clamps to size-1 → advance = 1. Sequence:
        // [ab][bc][cd][de] for size=2, overlap=99 on "abcde".
        let out = chunk_text("abcde", 2, 99);
        assert_eq!(
            out.iter().map(|c| c.content.as_str()).collect::<Vec<_>>(),
            vec!["ab", "bc", "cd", "de"]
        );
    }

    #[test]
    fn zero_chunk_size_is_clamped_to_one_not_a_panic() {
        let out = chunk_text("ab", 0, 0);
        assert_eq!(
            out.iter().map(|c| c.content.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn line_numbers_track_newlines_within_chunks() {
        let text = "line1\nline2\nline3\nline4";
        let out = chunk_text(text, 10, 2);
        assert_eq!(out[0].start_line, 1);
        // First chunk "line1\nlin" ends inside line 2.
        assert_eq!(out[0].end_line, 2);
        // The final chunk must end on the last line.
        let last = out.last().unwrap();
        assert_eq!(last.end_line, 4);
    }

    #[test]
    fn multibyte_chars_do_not_split_within_a_window() {
        // 4 emoji × 4 bytes each = 16 bytes; 4 chars. chunk_size=2 → two
        // 2-char chunks; each must remain valid UTF-8.
        let text = "🌊🌊🌊🌊";
        let out = chunk_text(text, 2, 0);
        assert_eq!(out.len(), 2);
        assert!(out[0].content.chars().all(|c| c == '🌊'));
        assert!(out[1].content.chars().all(|c| c == '🌊'));
    }
}
