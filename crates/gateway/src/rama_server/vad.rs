// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Pre-transcription silence trimming via neural VAD.
//!
//! Whisper is trained to always produce text — feeding it silence
//! reliably triggers hallucinations like "thanks for watching" or
//! repeated phrases from training data. Local-Whisper apps
//! (openwhispr, MacWhisper, …) all converge on running a VAD pass that
//! strips leading/trailing silence — *and* clips long internal pauses
//! — before audio reaches the model.
//!
//! The browser uploads 16 kHz mono 16-bit WAV (see `assets/app.js` —
//! AudioWorklet captures raw PCM and wraps it with a 44-byte WAV header
//! before posting). We:
//!
//!   1. Parse the WAV header (PCM, 16 kHz, mono, 16-bit — anything else
//!      we leave alone, since we'd be guessing).
//!   2. Run `earshot::Detector` over 256-sample (16 ms) frames to get a
//!      voice-probability per frame.
//!   3. Find maximal runs of speech frames (hysteresis filters out
//!      single-frame spikes from coughs/keyboard taps).
//!   4. Merge runs separated by a short gap (so a 100 ms breath in the
//!      middle of a sentence doesn't fragment the region).
//!   5. Pad each surviving run ~160 ms on either side so word onsets
//!      and offsets aren't clipped.
//!   6. Stitch the regions back together: keep short inter-region
//!      silences verbatim (Whisper uses them for sentence
//!      segmentation), but clip long silences to a fixed ~250 ms gap.
//!      Long stretches of silence inside the recording are also
//!      hallucination triggers — that's the change from a pure
//!      "trim ends only" approach.

use earshot::Detector;
use rama::bytes::Bytes;

/// Frame size earshot requires: 256 samples at 16 kHz = 16 ms.
const FRAME_SAMPLES: usize = 256;

/// Sample rate earshot is trained for. The browser worklet renders to
/// this rate; any other rate is treated as "not for us, pass through."
const SAMPLE_RATE: u32 = 16_000;

/// Probability threshold above which a frame counts as speech.
/// 0.5 is earshot's documented default.
const SPEECH_THRESHOLD: f32 = 0.5;

/// Number of consecutive frames above the threshold required to commit
/// to a speech start/end. Single-frame triggers fire on coughs and
/// keyboard taps; three consecutive frames is ~48 ms of sustained
/// energy in the speech-likelihood model — short enough to catch
/// staccato consonants, long enough to reject noise spikes.
const HYSTERESIS_FRAMES: usize = 3;

/// Frames of padding kept on either side of every speech region.
/// 10 frames × 16 ms = 160 ms — enough to retain a leading consonant
/// or trailing fricative without leaking obvious noise into the
/// transcript.
const PAD_FRAMES: usize = 10;

/// Two raw speech runs within this many frames of each other get
/// merged into one region (after which we apply padding once around
/// the merged whole). ~320 ms — long enough to bridge an in-sentence
/// breath; short enough that two separate utterances stay separate.
const MERGE_GAP_FRAMES: usize = 20;

/// Largest inter-region silence we'll emit verbatim. Anything longer
/// would be a hallucination trigger in Whisper — `~480 ms` is the
/// "natural pause" upper bound; beyond that, get rid of it.
const MAX_GAP_FRAMES: usize = 30;

/// Replacement length for clipped long gaps. Whisper still wants to
/// see *some* silence between distinct utterances for sentence
/// segmentation — collapsing all gaps to zero produces a run-on
/// transcript with no punctuation. 16 frames × 16 ms = 256 ms.
const GAP_TARGET_FRAMES: usize = 16;

/// Result of a successful trim. The MIME type and filename are fixed
/// (we always produce a 16 kHz mono s16 WAV); the caller rewrites the
/// `Content-Disposition` filename in the multipart body.
pub struct Trimmed {
    pub bytes: Bytes,
    pub content_type: &'static str,
    pub filename: &'static str,
}

/// Trim leading/trailing silence and clip long internal pauses from a
/// 16 kHz mono 16-bit WAV. Returns `None` if the input isn't in that
/// exact format, no speech was detected, or there's nothing to trim —
/// callers forward the original bytes in those cases.
pub fn trim_silence(audio: &[u8]) -> Option<Trimmed> {
    let pcm = parse_pcm16_mono_16k(audio)?;
    if pcm.len() < FRAME_SAMPLES {
        // Shorter than a single earshot frame — not worth invoking
        // the detector. Pure silence frames sit at the noise floor,
        // and earshot operates on whole frames anyway.
        return None;
    }

    let mut detector = Detector::default();
    let frame_count = pcm.len() / FRAME_SAMPLES;
    let mut speech = Vec::with_capacity(frame_count);
    for i in 0..frame_count {
        let start = i * FRAME_SAMPLES;
        let frame = &pcm[start..start + FRAME_SAMPLES];
        let score = detector.predict_i16(frame);
        speech.push(score >= SPEECH_THRESHOLD);
    }

    let raw = speech_runs(&speech, HYSTERESIS_FRAMES);
    if raw.is_empty() {
        // Pure silence (or close enough). Don't ship a re-encoded
        // empty WAV — let the caller forward the original so the
        // upstream sees an actual recording it can return whatever
        // best-effort transcript it likes.
        return None;
    }
    let merged = merge_close_runs(raw, MERGE_GAP_FRAMES);
    let padded = pad_runs(&merged, PAD_FRAMES, frame_count - 1);

    if covers_entire_input(&padded, frame_count) {
        // Speech is everywhere — trimming would be a no-op. Skip the
        // re-encode to save allocations + bandwidth and let the
        // caller forward original bytes.
        return None;
    }

    let trimmed = splice_regions(&pcm, &padded);
    let wav = write_pcm16_mono_16k_wav(&trimmed);
    Some(Trimmed {
        bytes: Bytes::from(wav),
        content_type: "audio/wav",
        filename: "recording.wav",
    })
}

/// Maximal contiguous runs of `true` in `mask`, dropping runs shorter
/// than `min_run` (which filters single-frame false positives).
/// Returned ranges are `(start, end_inclusive)` frame indices.
fn speech_runs(mask: &[bool], min_run: usize) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut i = 0;
    while i < mask.len() {
        if mask[i] {
            let start = i;
            while i < mask.len() && mask[i] {
                i += 1;
            }
            let end = i - 1;
            if end + 1 - start >= min_run {
                runs.push((start, end));
            }
        } else {
            i += 1;
        }
    }
    runs
}

/// Merge runs whose start-to-prev-end gap is `<= max_gap` frames.
/// Used to bridge brief in-sentence breaths so a 100 ms pause doesn't
/// fragment a region into two.
fn merge_close_runs(runs: Vec<(usize, usize)>, max_gap: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(runs.len());
    for (s, e) in runs {
        if let Some(last) = out.last_mut()
            && s.saturating_sub(last.1 + 1) <= max_gap
        {
            last.1 = e;
            continue;
        }
        out.push((s, e));
    }
    out
}

/// Expand each region by `pad` frames on either side, clipping to
/// `[0, max_frame]`. Because `merge_close_runs` already collapsed any
/// runs whose gap was ≤ `2 * pad`, the padded regions never overlap.
fn pad_runs(runs: &[(usize, usize)], pad: usize, max_frame: usize) -> Vec<(usize, usize)> {
    runs.iter()
        .map(|&(s, e)| (s.saturating_sub(pad), (e + pad).min(max_frame)))
        .collect()
}

fn covers_entire_input(regions: &[(usize, usize)], frame_count: usize) -> bool {
    regions.len() == 1 && regions[0].0 == 0 && regions[0].1 + 1 == frame_count
}

/// Stitch padded regions back into a single sample buffer. Between
/// each pair we emit either the original silence (if short) or a
/// fixed `GAP_TARGET_FRAMES` of zeros (if long). The latter is the
/// "no hallucinations on long pauses, but still sentence-segmentation
/// cues" compromise.
fn splice_regions(pcm: &[i16], regions: &[(usize, usize)]) -> Vec<i16> {
    let estimated_len: usize = regions
        .iter()
        .map(|(s, e)| (e + 1 - s) * FRAME_SAMPLES)
        .sum::<usize>()
        + regions.len().saturating_sub(1) * GAP_TARGET_FRAMES * FRAME_SAMPLES;
    let mut out: Vec<i16> = Vec::with_capacity(estimated_len);
    for (i, &(s, e)) in regions.iter().enumerate() {
        if i > 0 {
            let prev_end = regions[i - 1].1;
            let gap = s - prev_end - 1;
            if gap <= MAX_GAP_FRAMES {
                let from = (prev_end + 1) * FRAME_SAMPLES;
                let to = s * FRAME_SAMPLES;
                out.extend_from_slice(&pcm[from..to]);
            } else {
                out.extend(std::iter::repeat_n(0i16, GAP_TARGET_FRAMES * FRAME_SAMPLES));
            }
        }
        let from = s * FRAME_SAMPLES;
        let to = (e + 1) * FRAME_SAMPLES;
        out.extend_from_slice(&pcm[from..to]);
    }
    out
}

/// Duration in seconds of a 16 kHz / mono / PCM-16 WAV. Returns
/// `None` for anything outside that format (same conservative parser
/// `trim_silence` uses, so a refusal here means we wouldn't have
/// trimmed it either). Used by the transcription proxy to reject
/// sub-threshold recordings *before* they hit voxtral, which embeds
/// audio at 25 tokens/s and emits a `Realtime model received empty
/// multimodal embeddings for 1 input tokens` warning + a wedged
/// decode loop when handed anything under ~40 ms.
pub fn pcm16_mono_16k_duration_seconds(bytes: &[u8]) -> Option<f64> {
    let samples = parse_pcm16_mono_16k(bytes)?;
    Some(samples.len() as f64 / f64::from(SAMPLE_RATE))
}

/// Validate the incoming WAV and return its sample buffer. We only
/// accept the exact format the browser worklet produces — anything
/// else is left untrimmed (better than guessing wrong and silently
/// corrupting upstream audio).
///
/// Format check: RIFF/WAVE, format chunk says PCM (1) / mono /
/// 16 kHz / 16-bit, and at least one data chunk.
fn parse_pcm16_mono_16k(bytes: &[u8]) -> Option<Vec<i16>> {
    // Minimum: 12-byte RIFF header + 8 + 16 fmt chunk + 8 data chunk = 44.
    if bytes.len() < 44 {
        return None;
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }

    // Walk RIFF chunks looking for `fmt ` and `data`. WAV files in the
    // wild interleave optional chunks (`LIST`, `JUNK`, `fact`) between
    // the spec-mandated two; we skip anything we don't recognise.
    let mut cursor = 12;
    let mut format_ok = false;
    let mut data: Option<&[u8]> = None;
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().ok()?) as usize;
        let body_start = cursor + 8;
        let body_end = body_start.checked_add(size)?;
        if body_end > bytes.len() {
            return None;
        }
        match id {
            b"fmt " => {
                // PCM fmt chunk: at least 16 bytes.
                if size < 16 {
                    return None;
                }
                let body = &bytes[body_start..body_end];
                let audio_format = u16::from_le_bytes(body[0..2].try_into().ok()?);
                let channels = u16::from_le_bytes(body[2..4].try_into().ok()?);
                let rate = u32::from_le_bytes(body[4..8].try_into().ok()?);
                let bits = u16::from_le_bytes(body[14..16].try_into().ok()?);
                if audio_format != 1 || channels != 1 || rate != SAMPLE_RATE || bits != 16 {
                    return None;
                }
                format_ok = true;
            }
            b"data" => {
                data = Some(&bytes[body_start..body_end]);
                break;
            }
            _ => {}
        }
        // Chunks are word-aligned: odd sizes have a trailing pad byte.
        cursor = body_end + (size & 1);
    }

    if !format_ok {
        return None;
    }
    let data = data?;
    if data.len() % 2 != 0 {
        return None;
    }
    let mut samples = Vec::with_capacity(data.len() / 2);
    for chunk in data.chunks_exact(2) {
        samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    Some(samples)
}

/// Pack a PCM s16 sample buffer into a 16 kHz mono WAV with the
/// standard 44-byte canonical header. Reusing the same constant
/// header layout as the browser side means a future regression can
/// be diffed byte-for-byte.
fn write_pcm16_mono_16k_wav(samples: &[i16]) -> Vec<u8> {
    let bytes_per_sample: u16 = 2;
    let channels: u16 = 1;
    let byte_rate = SAMPLE_RATE * u32::from(channels) * u32::from(bytes_per_sample);
    let block_align = channels * bytes_per_sample;
    let data_size = (samples.len() * usize::from(bytes_per_sample)) as u32;

    let mut out = Vec::with_capacity(44 + data_size as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_size).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&(bytes_per_sample * 8).to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_wav(samples: &[i16]) -> Vec<u8> {
        write_pcm16_mono_16k_wav(samples)
    }

    #[test]
    fn parses_round_trip() {
        let samples: Vec<i16> = (0..1000).map(|i| (i % 256) as i16).collect();
        let wav = synth_wav(&samples);
        let parsed = parse_pcm16_mono_16k(&wav).expect("valid wav");
        assert_eq!(parsed, samples);
    }

    #[test]
    fn rejects_wrong_sample_rate() {
        // Manually build a WAV with 48 kHz — should be rejected.
        let mut wav = synth_wav(&[0, 0, 0, 0]);
        // Sample rate field lives at offset 24..28.
        wav[24..28].copy_from_slice(&48_000u32.to_le_bytes());
        // byte rate at 28..32 must also be patched to keep RIFF
        // internally consistent — but we test that parse rejects on
        // the rate alone, before checking byte rate. Either way the
        // function returns None.
        assert!(parse_pcm16_mono_16k(&wav).is_none());
    }

    #[test]
    fn pure_silence_returns_none() {
        // Earshot should never report > 0.5 on a buffer of zeros.
        let silence = vec![0i16; FRAME_SAMPLES * 50];
        let wav = synth_wav(&silence);
        assert!(trim_silence(&wav).is_none());
    }

    #[test]
    fn short_input_returns_none() {
        let short = vec![0i16; FRAME_SAMPLES - 1];
        let wav = synth_wav(&short);
        assert!(trim_silence(&wav).is_none());
    }

    #[test]
    fn non_wav_returns_none() {
        // The proxy hands us bytes from the browser; a non-WAV blob
        // (e.g. a stray opus-webm from an older client) should make
        // us bail and let the caller forward original bytes.
        assert!(trim_silence(b"not a wav file at all").is_none());
    }

    #[test]
    fn speech_runs_drops_short_spikes() {
        // F = false, T = true. min_run = 3 means we keep only runs
        // of length >= 3.
        let mask = vec![
            false, true, false, // single-frame spike — dropped
            false, true, true, true, false, // 3-frame run — kept
            true, true, // 2-frame run — dropped
            false, true, true, true, true, // 4-frame run — kept
        ];
        let runs = speech_runs(&mask, 3);
        assert_eq!(runs, vec![(4, 6), (11, 14)]);
    }

    #[test]
    fn merge_close_runs_bridges_short_gaps() {
        // Two runs separated by 2 frames; max_gap = 3 → merge.
        let runs = vec![(0, 5), (8, 12), (50, 55)];
        let merged = merge_close_runs(runs, 3);
        // (0,5) + (8,12) — gap is 8-5-1 = 2 ≤ 3 → merged into (0,12).
        // (50,55) — gap is 50-12-1 = 37 > 3 → kept separate.
        assert_eq!(merged, vec![(0, 12), (50, 55)]);
    }

    #[test]
    fn merge_close_runs_respects_boundary() {
        let runs = vec![(0, 5), (10, 15)];
        // Gap = 4; max_gap = 4 → still merged (`<=`).
        assert_eq!(merge_close_runs(runs.clone(), 4), vec![(0, 15)]);
        // Gap = 4; max_gap = 3 → kept separate.
        assert_eq!(merge_close_runs(runs, 3), vec![(0, 5), (10, 15)]);
    }

    #[test]
    fn pad_runs_clips_to_bounds() {
        let runs = vec![(5, 10), (50, 90)];
        let padded = pad_runs(&runs, 8, 95);
        // First: (5-8, 10+8) → clamp lower to 0 → (0, 18).
        // Second: (50-8, 90+8) → clamp upper to 95 → (42, 95).
        assert_eq!(padded, vec![(0, 18), (42, 95)]);
    }

    #[test]
    fn splice_clips_long_gaps_but_keeps_short_ones() {
        // Build a small PCM buffer where samples encode their frame
        // index — makes it easy to see which frames survived.
        let frame_count = 100;
        let pcm: Vec<i16> = (0..frame_count)
            .flat_map(|frame_idx| std::iter::repeat_n(frame_idx as i16, FRAME_SAMPLES))
            .collect();

        // Region A covers frames 5..=10; region B covers 40..=50.
        // Gap between them is 40-10-1 = 29 frames.
        // MAX_GAP_FRAMES = 30 → 29 ≤ 30 → keep verbatim.
        let regions = vec![(5usize, 10usize), (40usize, 50usize)];
        let spliced = splice_regions(&pcm, &regions);
        // Expected length: 6 + 29 + 11 = 46 frames worth of samples.
        assert_eq!(spliced.len(), 46 * FRAME_SAMPLES);
        // Spot-check that we kept the original silence frames (their
        // encoded value is their frame index).
        assert_eq!(spliced[0], 5);
        assert_eq!(spliced[6 * FRAME_SAMPLES], 11); // first gap frame
        assert_eq!(spliced[(6 + 29) * FRAME_SAMPLES], 40); // first frame of region B

        // Now make the gap longer than MAX_GAP_FRAMES: regions 5..=10
        // and 60..=70. Gap = 49 > 30 → clipped to GAP_TARGET_FRAMES
        // of zeros.
        let regions = vec![(5usize, 10usize), (60usize, 70usize)];
        let spliced = splice_regions(&pcm, &regions);
        assert_eq!(spliced.len(), (6 + GAP_TARGET_FRAMES + 11) * FRAME_SAMPLES);
        // The gap should be exactly zero-valued.
        let gap_start = 6 * FRAME_SAMPLES;
        let gap_end = gap_start + GAP_TARGET_FRAMES * FRAME_SAMPLES;
        assert!(spliced[gap_start..gap_end].iter().all(|&s| s == 0));
        // Region B's first sample should be right after the clip.
        assert_eq!(spliced[gap_end], 60);
    }
}
