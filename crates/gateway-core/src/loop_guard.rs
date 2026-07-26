// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Degenerate-repetition detector for streamed model output.
//!
//! Reasoning models occasionally collapse into emitting the same short
//! phrase forever — "I'll send it. I'll send it. …" thousands of times —
//! until they hit the token ceiling. That's minutes of generation that
//! end in an empty answer (we saw a 27-minute turn do exactly this).
//!
//! [`LoopGuard`] watches the streamed text and trips when the recent tail
//! is a short unit repeated many times. It is **purely repetition-based,
//! never length- or time-based**: a long but *progressing* generation (a
//! legitimately big answer on the `/v1` API) is never aborted — only one
//! that has degenerated into a tight repeat. That distinction is why the
//! same guard is safe on both the chat path (which also caps tokens/time)
//! and the API path (where long requests must be allowed to run).
//!
//! Feed it the streamed deltas of one channel; the loop usually lives in
//! the reasoning channel, so callers run one guard for `content` and one
//! for `reasoning`.
//!
//! Detection (two complementary signals over the last [`TAIL`] bytes,
//! re-run every [`CHECK_EVERY`] bytes):
//!
//!  1. **Exact period** — the smallest period `p` of the window via the KMP
//!     failure function (O(TAIL)); a loop when `p <= MAX_PERIOD` over
//!     `>= MIN_REPEATS` repeats. Catches verbatim repeats ("I'll send it."
//!     ×1000, a single-char flood).
//!  2. **Repeated line** — a *template* loop where a fixed skeleton recurs
//!     but a slot varies each cycle ("Okay. / I will output. / One detail:
//!     \"<word>\".") has no short exact period, so (1) misses it. But its
//!     skeleton *lines* recur verbatim: if one non-trivial line
//!     (>= [`MIN_REPEAT_LINE_LEN`] bytes) appears >= [`MIN_LINE_REPEATS`]
//!     times in the window, that's a loop. Still purely repetition-based —
//!     a progressing answer has distinct lines and never trips.

/// Shown to the user (chat) and returned to API callers when the guard
/// stops a degenerate repetition. Kept here so every stream path words it
/// the same way.
pub const LOOP_MESSAGE: &str =
    "The response was stopped because the model started repeating itself (loop detected).";

/// Bytes of recent output we inspect. A loop trips once this much pure
/// repetition has accumulated — generous enough that a real loop is caught
/// in seconds, far short of the minutes an unbounded one would burn.
const TAIL: usize = 6144;
/// Longest repeating unit we recognise. A 14-byte phrase, a 1-byte
/// character, a 400-byte sentence — all caught; only repeats of units
/// longer than this slip through.
const MAX_PERIOD: usize = 512;
/// The unit must repeat at least this many times across the window. With
/// `TAIL`/`MAX_PERIOD` this is the binding floor for long units; short
/// units repeat far more than this before the window fills.
const MIN_REPEATS: usize = 12;
/// Re-scan only once this many new bytes have arrived, to bound CPU on a
/// long healthy stream (the scan is O(TAIL); this makes it amortized O(1)
/// per byte).
const CHECK_EVERY: usize = 512;
/// Shortest line (trimmed) the repeated-line signal will consider. Keeps
/// one-or-two-char structural lines a real answer legitimately repeats
/// (`}`, `);`, `- `) from ever tripping it; only a substantive recurring
/// line counts.
const MIN_REPEAT_LINE_LEN: usize = 8;
/// How many times one such line must recur in the window to be a loop.
/// Matches [`MIN_REPEATS`] — a dozen verbatim repeats of a real sentence in
/// 6 KB is degeneration, not prose.
const MIN_LINE_REPEATS: usize = 12;

/// Watches one streamed text channel for a degenerate repetition loop.
/// Sticky: once it trips, every later [`push`](LoopGuard::push) keeps
/// returning `true`.
#[derive(Default)]
pub struct LoopGuard {
    tail: Vec<u8>,
    since_check: usize,
    fail: Vec<usize>,
    tripped: bool,
}

impl LoopGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a streamed delta. Returns `true` the first time (and every
    /// time after) the tail has degenerated into a tight repeat.
    pub fn push(&mut self, delta: &str) -> bool {
        if self.tripped {
            return true;
        }
        if delta.is_empty() {
            return false;
        }
        self.tail.extend_from_slice(delta.as_bytes());
        if self.tail.len() > TAIL {
            let cut = self.tail.len() - TAIL;
            self.tail.drain(..cut);
        }
        self.since_check += delta.len();
        if self.tail.len() < TAIL || self.since_check < CHECK_EVERY {
            return false;
        }
        self.since_check = 0;
        if self.is_degenerate() {
            self.tripped = true;
        }
        self.tripped
    }

    /// A loop on either signal: a short exact period, or a non-trivial line
    /// repeated many times (the template-with-varying-slot case).
    fn is_degenerate(&mut self) -> bool {
        self.has_short_period() || self.has_repeated_line()
    }

    /// Smallest period of the current tail (KMP failure function) — is it
    /// short enough, repeated often enough, to be a loop?
    fn has_short_period(&mut self) -> bool {
        let l = self.tail.len();
        if l < TAIL {
            return false;
        }
        self.fail.clear();
        self.fail.resize(l, 0);
        // Disjoint borrows of two distinct fields — the byte window (read)
        // and the failure-function scratch (write).
        let s = &self.tail;
        let fail = &mut self.fail;
        let mut k = 0usize;
        for i in 1..l {
            while k > 0 && s[i] != s[k] {
                k = fail[k - 1];
            }
            if s[i] == s[k] {
                k += 1;
            }
            fail[i] = k;
        }
        // `l - border` is the smallest p with s[i] == s[i + p] for all i
        // (weak periodicity — handles a unit that doesn't divide the
        // window cleanly because the cut fell mid-unit).
        let period = l - fail[l - 1];
        period <= MAX_PERIOD && l / period >= MIN_REPEATS
    }

    /// Does one trimmed, non-trivial line recur `>= MIN_LINE_REPEATS` times
    /// in the window? Catches a fixed skeleton with a varying slot, whose
    /// skeleton lines repeat verbatim even though the whole window has no
    /// short exact period. O(TAIL) over byte-slice lines (no allocation,
    /// robust to a window that starts mid-UTF-8 after a drain).
    fn has_repeated_line(&self) -> bool {
        use std::collections::HashMap;
        let mut counts: HashMap<&[u8], usize> = HashMap::new();
        for line in self.tail.split(|&b| b == b'\n') {
            let trimmed = line.trim_ascii();
            if trimmed.len() < MIN_REPEAT_LINE_LEN {
                continue;
            }
            let c = counts.entry(trimmed).or_insert(0);
            *c += 1;
            if *c >= MIN_LINE_REPEATS {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Push a string in small chunks (as it would arrive over SSE) and
    /// report whether the guard tripped by the end.
    fn feed_in_chunks(text: &str, chunk: usize) -> bool {
        let mut g = LoopGuard::new();
        let bytes = text.as_bytes();
        let mut tripped = false;
        let mut i = 0;
        while i < bytes.len() {
            let end = (i + chunk).min(bytes.len());
            // Split on a char boundary so &str slicing is valid.
            let mut e = end;
            while e < bytes.len() && !text.is_char_boundary(e) {
                e += 1;
            }
            tripped |= g.push(&text[i..e]);
            i = e;
        }
        tripped
    }

    #[test]
    fn trips_on_repeated_phrase() {
        let loopy = "I'll send it. ".repeat(2000);
        assert!(feed_in_chunks(&loopy, 13), "should catch a repeated phrase");
    }

    #[test]
    fn trips_on_single_char_flood() {
        let loopy = "a".repeat(20_000);
        assert!(
            feed_in_chunks(&loopy, 7),
            "should catch a single-char flood"
        );
    }

    #[test]
    fn trips_on_repeated_long_sentence() {
        // ~120-byte unit, well under MAX_PERIOD, repeated to fill the window.
        let unit = "The quick brown fox jumps over the lazy dog while the sun sets slowly behind the distant rolling green hills today.\n";
        assert!(
            feed_in_chunks(&unit.repeat(300), 64),
            "a repeated long sentence is still a loop"
        );
    }

    #[test]
    fn trips_on_template_loop_with_a_varying_slot() {
        // The real-world failure: a fixed skeleton — "Okay. / I will output.
        // / One detail: \"<word>\"." — with ONE word changing every cycle.
        // There's no short *exact* period (the word varies), so the KMP
        // period detector misses it; but the skeleton lines recur verbatim.
        let words = [
            "Night",
            "Week",
            "Month",
            "Year",
            "Decade",
            "Century",
            "Millennium",
            "Era",
            "Age",
            "Epoch",
            "Period",
            "Time",
            "Moment",
            "Instant",
            "Hour",
            "Minute",
        ];
        let loopy: String = (0..400)
            .map(|i| {
                format!(
                    "Okay.\n\nI will output.\n\nOne detail: \"{}\".\n\n",
                    words[i % words.len()]
                )
            })
            .collect();
        assert!(
            feed_in_chunks(&loopy, 17),
            "a template loop with a varying slot must trip the guard"
        );
    }

    #[test]
    fn does_not_trip_on_distinct_lines() {
        // Many short lines, each distinct — a real structured answer (steps,
        // list items). No line recurs, so the line detector must stay quiet.
        let text: String = (0..500)
            .map(|i| format!("Step {i}: do the distinct thing number {i} carefully.\n"))
            .collect();
        assert!(text.len() > TAIL * 2, "fixture must exceed the window");
        assert!(
            !feed_in_chunks(&text, 40),
            "distinct lines must not be mistaken for a loop"
        );
    }

    #[test]
    fn does_not_trip_on_long_progressing_text() {
        // Non-repetitive text far longer than the window: the integer
        // sequence keeps changing, so the smallest period stays large.
        let varied: String = (0..4000)
            .map(|i| format!("item-{i} carries a distinct value {}; ", i * 7 + 3))
            .collect();
        assert!(
            varied.len() > TAIL * 2,
            "test fixture must exceed the window"
        );
        assert!(
            !feed_in_chunks(&varied, 50),
            "a long but progressing answer must not be aborted"
        );
    }

    #[test]
    fn does_not_trip_below_thresholds() {
        // A short burst of repetition (a few dozen bytes) is normal prose
        // ("no no no", "ha ha ha") and must not trip.
        assert!(!feed_in_chunks(&"ha ".repeat(8), 3));
        assert!(!feed_in_chunks(&"no ".repeat(20), 10));
    }

    #[test]
    fn trip_is_sticky() {
        let mut g = LoopGuard::new();
        assert!(g.push(&"loop ".repeat(4000)));
        // Even an empty / fresh push after tripping stays tripped.
        assert!(g.push("anything"));
        assert!(g.push(""));
    }
}
