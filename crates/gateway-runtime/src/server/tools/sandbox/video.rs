// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `render_video` — assemble a video from a declarative JSON timeline.
//!
//! The point of the tool is that the timeline is **data**, not a shell command:
//! the model writes (or edits) a JSON project, usually as a `format: "json"`
//! canvas document, and rendering it is reproducible — same spec, same video.
//! That matters for ad production, where the next cut has to look exactly like
//! the last one with different words in it.
//!
//! ## Why not let the model write ffmpeg
//!
//! It can, via `run_in_sandbox`, and for one-off odd jobs that is the right
//! tool. But ffmpeg's `drawtext` needs commas, colons and quotes escaped inside
//! a `-filter_complex` that is itself inside a shell command, and every
//! generation is a fresh chance to get that wrong — at minutes per attempt once
//! a render is involved. Here the escaping problem is not solved but *avoided*:
//!
//! * overlay text never enters the filtergraph — each string is written to its
//!   own file and referenced with `drawtext=textfile=…`;
//! * the filtergraph never enters the shell — it is written to a file and
//!   passed with `-filter_complex_script`;
//! * input files are renamed to generated names (`in0.mp4`), so a filename from
//!   an attachment cannot reach a command line either;
//! * the only user-supplied values that *do* land in the graph are numbers and
//!   position expressions, and those go through [`validate_expr`], which
//!   permits nothing that could close an option or start a new filter.
//!
//! What is left in the generated script is therefore fixed tokens and numbers.
//!
//! ## Two passes
//!
//! `xfade` needs to know when to start the crossfade, which means knowing how
//! long the preceding clip is. Rather than assembling the graph inside the
//! sandbox (which would put string-building back into generated code), the tool
//! runs `ffprobe` first, reads the durations, and only then builds the render.
//! Both calls share one leased container, so `/work` — and the staged inputs —
//! persist between them.

use std::collections::HashMap;
use std::fmt::Write as _;

use super::*;

// ---------------------------------------------------------------------------
// Spec

/// One video project. Deliberately small: the shapes an ad needs (cuts,
/// crossfades, lower thirds, a logo, a music bed) rather than a general
/// compositor.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct VideoSpec {
    #[serde(default)]
    pub(crate) output: Output,
    /// Clips in playback order. At least one.
    pub(crate) clips: Vec<Clip>,
    #[serde(default)]
    pub(crate) overlays: Vec<Overlay>,
    /// Audio to lay under the video: one music bed, or several tracks (a bed
    /// plus a narration line per scene) each with its own `start`. Accepts a
    /// single object as well as a list.
    #[serde(default, deserialize_with = "one_or_many_tracks")]
    pub(crate) audio: Vec<AudioTrack>,
    /// Fade the finished video to black over this many seconds.
    #[serde(default)]
    pub(crate) fade_out: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Output {
    #[serde(default = "d_width")]
    pub(crate) width: u32,
    #[serde(default = "d_height")]
    pub(crate) height: u32,
    #[serde(default = "d_fps")]
    pub(crate) fps: f64,
    /// x264 constant-rate factor: lower is better and bigger. 20 is a good ad
    /// default; 28 is visibly soft.
    #[serde(default = "d_crf")]
    pub(crate) crf: u32,
}

fn d_width() -> u32 {
    1920
}
fn d_height() -> u32 {
    1080
}
fn d_fps() -> f64 {
    30.0
}
fn d_crf() -> u32 {
    20
}

impl Default for Output {
    fn default() -> Self {
        Self {
            width: d_width(),
            height: d_height(),
            fps: d_fps(),
            crf: d_crf(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Clip {
    /// Attachment id or staged filename of the source video.
    pub(crate) source: String,
    /// Trim: seconds into the source to start at.
    #[serde(default)]
    pub(crate) start: Option<f64>,
    /// Trim: seconds into the source to stop at. Must exceed `start`.
    #[serde(default)]
    pub(crate) end: Option<f64>,
    /// How this clip enters. Ignored on the first clip — there is nothing to
    /// transition from.
    #[serde(default)]
    pub(crate) transition: Option<Transition>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Transition {
    #[serde(default)]
    pub(crate) kind: TransitionKind,
    #[serde(default = "d_transition_dur")]
    pub(crate) duration: f64,
}

fn d_transition_dur() -> f64 {
    0.5
}

/// A safe subset of ffmpeg's `xfade` transitions — enumerated rather than
/// free text so an unknown name is a clear tool error instead of an ffmpeg one.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TransitionKind {
    #[default]
    Fade,
    Dissolve,
    WipeLeft,
    WipeRight,
    WipeUp,
    WipeDown,
    SlideLeft,
    SlideRight,
    SmoothLeft,
    SmoothRight,
    CircleOpen,
    CircleClose,
    /// Hard cut — no crossfade, the clips are simply concatenated.
    Cut,
}

impl TransitionKind {
    fn xfade_name(self) -> Option<&'static str> {
        Some(match self {
            Self::Cut => return None,
            Self::Fade => "fade",
            Self::Dissolve => "dissolve",
            Self::WipeLeft => "wipeleft",
            Self::WipeRight => "wiperight",
            Self::WipeUp => "wipeup",
            Self::WipeDown => "wipedown",
            Self::SlideLeft => "slideleft",
            Self::SlideRight => "slideright",
            Self::SmoothLeft => "smoothleft",
            Self::SmoothRight => "smoothright",
            Self::CircleOpen => "circleopen",
            Self::CircleClose => "circleclose",
        })
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Overlay {
    /// Text burned into the picture: claims, lower thirds, end titles.
    Text {
        text: String,
        /// fontconfig pattern, e.g. `Inter:style=Bold`. Resolved inside the
        /// sandbox, so it must be a font the image actually carries.
        #[serde(default = "d_font")]
        font: String,
        #[serde(default = "d_font_size")]
        size: u32,
        #[serde(default = "d_white")]
        color: String,
        /// Position expressions. `w`/`h` are the frame, `tw`/`th` the text,
        /// `t` the timestamp — e.g. `x: "(w-tw)/2"` centres.
        #[serde(default = "d_center_x")]
        x: String,
        #[serde(default = "d_lower_third_y")]
        y: String,
        /// When the overlay is visible, in output seconds.
        #[serde(default)]
        start: f64,
        #[serde(default)]
        end: Option<f64>,
        #[serde(default)]
        animate_in: Option<Animation>,
        #[serde(default)]
        animate_out: Option<Animation>,
        /// Draw a background box behind the text for legibility.
        #[serde(default)]
        box_color: Option<String>,
        #[serde(default = "d_box_pad")]
        box_padding: u32,
    },
    /// A still image composited on top: logo, badge, lower-third plate.
    Image {
        source: String,
        #[serde(default = "d_zero_expr")]
        x: String,
        #[serde(default = "d_zero_expr")]
        y: String,
        /// Scale the image to this width, height derived to keep the aspect.
        #[serde(default)]
        width: Option<u32>,
        #[serde(default)]
        start: f64,
        #[serde(default)]
        end: Option<f64>,
    },
}

fn d_font() -> String {
    "Inter:style=Bold".into()
}
fn d_font_size() -> u32 {
    48
}
fn d_white() -> String {
    "#ffffff".into()
}
fn d_center_x() -> String {
    "(w-tw)/2".into()
}
fn d_lower_third_y() -> String {
    "h-h/6".into()
}
fn d_zero_expr() -> String {
    "0".into()
}
fn d_box_pad() -> u32 {
    12
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Animation {
    #[serde(default)]
    pub(crate) kind: AnimationKind,
    #[serde(default = "d_anim_dur")]
    pub(crate) duration: f64,
}

fn d_anim_dur() -> f64 {
    0.4
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnimationKind {
    #[default]
    Fade,
    /// Slide in from (or out to) the left edge.
    SlideLeft,
    SlideRight,
    SlideUp,
    SlideDown,
    /// Grow into place (in) / shrink away (out).
    Scale,
    None,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AudioTrack {
    pub(crate) source: String,
    /// Linear gain. 0.3 sits a music bed under speech.
    #[serde(default = "d_volume")]
    pub(crate) volume: f64,
    /// Normalise perceived loudness to broadcast-ish levels (EBU R128).
    ///
    /// Right for a music bed, usually wrong for one line of narration among
    /// several: R128 normalises each track to the same *perceived* loudness, so
    /// a quiet line and a loud one end up equally loud and the delivery
    /// flattens out. Turn it off on the voice tracks and keep the levels the
    /// synthesis produced.
    #[serde(default = "d_true")]
    pub(crate) loudnorm: bool,
    /// Seconds into the finished video where this track starts. Omitted (or 0)
    /// starts it at the top, which is what a music bed wants; a narration line
    /// belongs at the cut it speaks over.
    #[serde(default)]
    pub(crate) start: Option<f64>,
    #[serde(default)]
    pub(crate) fade_in: Option<f64>,
    #[serde(default)]
    pub(crate) fade_out: Option<f64>,
}

/// Accept either one audio track or a list of them.
///
/// The field began as a single optional music bed, and every spec written
/// against that shape has to keep working — a stored canvas document is a
/// *saved* spec, so changing the accepted JSON would break documents already
/// on disk, not just future calls.
fn one_or_many_tracks<'de, D>(d: D) -> Result<Vec<AudioTrack>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Box<AudioTrack>),
        Many(Vec<AudioTrack>),
    }
    Ok(match Option::<OneOrMany>::deserialize(d)? {
        None => Vec::new(),
        Some(OneOrMany::One(t)) => vec![*t],
        Some(OneOrMany::Many(v)) => v,
    })
}

fn d_volume() -> f64 {
    1.0
}
fn d_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Validation

/// Upper bounds that keep one call from monopolising the sandbox. Generous
/// enough for an ad; a feature film is not the use case.
const MAX_CLIPS: usize = 40;
/// Enough for a music bed plus a narration line per scene; well below the point
/// where `amix` starts costing real time.
const MAX_AUDIO_TRACKS: usize = 24;
const MAX_OVERLAYS: usize = 40;
const MAX_DIM: u32 = 3840;
const MIN_DIM: u32 = 64;

/// Characters and identifiers ffmpeg position/size expressions may use.
///
/// This is the one place user input reaches the filtergraph, so it is a
/// whitelist: digits, spaces, arithmetic, parentheses, a decimal point, and the
/// handful of variables `drawtext`/`overlay` expose. Notably absent are `:` and
/// `,` (which would end the option or the filter), quotes, `;`, `[`, `]` and
/// backslashes. Function names like `if(...)` are not allowed either — the
/// animations that need them are generated here, not written by the caller.
pub(crate) fn validate_expr(field: &str, e: &str) -> Result<(), ToolError> {
    const VARS: [&str; 8] = ["w", "h", "tw", "th", "lh", "t", "n", "main_w"];
    if e.is_empty() {
        return Err(ToolError::InvalidArgs(format!("{field} must not be empty")));
    }
    if e.len() > 120 {
        return Err(ToolError::InvalidArgs(format!(
            "{field} is too long ({} chars, max 120)",
            e.len()
        )));
    }
    let mut ident = String::new();
    for c in e.chars() {
        if c.is_ascii_alphabetic() || c == '_' {
            ident.push(c);
            continue;
        }
        if !ident.is_empty() {
            if !VARS.contains(&ident.as_str()) {
                return Err(ToolError::InvalidArgs(format!(
                    "{field}: unknown variable `{ident}` — allowed: {}",
                    VARS.join(", ")
                )));
            }
            ident.clear();
        }
        if !(c.is_ascii_digit() || " +-*/().".contains(c)) {
            return Err(ToolError::InvalidArgs(format!(
                "{field}: character `{c}` is not allowed in a position expression"
            )));
        }
    }
    if !ident.is_empty() && !VARS.contains(&ident.as_str()) {
        return Err(ToolError::InvalidArgs(format!(
            "{field}: unknown variable `{ident}` — allowed: {}",
            VARS.join(", ")
        )));
    }
    Ok(())
}

/// `#rrggbb`, `#rrggbbaa`, or one of a few names. Returned in the form
/// `drawtext` wants (`0xRRGGBB` with an optional `@alpha`).
pub(crate) fn validate_color(field: &str, c: &str) -> Result<String, ToolError> {
    const NAMED: [(&str, &str); 8] = [
        ("white", "0xffffff"),
        ("black", "0x000000"),
        ("red", "0xff0000"),
        ("green", "0x00ff00"),
        ("blue", "0x0000ff"),
        ("yellow", "0xffff00"),
        ("orange", "0xff8800"),
        ("grey", "0x808080"),
    ];
    if let Some((_, v)) = NAMED.iter().find(|(n, _)| *n == c.to_ascii_lowercase()) {
        return Ok((*v).to_string());
    }
    let hex = c.strip_prefix('#').unwrap_or(c);
    let ok = (hex.len() == 6 || hex.len() == 8) && hex.chars().all(|c| c.is_ascii_hexdigit());
    if !ok {
        return Err(ToolError::InvalidArgs(format!(
            "{field}: `{c}` is not a colour — use #rrggbb, #rrggbbaa, or one of {}",
            NAMED.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
        )));
    }
    if hex.len() == 8 {
        // drawtext takes alpha separately: 0xRRGGBB@0.xx
        let a = u8::from_str_radix(&hex[6..8], 16).unwrap_or(255);
        Ok(format!("0x{}@{:.3}", &hex[..6], f64::from(a) / 255.0))
    } else {
        Ok(format!("0x{hex}"))
    }
}

fn check_time(field: &str, v: f64) -> Result<(), ToolError> {
    if !v.is_finite() || !(0.0..=3600.0).contains(&v) {
        return Err(ToolError::InvalidArgs(format!(
            "{field} must be between 0 and 3600 seconds (got {v})"
        )));
    }
    Ok(())
}

impl VideoSpec {
    /// Reject a spec before anything is staged or spawned, with messages that
    /// name the field — an ffmpeg failure three layers down is useless to the
    /// model.
    pub(crate) fn validate(&self) -> Result<(), ToolError> {
        if self.clips.is_empty() {
            return Err(ToolError::InvalidArgs(
                "clips must contain at least one clip".into(),
            ));
        }
        if self.clips.len() > MAX_CLIPS {
            return Err(ToolError::InvalidArgs(format!(
                "too many clips ({}, max {MAX_CLIPS})",
                self.clips.len()
            )));
        }
        if self.overlays.len() > MAX_OVERLAYS {
            return Err(ToolError::InvalidArgs(format!(
                "too many overlays ({}, max {MAX_OVERLAYS})",
                self.overlays.len()
            )));
        }
        let o = &self.output;
        for (f, v) in [("output.width", o.width), ("output.height", o.height)] {
            if !(MIN_DIM..=MAX_DIM).contains(&v) {
                return Err(ToolError::InvalidArgs(format!(
                    "{f} must be between {MIN_DIM} and {MAX_DIM} (got {v})"
                )));
            }
            if v % 2 != 0 {
                return Err(ToolError::InvalidArgs(format!(
                    "{f} must be even — H.264 chroma subsampling needs it (got {v})"
                )));
            }
        }
        if !(1.0..=120.0).contains(&o.fps) {
            return Err(ToolError::InvalidArgs(format!(
                "output.fps must be between 1 and 120 (got {})",
                o.fps
            )));
        }
        if o.crf > 51 {
            return Err(ToolError::InvalidArgs(format!(
                "output.crf must be 0–51 (got {})",
                o.crf
            )));
        }
        for (i, c) in self.clips.iter().enumerate() {
            if c.source.trim().is_empty() {
                return Err(ToolError::InvalidArgs(format!(
                    "clips[{i}].source must not be empty"
                )));
            }
            if let Some(s) = c.start {
                check_time(&format!("clips[{i}].start"), s)?;
            }
            if let Some(e) = c.end {
                check_time(&format!("clips[{i}].end"), e)?;
            }
            if let (Some(s), Some(e)) = (c.start, c.end)
                && e <= s
            {
                return Err(ToolError::InvalidArgs(format!(
                    "clips[{i}]: end ({e}) must be greater than start ({s})"
                )));
            }
            if let Some(t) = &c.transition
                && !(0.05..=5.0).contains(&t.duration)
            {
                return Err(ToolError::InvalidArgs(format!(
                    "clips[{i}].transition.duration must be 0.05–5 s (got {})",
                    t.duration
                )));
            }
        }
        for (i, ov) in self.overlays.iter().enumerate() {
            match ov {
                Overlay::Text {
                    text,
                    size,
                    color,
                    x,
                    y,
                    start,
                    end,
                    animate_in,
                    animate_out,
                    box_color,
                    font,
                    ..
                } => {
                    if text.is_empty() {
                        return Err(ToolError::InvalidArgs(format!(
                            "overlays[{i}].text must not be empty"
                        )));
                    }
                    if text.len() > 500 {
                        return Err(ToolError::InvalidArgs(format!(
                            "overlays[{i}].text is too long ({} chars, max 500)",
                            text.len()
                        )));
                    }
                    // The font pattern goes to fc-match inside the sandbox, so
                    // keep it to what a fontconfig pattern needs.
                    if font.len() > 80
                        || !font
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || " -_.:=".contains(c))
                    {
                        return Err(ToolError::InvalidArgs(format!(
                            "overlays[{i}].font must be a plain fontconfig pattern like \
                             `Inter:style=Bold`"
                        )));
                    }
                    if !(4..=400).contains(size) {
                        return Err(ToolError::InvalidArgs(format!(
                            "overlays[{i}].size must be 4–400 (got {size})"
                        )));
                    }
                    validate_color(&format!("overlays[{i}].color"), color)?;
                    if let Some(bc) = box_color {
                        validate_color(&format!("overlays[{i}].box_color"), bc)?;
                    }
                    validate_expr(&format!("overlays[{i}].x"), x)?;
                    validate_expr(&format!("overlays[{i}].y"), y)?;
                    check_time(&format!("overlays[{i}].start"), *start)?;
                    if let Some(e) = end {
                        check_time(&format!("overlays[{i}].end"), *e)?;
                        if *e <= *start {
                            return Err(ToolError::InvalidArgs(format!(
                                "overlays[{i}]: end ({e}) must be greater than start ({start})"
                            )));
                        }
                    }
                    for (f, a) in [("animate_in", animate_in), ("animate_out", animate_out)] {
                        if let Some(a) = a
                            && !(0.05..=5.0).contains(&a.duration)
                        {
                            return Err(ToolError::InvalidArgs(format!(
                                "overlays[{i}].{f}.duration must be 0.05–5 s (got {})",
                                a.duration
                            )));
                        }
                    }
                }
                Overlay::Image {
                    source,
                    x,
                    y,
                    width,
                    start,
                    end,
                } => {
                    if source.trim().is_empty() {
                        return Err(ToolError::InvalidArgs(format!(
                            "overlays[{i}].source must not be empty"
                        )));
                    }
                    validate_expr(&format!("overlays[{i}].x"), x)?;
                    validate_expr(&format!("overlays[{i}].y"), y)?;
                    if let Some(w) = width
                        && !(2..=MAX_DIM).contains(w)
                    {
                        return Err(ToolError::InvalidArgs(format!(
                            "overlays[{i}].width must be 2–{MAX_DIM} (got {w})"
                        )));
                    }
                    check_time(&format!("overlays[{i}].start"), *start)?;
                    if let Some(e) = end {
                        check_time(&format!("overlays[{i}].end"), *e)?;
                        if *e <= *start {
                            return Err(ToolError::InvalidArgs(format!(
                                "overlays[{i}]: end ({e}) must be greater than start ({start})"
                            )));
                        }
                    }
                }
            }
        }
        if self.audio.len() > MAX_AUDIO_TRACKS {
            return Err(ToolError::InvalidArgs(format!(
                "at most {MAX_AUDIO_TRACKS} audio tracks (got {})",
                self.audio.len()
            )));
        }
        for (i, a) in self.audio.iter().enumerate() {
            if a.source.trim().is_empty() {
                return Err(ToolError::InvalidArgs(format!(
                    "audio[{i}].source must not be empty"
                )));
            }
            if !(0.0..=4.0).contains(&a.volume) {
                return Err(ToolError::InvalidArgs(format!(
                    "audio[{i}].volume must be 0–4 (got {})",
                    a.volume
                )));
            }
            for (f, v) in [
                (format!("audio[{i}].start"), a.start),
                (format!("audio[{i}].fade_in"), a.fade_in),
                (format!("audio[{i}].fade_out"), a.fade_out),
            ] {
                if let Some(v) = v {
                    check_time(&f, v)?;
                }
            }
        }
        if let Some(f) = self.fade_out {
            check_time("fade_out", f)?;
        }
        Ok(())
    }

    /// Every source the spec references, in a stable order — clips first, then
    /// overlay images, then audio. The caller maps these to staged files.
    pub(crate) fn sources(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.clips.iter().map(|c| c.source.as_str()).collect();
        for o in &self.overlays {
            if let Overlay::Image { source, .. } = o {
                v.push(source.as_str());
            }
        }
        for a in &self.audio {
            v.push(a.source.as_str());
        }
        v
    }
}

// ---------------------------------------------------------------------------
// Script building

/// A source mapped to the generated filename it gets inside `/work`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SafeInput {
    /// What the spec calls it (attachment id or filename).
    pub(crate) source: String,
    /// What the script calls it: `in0.mp4`, `img1.png`, …
    pub(crate) safe_name: String,
}

/// Everything one render needs: the bash to run plus the files the script
/// reads (filtergraph, overlay texts). No user string is ever in `script`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RenderPlan {
    pub(crate) script: String,
    pub(crate) files: Vec<(String, Vec<u8>)>,
    pub(crate) output_name: String,
}

/// Format a float without an exponent and without a trailing `.0`, so the
/// generated graph stays readable and never emits `1e-7`.
fn num(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    if s.is_empty() || s == "-0" {
        "0".into()
    } else {
        s
    }
}

/// Pass 1: ask ffprobe how long every clip is and fontconfig where each
/// requested font lives, one `key<TAB>value` line each.
///
/// Resolving fonts here rather than patching the graph inside the sandbox is
/// what lets pass 2 write a *finished* filtergraph: no placeholder substitution,
/// no font pattern anywhere near a command line.
pub(crate) fn build_probe_script(inputs: &[SafeInput], fonts: &[String]) -> String {
    let mut s = String::from("set -e\n");
    for i in inputs {
        // `|| echo 0` so a still image (no duration) doesn't abort the pass.
        let _ = writeln!(
            s,
            "printf 'dur\\t%s\\t' {n:?}; ffprobe -v error -show_entries format=duration \
             -of default=nw=1:nk=1 {n:?} 2>/dev/null || echo 0",
            n = i.safe_name
        );
    }
    for f in fonts {
        let _ = writeln!(
            s,
            "printf 'font\\t%s\\t' {f:?}; fc-match -f '%{{file}}\\n' {f:?} 2>/dev/null || echo"
        );
    }
    s
}

/// What pass 1 found out.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Probed {
    /// `safe_name -> seconds`; absent for stills and unreadable inputs.
    pub(crate) durations: HashMap<String, f64>,
    /// `font pattern -> font file path`.
    pub(crate) fonts: HashMap<String, String>,
}

/// A resolved font path only reaches the graph if it looks like one: absolute,
/// no `:` (which would end the drawtext option) and no whitespace tricks.
fn plausible_font_path(p: &str) -> bool {
    p.starts_with('/')
        && p.len() < 300
        && !p.contains(':')
        && !p.contains('\'')
        && !p.chars().any(|c| c.is_control())
}

/// Parse pass 1's `kind<TAB>key<TAB>value` lines.
pub(crate) fn parse_probe_output(out: &str) -> Probed {
    let mut p = Probed::default();
    for line in out.lines() {
        let mut it = line.splitn(3, '\t');
        match (it.next(), it.next(), it.next()) {
            (Some("dur"), Some(name), Some(v)) => {
                if let Ok(d) = v.trim().parse::<f64>()
                    && d.is_finite()
                    && d > 0.0
                {
                    p.durations.insert(name.trim().to_string(), d);
                }
            }
            (Some("font"), Some(pat), Some(path)) => {
                let path = path.trim();
                if plausible_font_path(path) {
                    p.fonts.insert(pat.trim().to_string(), path.to_string());
                }
            }
            _ => {}
        }
    }
    p
}

/// The visible length of a clip after trimming, given the probed source length.
fn clip_duration(c: &Clip, probed: Option<f64>) -> f64 {
    let src = probed.unwrap_or(0.0);
    let start = c.start.unwrap_or(0.0);
    let end = c.end.unwrap_or(src);
    let d = end - start;
    if d.is_finite() && d > 0.0 { d } else { 0.0 }
}

/// Build the `alpha`/`x`/`y`/`fontsize` expressions that animate one text
/// overlay. Generated here — never taken from the caller — so the only
/// user-controlled part is the static position.
struct TextMotion {
    alpha: String,
    x: String,
    y: String,
    size: String,
}

fn text_motion(
    x: &str,
    y: &str,
    size: u32,
    start: f64,
    end: Option<f64>,
    ain: Option<&Animation>,
    aout: Option<&Animation>,
) -> TextMotion {
    let mut alpha_terms: Vec<String> = Vec::new();
    let mut xe = x.to_string();
    let mut ye = y.to_string();
    let mut se = size.to_string();

    if let Some(a) = ain {
        let d = num(a.duration);
        let s = num(start);
        // Progress 0→1 over the animation, clamped.
        let p = format!("min(1,max(0,(t-{s})/{d}))");
        match a.kind {
            AnimationKind::Fade => alpha_terms.push(p.clone()),
            AnimationKind::SlideLeft => xe = format!("({xe})*({p})-tw*(1-({p}))"),
            AnimationKind::SlideRight => xe = format!("({xe})+(w-({xe}))*(1-({p}))"),
            AnimationKind::SlideUp => ye = format!("({ye})+(h-({ye}))*(1-({p}))"),
            AnimationKind::SlideDown => ye = format!("({ye})*({p})-th*(1-({p}))"),
            AnimationKind::Scale => se = format!("max(4,{size}*({p}))"),
            AnimationKind::None => {}
        }
    }
    if let Some(a) = aout
        && let Some(e) = end
    {
        let d = num(a.duration);
        let s = num((e - a.duration).max(0.0));
        // Progress 1→0 across the tail.
        let q = format!("min(1,max(0,({e}-t)/{d}))", e = num(e));
        match a.kind {
            AnimationKind::Fade => alpha_terms.push(q.clone()),
            AnimationKind::SlideLeft => xe = format!("({xe})-(({xe})+tw)*(1-({q}))"),
            AnimationKind::SlideRight => xe = format!("({xe})+(w-({xe}))*(1-({q}))"),
            AnimationKind::SlideUp => ye = format!("({ye})-(({ye})+th)*(1-({q}))"),
            AnimationKind::SlideDown => ye = format!("({ye})+(h-({ye}))*(1-({q}))"),
            AnimationKind::Scale => se = format!("max(4,{size}*({q}))"),
            AnimationKind::None => {}
        }
        let _ = s;
    }

    let alpha = match alpha_terms.len() {
        0 => "1".to_string(),
        1 => alpha_terms.remove_first(),
        _ => format!("min({},{})", alpha_terms[0], alpha_terms[1]),
    };
    TextMotion {
        alpha,
        x: xe,
        y: ye,
        size: se,
    }
}

/// Tiny helper so `text_motion` reads cleanly above.
trait RemoveFirst {
    fn remove_first(self) -> String;
}
impl RemoveFirst for Vec<String> {
    fn remove_first(mut self) -> String {
        self.remove(0)
    }
}

/// Pass 2: normalise every clip, join them, apply overlays and audio.
///
/// Returns the script plus the files it reads. The filtergraph and every
/// overlay string are files precisely so that nothing user-supplied has to be
/// escaped for a shell or for ffmpeg's option parser.
pub(crate) fn build_render_plan(
    spec: &VideoSpec,
    inputs: &[SafeInput],
    probed: &Probed,
) -> Result<RenderPlan, ToolError> {
    let durations = &probed.durations;
    let by_source: HashMap<&str, &str> = inputs
        .iter()
        .map(|i| (i.source.as_str(), i.safe_name.as_str()))
        .collect();
    let name_of = |src: &str| -> Result<String, ToolError> {
        by_source
            .get(src)
            .map(|s| (*s).to_string())
            .ok_or_else(|| ToolError::Failed(format!("source `{src}` was not staged")))
    };

    // Durations were probed on the staged sources, so look them up by the
    // input's safe name — not by the normalised file, which does not exist yet.
    let mut clip_len: Vec<f64> = Vec::with_capacity(spec.clips.len());
    for c in &spec.clips {
        let src = name_of(&c.source)?;
        clip_len.push(clip_duration(c, durations.get(src.as_str()).copied()));
    }

    let o = &spec.output;
    let (w, h, fps) = (o.width, o.height, num(o.fps));
    let mut script = String::from("set -e\n");
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();

    // --- pass 2a: every clip onto the same grid ----------------------------
    // Scale-then-pad keeps the source aspect and letterboxes the rest, which is
    // what stops a portrait phone clip from being stretched into a 16:9 slot.
    let mut norm: Vec<String> = Vec::new();
    for (i, c) in spec.clips.iter().enumerate() {
        let src = name_of(&c.source)?;
        let out = format!("n{i}.mp4");
        let mut trim = String::new();
        if let Some(s) = c.start {
            let _ = write!(trim, "-ss {} ", num(s));
        }
        if let Some(e) = c.end {
            let d = e - c.start.unwrap_or(0.0);
            let _ = write!(trim, "-t {} ", num(d));
        }
        let _ = writeln!(
            script,
            "ffmpeg -y -loglevel error {trim}-i {src:?} -vf \
             \"scale={w}:{h}:force_original_aspect_ratio=decrease,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,fps={fps},setsar=1\" \
             -an -c:v libx264 -crf {crf} -pix_fmt yuv420p {out:?}",
            crf = o.crf
        );
        norm.push(out);
    }

    // --- pass 2b: the filtergraph -----------------------------------------
    let mut graph = String::new();
    let mut label: String;

    if norm.len() == 1 {
        label = "0:v".into();
    } else {
        // Chain xfades left to right. Each transition overlaps the previous
        // result by its duration, so the offset is the running length minus it.
        let mut running = clip_len[0];
        let mut cur = "0:v".to_string();
        for (i, (c, dur)) in spec
            .clips
            .iter()
            .zip(clip_len.iter().copied())
            .enumerate()
            .skip(1)
        {
            let t = c.transition.clone().unwrap_or(Transition {
                kind: TransitionKind::Cut,
                duration: 0.0,
            });
            let next = format!("v{i}");
            match t.kind.xfade_name() {
                None => {
                    let _ = writeln!(
                        graph,
                        "[{cur}][{i}:v]concat=n=2:v=1:a=0[{next}];",
                        cur = cur,
                        i = i
                    );
                    running += dur;
                }
                Some(x) => {
                    let off = (running - t.duration).max(0.0);
                    let _ = writeln!(
                        graph,
                        "[{cur}][{i}:v]xfade=transition={x}:duration={d}:offset={off}[{next}];",
                        d = num(t.duration),
                        off = num(off)
                    );
                    running += dur - t.duration;
                }
            }
            cur = next;
        }
        label = cur;
    }

    // --- pass 2c: overlays -------------------------------------------------
    // Image overlays come first so text sits on top of a logo plate.
    let mut img_input_index = norm.len();
    let mut extra_inputs: Vec<String> = Vec::new();
    for (i, ov) in spec.overlays.iter().enumerate() {
        if let Overlay::Image {
            source,
            x,
            y,
            width,
            start,
            end,
        } = ov
        {
            let src = name_of(source)?;
            extra_inputs.push(src);
            let idx = img_input_index;
            img_input_index += 1;
            let scaled = format!("ov{i}");
            match width {
                Some(w) => {
                    let _ = writeln!(graph, "[{idx}:v]scale={w}:-2[{scaled}];");
                }
                None => {
                    let _ = writeln!(graph, "[{idx}:v]null[{scaled}];");
                }
            }
            let next = format!("o{i}");
            let enable = match end {
                Some(e) => format!(":enable='between(t,{s},{e})'", s = num(*start), e = num(*e)),
                None if *start > 0.0 => format!(":enable='gte(t,{s})'", s = num(*start)),
                None => String::new(),
            };
            let _ = writeln!(
                graph,
                "[{label}][{scaled}]overlay=x='{x}':y='{y}'{enable}[{next}];"
            );
            label = next;
        }
    }
    for (i, ov) in spec.overlays.iter().enumerate() {
        if let Overlay::Text {
            text,
            font,
            size,
            color,
            x,
            y,
            start,
            end,
            animate_in,
            animate_out,
            box_color,
            box_padding,
        } = ov
        {
            // The string itself never enters the graph — it is a file.
            let tf = format!("text{i}.txt");
            files.push((tf.clone(), text.as_bytes().to_vec()));
            let m = text_motion(
                x,
                y,
                *size,
                *start,
                *end,
                animate_in.as_ref(),
                animate_out.as_ref(),
            );
            let col = validate_color("color", color)?;
            // Resolved in pass 1. A font fontconfig couldn't place is a clear
            // error here rather than an ffmpeg one three layers down.
            let font_file = probed.fonts.get(font).ok_or_else(|| {
                ToolError::InvalidArgs(format!(
                    "overlays[{i}].font `{font}` did not resolve to a font file in the                      sandbox — try `Inter:style=Bold` or `DejaVu Sans:style=Bold`"
                ))
            })?;
            // Every expression is single-quoted: `min(1,max(0,…))` contains
            // commas, and an unquoted comma ends the filter — which is the same
            // trap hand-written graphs fall into, just with generated text.
            let mut opts = format!(
                "textfile={tf}:fontfile={font_file}:fontcolor={col}:fontsize='{fs}':x='{x}':y='{y}':alpha='{a}'",
                fs = m.size,
                x = m.x,
                y = m.y,
                a = m.alpha
            );
            if let Some(bc) = box_color {
                let bcol = validate_color("box_color", bc)?;
                let _ = write!(opts, ":box=1:boxcolor={bcol}:boxborderw={box_padding}");
            }
            let window = match end {
                Some(e) => format!(":enable='between(t,{s},{e})'", s = num(*start), e = num(*e)),
                None if *start > 0.0 => format!(":enable='gte(t,{s})'", s = num(*start)),
                None => String::new(),
            };
            let next = format!("t{i}");
            let _ = writeln!(graph, "[{label}]drawtext={opts}{window}[{next}];");
            label = next;
        }
    }

    // --- pass 2d: closing fade + audio ------------------------------------
    // Finished length: the running sum after the xfade chain, where each
    // crossfade eats its own duration. Both the closing video fade and every
    // audio fade-out are positioned against it.
    let total: f64 = {
        let mut t = clip_len[0];
        for (c, d) in spec.clips.iter().zip(clip_len.iter().copied()).skip(1) {
            let overlap = c
                .transition
                .as_ref()
                .filter(|t| t.kind.xfade_name().is_some())
                .map(|t| t.duration)
                .unwrap_or(0.0);
            t += d - overlap;
        }
        t
    };
    if let Some(f) = spec.fade_out {
        let st = num((total - f).max(0.0));
        let next = "vfade".to_string();
        let _ = writeln!(
            graph,
            "[{label}]fade=t=out:st={st}:d={d}[{next}];",
            d = num(f)
        );
        label = next;
    }
    let _ = writeln!(graph, "[{label}]null[vout]");

    let mut audio_map = String::new();
    if !spec.audio.is_empty() {
        // One chain per track, then a single mix. Each chain ends in `apad`,
        // which pairs with `-shortest` on the command line to give the one
        // behaviour that is right in both directions: audio longer than the
        // video is cut off at the last frame, audio shorter than it is padded
        // with silence. Without the pad, `-shortest` would truncate the *video*
        // to the length of a short music bed or a narration line.
        let mut labels: Vec<String> = Vec::with_capacity(spec.audio.len());
        for (i, a) in spec.audio.iter().enumerate() {
            let src = name_of(&a.source)?;
            let idx = img_input_index + i;
            extra_inputs.push(src.clone());
            let mut chain = format!("[{idx}:a]volume={v}", v = num(a.volume));
            if a.loudnorm {
                chain.push_str(",loudnorm=I=-16:TP=-1.5:LRA=11");
            }
            // Fades are relative to the track, so they go on before the delay
            // shifts it into place on the timeline.
            if let Some(f) = a.fade_in {
                let _ = write!(chain, ",afade=t=in:st=0:d={}", num(f));
            }
            // `afade=t=out` needs an explicit start: ffmpeg defaults `st` to 0,
            // which fades the track out over its first seconds instead of its
            // last — a music bed that vanishes a second in. So aim it at
            // whichever end comes first, the track's own or the video's, in
            // track-local time (the delay below shifts the whole chain).
            if let Some(f) = a.fade_out {
                let start = a.start.unwrap_or(0.0);
                let audible = probed
                    .durations
                    .get(&src)
                    .copied()
                    .map_or(total - start, |len| len.min(total - start));
                let _ = write!(
                    chain,
                    ",afade=t=out:st={st}:d={d}",
                    st = num((audible - f).max(0.0)),
                    d = num(f)
                );
            }
            // `all=1` delays every channel; without it `adelay` shifts only the
            // first, which turns a stereo line into a smeared half-echo.
            if let Some(start) = a.start.filter(|s| *s > 0.0) {
                let _ = write!(
                    chain,
                    ",adelay={ms}:all=1",
                    ms = num((start * 1000.0).round())
                );
            }
            let label = format!("a{i}");
            let _ = writeln!(graph, ";{chain}[{label}]");
            labels.push(label);
        }
        if labels.len() == 1 {
            let _ = writeln!(graph, ";[{}]apad[aout]", labels[0]);
        } else {
            // `normalize=0`: amix otherwise divides every input by the number of
            // inputs, so adding a narration line would quietly halve the music.
            let inputs: String = labels.iter().map(|l| format!("[{l}]")).collect();
            let _ = writeln!(
                graph,
                ";{inputs}amix=inputs={n}:normalize=0:dropout_transition=0,apad[aout]",
                n = labels.len()
            );
        }
        audio_map = " -map \"[aout]\" -c:a aac -b:a 192k -shortest".into();
    }

    files.push(("filter.txt".into(), graph.into_bytes()));

    // --- the single render command ----------------------------------------
    let mut cmd = String::from("ffmpeg -y -loglevel error");
    for n in &norm {
        let _ = write!(cmd, " -i {n:?}");
    }
    for n in &extra_inputs {
        let _ = write!(cmd, " -i {n:?}");
    }
    let out_name = "video.mp4".to_string();
    let _ = write!(
        cmd,
        " -filter_complex_script filter.txt -map \"[vout]\"{audio_map} \
         -c:v libx264 -crf {crf} -pix_fmt yuv420p -movflags +faststart {out:?}",
        crf = o.crf,
        out = out_name
    );
    let _ = writeln!(script, "{cmd}");
    // Clean the intermediates so they aren't collected as artifacts too.
    for n in &norm {
        let _ = writeln!(script, "rm -f {n:?}");
    }
    let _ = writeln!(script, "rm -f {:?}", "filter.txt");
    for (i, ov) in spec.overlays.iter().enumerate() {
        if matches!(ov, Overlay::Text { .. }) {
            let _ = writeln!(script, "rm -f {:?}", format!("text{i}.txt"));
        }
    }

    Ok(RenderPlan {
        script,
        files,
        output_name: out_name,
    })
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> Vec<SafeInput> {
        vec![
            SafeInput {
                source: "a.mp4".into(),
                safe_name: "in0.mp4".into(),
            },
            SafeInput {
                source: "b.mp4".into(),
                safe_name: "in1.mp4".into(),
            },
            SafeInput {
                source: "logo.png".into(),
                safe_name: "img2.png".into(),
            },
            SafeInput {
                source: "music.mp3".into(),
                safe_name: "aud3.mp3".into(),
            },
            SafeInput {
                source: "line1.mp3".into(),
                safe_name: "aud4.mp3".into(),
            },
            SafeInput {
                source: "line2.mp3".into(),
                safe_name: "aud5.mp3".into(),
            },
        ]
    }

    fn probed() -> Probed {
        Probed {
            durations: HashMap::from([
                ("in0.mp4".to_string(), 4.0),
                ("in1.mp4".to_string(), 6.0),
                ("aud3.mp3".to_string(), 2.0),
            ]),
            fonts: HashMap::from([(
                "Inter:style=Bold".to_string(),
                "/usr/share/fonts/inter/Inter-Bold.otf".to_string(),
            )]),
        }
    }

    fn spec_from(j: serde_json::Value) -> VideoSpec {
        serde_json::from_value(j).expect("spec should parse")
    }

    fn plan(j: serde_json::Value) -> RenderPlan {
        let s = spec_from(j);
        s.validate().expect("spec should validate");
        build_render_plan(&s, &inputs(), &probed()).expect("plan should build")
    }

    fn filter_of(p: &RenderPlan) -> String {
        let (_, bytes) = p
            .files
            .iter()
            .find(|(n, _)| n == "filter.txt")
            .expect("filter.txt");
        String::from_utf8(bytes.clone()).unwrap()
    }

    // --- the escaping problem this tool exists to remove -------------------

    #[test]
    fn overlay_text_never_enters_the_graph_or_the_script() {
        // Exactly the characters that break a hand-written filtergraph: a
        // comma ends the filter, a colon ends the option, quotes and brackets
        // confuse both ffmpeg and the shell.
        let nasty = "Preis: 9,99 € — \"jetzt\" [neu]; rm -rf /";
        let p = plan(serde_json::json!({
            "clips": [{"source": "a.mp4"}],
            "overlays": [{"type": "text", "text": nasty}],
        }));
        assert!(
            !p.script.contains("9,99"),
            "text must not reach the shell script:\n{}",
            p.script
        );
        let f = filter_of(&p);
        assert!(
            !f.contains("9,99"),
            "text must not reach the filtergraph:\n{f}"
        );
        assert!(f.contains("textfile=text0.txt"), "{f}");
        // …and the string is delivered verbatim as a file instead.
        let (_, bytes) = p.files.iter().find(|(n, _)| n == "text0.txt").unwrap();
        assert_eq!(String::from_utf8(bytes.clone()).unwrap(), nasty);
    }

    #[test]
    fn input_filenames_are_generated_never_taken_from_the_spec() {
        let p = plan(serde_json::json!({
            "clips": [{"source": "a.mp4"}],
            "overlays": [{"type": "image", "source": "logo.png"}],
            "audio": {"source": "music.mp3"},
        }));
        // The spec's own names must not appear anywhere in the command line.
        for name in ["a.mp4", "logo.png", "music.mp3"] {
            assert!(
                !p.script.contains(name),
                "{name} leaked into the script:\n{}",
                p.script
            );
        }
        assert!(p.script.contains("in0.mp4") && p.script.contains("img2.png"));
    }

    #[test]
    fn position_expressions_reject_anything_that_could_break_out() {
        // A colon would end the drawtext option; a comma would end the filter;
        // brackets would start a new graph link.
        for bad in [
            "0:text=pwned",
            "1,drawtext=text=x",
            "(w-tw)/2]",
            "w'",
            "if(lt(t,1),0,w)",
            "$(id)",
            "w\\",
        ] {
            assert!(validate_expr("x", bad).is_err(), "{bad:?} must be rejected");
        }
        // …while the expressions an ad actually needs are fine.
        for good in ["0", "(w-tw)/2", "h-h/6", "w-tw-40", "main_w/2 + 10", "t*20"] {
            validate_expr("x", good).unwrap_or_else(|e| panic!("{good:?} rejected: {e:?}"));
        }
    }

    #[test]
    fn colours_accept_hex_and_names_and_reject_junk() {
        assert_eq!(validate_color("c", "#ff8800").unwrap(), "0xff8800");
        assert_eq!(validate_color("c", "white").unwrap(), "0xffffff");
        // Alpha is split out the way drawtext wants it.
        assert_eq!(validate_color("c", "#00000080").unwrap(), "0x000000@0.502");
        for bad in ["red:1", "#12", "rgb(1,2,3)", "'; rm -rf /", "#gggggg"] {
            assert!(
                validate_color("c", bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    // --- the timeline itself ----------------------------------------------

    #[test]
    fn a_single_clip_needs_no_join() {
        let f = filter_of(&plan(serde_json::json!({"clips": [{"source": "a.mp4"}]})));
        assert!(!f.contains("xfade"), "{f}");
        assert!(f.contains("[0:v]null[vout]"), "{f}");
    }

    #[test]
    fn crossfade_offset_follows_the_previous_clip_length() {
        // in0 is 4 s and the fade lasts 0.5 s, so the overlap starts at 3.5 s.
        let f = filter_of(&plan(serde_json::json!({
            "clips": [
                {"source": "a.mp4"},
                {"source": "b.mp4", "transition": {"kind": "wipe_left", "duration": 0.5}},
            ],
        })));
        assert!(
            f.contains("xfade=transition=wipeleft:duration=0.5:offset=3.5"),
            "{f}"
        );
    }

    #[test]
    fn trimming_shortens_the_clip_and_moves_the_next_transition() {
        // a.mp4 trimmed to 1..3 is 2 s long, so a 0.5 s fade starts at 1.5 s.
        let f = filter_of(&plan(serde_json::json!({
            "clips": [
                {"source": "a.mp4", "start": 1.0, "end": 3.0},
                {"source": "b.mp4", "transition": {"kind": "fade", "duration": 0.5}},
            ],
        })));
        assert!(f.contains("offset=1.5"), "{f}");
    }

    #[test]
    fn a_cut_concatenates_instead_of_crossfading() {
        let f = filter_of(&plan(serde_json::json!({
            "clips": [
                {"source": "a.mp4"},
                {"source": "b.mp4", "transition": {"kind": "cut"}},
            ],
        })));
        assert!(f.contains("concat=n=2"), "{f}");
        assert!(!f.contains("xfade"), "{f}");
    }

    #[test]
    fn every_clip_is_normalised_onto_the_output_grid() {
        let p = plan(serde_json::json!({
            "output": {"width": 1080, "height": 1920, "fps": 25, "crf": 18},
            "clips": [{"source": "a.mp4"}, {"source": "b.mp4"}],
        }));
        // Aspect-preserving scale plus padding — a portrait source must not be
        // stretched into a landscape slot.
        assert_eq!(
            p.script
                .matches("scale=1080:1920:force_original_aspect_ratio=decrease")
                .count(),
            2
        );
        assert!(p.script.contains("pad=1080:1920:(ow-iw)/2:(oh-ih)/2"));
        assert!(p.script.contains("fps=25"));
        assert!(p.script.contains("-crf 18"));
    }

    #[test]
    fn text_animation_generates_the_motion_expressions() {
        let f = filter_of(&plan(serde_json::json!({
            "clips": [{"source": "a.mp4"}],
            "overlays": [{
                "type": "text", "text": "CROIT", "start": 0.5, "end": 3.0,
                "animate_in": {"kind": "slide_left", "duration": 0.4},
                "animate_out": {"kind": "fade", "duration": 0.6},
            }],
        })));
        // Slide in: x interpolates from off-screen to the target.
        assert!(f.contains("min(1,max(0,(t-0.5)/0.4))"), "{f}");
        // Fade out: alpha ramps down across the tail.
        assert!(f.contains("min(1,max(0,(3-t)/0.6))"), "{f}");
        // Visible only inside its window.
        assert!(f.contains("enable='between(t,0.5,3)'"), "{f}");
        // And the resolved font path is baked in, not a placeholder.
        assert!(
            f.contains("fontfile=/usr/share/fonts/inter/Inter-Bold.otf"),
            "{f}"
        );
        assert!(!f.contains("%FONT"), "{f}");
    }

    #[test]
    fn image_overlay_scales_and_windows() {
        let f = filter_of(&plan(serde_json::json!({
            "clips": [{"source": "a.mp4"}],
            "overlays": [{
                "type": "image", "source": "logo.png", "width": 200,
                "x": "w-tw-40", "y": "40", "start": 1.0, "end": 5.0,
            }],
        })));
        assert!(f.contains("scale=200:-2"), "{f}");
        assert!(
            f.contains("overlay=x='w-tw-40':y='40':enable='between(t,1,5)'"),
            "{f}"
        );
    }

    #[test]
    fn audio_is_mixed_with_gain_loudness_and_fades() {
        let p = plan(serde_json::json!({
            "clips": [{"source": "a.mp4"}],
            "audio": {"source": "music.mp3", "volume": 0.3, "fade_in": 0.5, "fade_out": 1.0},
        }));
        let f = filter_of(&p);
        assert!(f.contains("volume=0.3"), "{f}");
        assert!(f.contains("loudnorm=I=-16:TP=-1.5:LRA=11"), "{f}");
        // The fade-out is aimed at an end rather than left at ffmpeg's default
        // st=0 (which fades a track out over its first seconds). The bed is
        // 2 s and the video 4 s, so the earlier end wins: 2 - 1 = 1.
        assert!(
            f.contains("afade=t=in:st=0:d=0.5") && f.contains("afade=t=out:st=1:d=1"),
            "{f}"
        );
        assert!(p.script.contains("-map \"[aout]\"") && p.script.contains("-shortest"));
    }

    #[test]
    fn a_single_track_is_padded_so_it_cannot_shorten_the_video() {
        // Regression: `-shortest` alone cuts the *video* down to a music bed
        // that is shorter than the footage. `apad` makes the audio the longer
        // stream, so the cut lands on the video's own last frame instead.
        let f = filter_of(&plan(serde_json::json!({
            "clips": [{"source": "a.mp4"}],
            "audio": {"source": "music.mp3"},
        })));
        assert!(f.contains("apad[aout]"), "{f}");
    }

    #[test]
    fn narration_tracks_are_delayed_and_mixed_without_level_loss() {
        let p = plan(serde_json::json!({
            "clips": [{"source": "a.mp4"}],
            "audio": [
                {"source": "music.mp3", "volume": 0.25, "fade_out": 1.0},
                {"source": "line1.mp3", "loudnorm": false},
                {"source": "line2.mp3", "loudnorm": false, "start": 2.5, "fade_out": 0.3},
            ],
        }));
        let f = filter_of(&p);
        // Each track gets its own chain…
        assert!(f.contains("[1:a]volume=0.25"), "{f}");
        assert!(f.contains("[2:a]volume=1"), "{f}");
        assert!(f.contains("[3:a]volume=1"), "{f}");
        // …the offset one is delayed on every channel, in milliseconds…
        assert!(f.contains("adelay=2500:all=1"), "{f}");
        // …its fade-out is measured from the video's end in track-local time
        // (4 s total - 2.5 s start = 1.5 s audible, minus the 0.3 s fade)…
        assert!(f.contains("afade=t=out:st=1.2:d=0.3"), "{f}");
        // …the one at the top is not delayed at all…
        assert!(!f.contains("adelay=0"), "{f}");
        // …loudnorm applies only where it was asked for…
        assert_eq!(f.matches("loudnorm").count(), 1, "{f}");
        // …and the mix keeps every track at its own level.
        assert!(
            f.contains("[a0][a1][a2]amix=inputs=3:normalize=0:dropout_transition=0,apad[aout]"),
            "{f}"
        );
        // All three audio files are fed to ffmpeg, under their staged names.
        for name in ["aud3.mp3", "aud4.mp3", "aud5.mp3"] {
            assert!(p.script.contains(&format!("-i \"{name}\"")), "{}", p.script);
        }
    }

    #[test]
    fn a_single_audio_object_still_parses_as_before() {
        // Specs saved in canvas documents were written against the original
        // single-object shape; they must keep loading unchanged.
        let one: VideoSpec = serde_json::from_value(serde_json::json!({
            "clips": [{"source": "a.mp4"}],
            "audio": {"source": "music.mp3", "volume": 0.5},
        }))
        .expect("single object parses");
        assert_eq!(one.audio.len(), 1);
        assert_eq!(one.audio[0].volume, 0.5);
        assert_eq!(one.audio[0].start, None);

        let none: VideoSpec =
            serde_json::from_value(serde_json::json!({"clips": [{"source": "a.mp4"}]}))
                .expect("absent parses");
        assert!(none.audio.is_empty());
    }

    #[test]
    fn closing_fade_starts_from_the_joined_length() {
        // 4 s + 6 s with a 0.5 s crossfade = 9.5 s total; a 1 s fade starts at 8.5 s.
        let f = filter_of(&plan(serde_json::json!({
            "clips": [
                {"source": "a.mp4"},
                {"source": "b.mp4", "transition": {"kind": "fade", "duration": 0.5}},
            ],
            "fade_out": 1.0,
        })));
        assert!(f.contains("fade=t=out:st=8.5:d=1"), "{f}");
    }

    #[test]
    fn intermediates_are_cleaned_so_only_the_video_is_delivered() {
        let p = plan(serde_json::json!({
            "clips": [{"source": "a.mp4"}, {"source": "b.mp4"}],
            "overlays": [{"type": "text", "text": "x"}],
        }));
        for f in ["n0.mp4", "n1.mp4", "filter.txt", "text0.txt"] {
            assert!(
                p.script.contains(&format!("rm -f {f:?}")),
                "{f} not cleaned:\n{}",
                p.script
            );
        }
        assert_eq!(p.output_name, "video.mp4");
    }

    // --- validation --------------------------------------------------------

    #[test]
    fn validation_names_the_offending_field() {
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (serde_json::json!({"clips": []}), "at least one clip"),
            (
                serde_json::json!({"clips": [{"source": "a.mp4", "start": 5.0, "end": 2.0}]}),
                "clips[0]",
            ),
            (
                serde_json::json!({"output": {"width": 1081}, "clips": [{"source": "a.mp4"}]}),
                "even",
            ),
            (
                serde_json::json!({"output": {"fps": 500}, "clips": [{"source": "a.mp4"}]}),
                "output.fps",
            ),
            (
                serde_json::json!({
                    "clips": [{"source": "a.mp4"}],
                    "overlays": [{"type": "text", "text": "x", "size": 900}],
                }),
                "overlays[0].size",
            ),
            (
                serde_json::json!({
                    "clips": [{"source": "a.mp4"}],
                    "audio": {"source": "m.mp3", "volume": 9.0},
                }),
                "audio[0].volume",
            ),
            (
                serde_json::json!({
                    "clips": [{"source": "a.mp4"}],
                    "audio": [{"source": "m.mp3"}, {"source": "v.mp3", "start": -1.0}],
                }),
                "audio[1].start",
            ),
        ];
        for (j, needle) in cases {
            let s: VideoSpec = serde_json::from_value(j.clone()).expect("parses");
            let err = s.validate().expect_err(&format!("{j} should be rejected"));
            let msg = format!("{err:?}");
            assert!(msg.contains(needle), "expected {needle:?} in {msg}");
        }
    }

    #[test]
    fn an_unresolvable_font_is_a_clear_error_not_an_ffmpeg_one() {
        let s = spec_from(serde_json::json!({
            "clips": [{"source": "a.mp4"}],
            "overlays": [{"type": "text", "text": "x", "font": "Comic Sans MS"}],
        }));
        s.validate().unwrap();
        let err = build_render_plan(&s, &inputs(), &probed()).expect_err("font missing");
        assert!(format!("{err:?}").contains("did not resolve to a font file"));
    }

    #[test]
    fn a_source_that_was_not_staged_is_reported() {
        let s = spec_from(serde_json::json!({"clips": [{"source": "nope.mp4"}]}));
        let err = build_render_plan(&s, &inputs(), &probed()).expect_err("unstaged");
        assert!(format!("{err:?}").contains("was not staged"));
    }

    #[test]
    fn unknown_fields_are_rejected_so_typos_surface() {
        // `deny_unknown_fields` turns a misspelled key into a parse error the
        // model can act on, instead of a silently ignored setting.
        let r: Result<VideoSpec, _> = serde_json::from_value(serde_json::json!({
            "clips": [{"source": "a.mp4", "trasition": {"kind": "fade"}}],
        }));
        assert!(r.is_err());
    }

    // --- pass 1 ------------------------------------------------------------

    #[test]
    fn probe_script_asks_for_durations_and_fonts() {
        let s = build_probe_script(&inputs()[..1], &["Inter:style=Bold".to_string()]);
        assert!(s.contains("ffprobe") && s.contains("in0.mp4"), "{s}");
        assert!(
            s.contains("fc-match") && s.contains("Inter:style=Bold"),
            "{s}"
        );
    }

    #[test]
    fn probe_output_is_parsed_and_odd_font_paths_are_dropped() {
        let p = parse_probe_output(
            "dur\tin0.mp4\t4.02\n\
             dur\tin1.mp4\t0\n\
             dur\timg2.png\tN/A\n\
             font\tInter:style=Bold\t/usr/share/fonts/Inter-Bold.otf\n\
             font\tEvil\t/x/font:with:colons.otf\n",
        );
        assert_eq!(p.durations.get("in0.mp4"), Some(&4.02));
        // A still image and an unreadable duration simply have none.
        assert!(!p.durations.contains_key("in1.mp4"));
        assert!(!p.durations.contains_key("img2.png"));
        assert_eq!(
            p.fonts.get("Inter:style=Bold").map(String::as_str),
            Some("/usr/share/fonts/Inter-Bold.otf")
        );
        // A path with a colon would end the drawtext option — refused.
        assert!(!p.fonts.contains_key("Evil"));
    }

    #[test]
    fn sources_are_listed_in_staging_order() {
        let s = spec_from(serde_json::json!({
            "clips": [{"source": "a.mp4"}, {"source": "b.mp4"}],
            "overlays": [{"type": "image", "source": "logo.png"}],
            "audio": {"source": "music.mp3"},
        }));
        assert_eq!(s.sources(), vec!["a.mp4", "b.mp4", "logo.png", "music.mp3"]);
    }
}

// ---------------------------------------------------------------------------
// The tool

#[derive(Deserialize)]
pub(crate) struct RenderVideoArgs {
    /// Inline timeline. Mutually exclusive with `document_id`.
    #[serde(default)]
    spec: Option<Value>,
    /// A `format: "json"` canvas document holding the timeline — the
    /// iterate-in-canvas, re-render-on-demand loop.
    #[serde(default)]
    document_id: Option<String>,
    #[serde(default)]
    version: Option<i64>,
    #[serde(default)]
    attachments: Vec<AttachmentArg>,
    #[serde(default)]
    filename: Option<String>,
}

pub struct RenderVideo(pub Arc<SandboxClient>);

impl Tool for RenderVideo {
    fn id(&self) -> &str {
        "render_video"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        // Two sandbox calls (probe + render), so allow for both.
        Some(self.0.loop_timeout() * 2)
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Assemble a video from a declarative JSON timeline: cut clips together with \
             transitions, burn in animated text, composite a logo, lay a music bed under \
             it. Built for ad/promo production, where the same look has to be reproducible \
             with different words in it — the same spec always renders the same video.\n\n\
             WORKFLOW — for anything the user will iterate on (which is nearly always), \
             write the timeline into a canvas document with `create_document` \
             (`format: \"json\"`) FIRST and pass its `document_id` here. Then a revision is \
             an edit to that document plus a re-render with the SAME document_id: the user \
             can also edit it by hand, and every version is kept. Use inline `spec` only \
             for a one-shot render.\n\n\
             Clip and overlay `source` values name attachments you also list in \
             `attachments` (an id `<turn>/<file>`, or a bare filename from earlier in the \
             conversation). Sources are staged into the sandbox for you.\n\n\
             SPEC — every field optional unless noted:\n\
             {\n  \"output\": {\"width\": 1920, \"height\": 1080, \"fps\": 30, \"crf\": 20},\n  \
             \"clips\": [ {\"source\": \"<attachment>\", \"start\": 0, \"end\": 4.5,\n              \
             \"transition\": {\"kind\": \"fade\", \"duration\": 0.5}} ],   // REQUIRED, in order\n  \
             \"overlays\": [\n    {\"type\": \"text\", \"text\": \"Your claim\", \"font\": \
             \"Inter:style=Bold\", \"size\": 48,\n     \"color\": \"#ffffff\", \"x\": \
             \"(w-tw)/2\", \"y\": \"h-h/6\", \"start\": 0.5, \"end\": 3.5,\n     \"animate_in\": \
             {\"kind\": \"slide_left\", \"duration\": 0.4},\n     \"animate_out\": {\"kind\": \
             \"fade\", \"duration\": 0.4},\n     \"box_color\": \"#00000080\", \"box_padding\": \
             12},\n    {\"type\": \"image\", \"source\": \"logo.png\", \"width\": 200, \"x\": \
             \"w-tw-40\", \"y\": \"40\",\n     \"start\": 1, \"end\": 5}\n  ],\n  \"audio\": [\n    \
             {\"source\": \"music.mp3\", \"volume\": 0.25, \"loudnorm\": true,\n     \
             \"fade_in\": 0.5, \"fade_out\": 1.0},\n    \
             {\"source\": \"line1.mp3\", \"loudnorm\": false},\n    \
             {\"source\": \"line2.mp3\", \"loudnorm\": false, \"start\": 4.0}\n  ],\n  \
             \"fade_out\": 0.5\n}\n\n\
             transition.kind: fade, dissolve, wipe_left/right/up/down, slide_left/right, \
             smooth_left/right, circle_open/close, or cut (no crossfade).\n\
             animate kind: fade, slide_left/right/up/down, scale, none.\n\
             x/y are ffmpeg expressions over `w`,`h` (frame) and `tw`,`th` (the text/image) \
             — `(w-tw)/2` centres, `h-h/6` is a lower third. Numbers and those variables \
             only; no function calls.\n\
             Clips are scaled and letterboxed onto the output size, so mixing portrait and \
             landscape footage is safe. Time values are seconds. Fonts must exist in the \
             sandbox: `Inter:style=Bold` and `DejaVu Sans:style=Bold` are good bets.\n\
             `audio` takes one track or several, mixed together. `start` places a track on \
             the timeline, which is how narration lines line up with the cuts they speak \
             over: synthesize each line with `comfyui_text_to_speech`, then give it the \
             start time of its scene. Leave `loudnorm` on for a music bed and OFF for \
             speech (it would level every line to the same loudness and flatten the \
             delivery). Audio never changes the video length — it is padded or cut to \
             fit.\n\n\
             The finished MP4 is attached to your reply. For a single still image use \
             `comfyui_*` image tools; for lip-synced talking heads use \
             `comfyui_talking_video`; for anything this spec cannot express, \
             `run_in_sandbox` has ffmpeg.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "spec": {
                        "type": "object",
                        "description": "Inline timeline (see the description). Use \
                                        `document_id` instead for anything iterative.",
                        "additionalProperties": true
                    },
                    "document_id": {
                        "type": "string",
                        "description": "Id of a `format: \"json\"` canvas document holding \
                                        the timeline. Preferred: edit + re-render keeps \
                                        working on the same document."
                    },
                    "version": {
                        "type": "integer",
                        "description": "With `document_id`: render this specific version \
                                        (default: latest)."
                    },
                    "attachments": {
                        "type": "array",
                        "description": "The clips, images and audio the spec references. \
                                        Every `source` in the spec must be listed here.",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["id"],
                            "properties": {
                                "id": {"type": "string", "description": "Attachment id \
                                       `<turn>/<file>`, or just a filename from earlier in \
                                       this conversation (newest match wins)."},
                                "name": {"type": "string", "description": "Filename to give \
                                         it in the working directory — use this when the \
                                         spec's `source` differs from the attachment name."}
                            }
                        }
                    },
                    "filename": {
                        "type": "string",
                        "description": "Optional output filename (`.mp4` is appended)."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: RenderVideoArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{spec? | document_id?, version?, attachments?, filename?}}: {e}"
                ))
            })?;

            // Exactly one source of truth: an inline spec XOR a canvas document.
            let (spec_value, canvas) = match (&args.spec, &args.document_id) {
                (Some(_), Some(_)) => {
                    return Err(ToolError::InvalidArgs(
                        "pass either `spec` or `document_id`, not both".into(),
                    ));
                }
                (None, None) => {
                    return Err(ToolError::InvalidArgs(
                        "pass `spec` (inline timeline) or `document_id` (a `format: \"json\"` \
                         canvas document holding one)"
                            .into(),
                    ));
                }
                (Some(s), None) => (s.clone(), None),
                (None, Some(doc_id)) => {
                    use gateway_core::server::db::documents::{self, DocumentFormat};
                    let session_id = ctx.session_id.as_deref().ok_or_else(|| {
                        ToolError::Failed(
                            "canvas documents are only available inside a chat session".into(),
                        )
                    })?;
                    let (doc, ver) =
                        documents::get_version(&ctx.db, session_id, doc_id, args.version)
                            .await
                            .map_err(|e| {
                                ToolError::Failed(format!("reading canvas document: {e}"))
                            })?
                            .ok_or_else(|| {
                                ToolError::InvalidArgs(format!(
                                    "no canvas document `{doc_id}` (v{:?}) in this conversation",
                                    args.version
                                ))
                            })?;
                    if doc.is_deleted() {
                        return Err(ToolError::InvalidArgs(format!(
                            "canvas document `{doc_id}` is deleted — call \
                             `undelete_document` first if you want to render it"
                        )));
                    }
                    if doc.format != DocumentFormat::Json {
                        return Err(ToolError::InvalidArgs(format!(
                            "canvas document `{doc_id}` is `{}` — render_video needs a \
                             `format: \"json\"` document holding the timeline",
                            doc.format.as_str()
                        )));
                    }
                    let v: Value = serde_json::from_str(&ver.content).map_err(|e| {
                        ToolError::InvalidArgs(format!(
                            "canvas document `{doc_id}` v{} is not valid JSON: {e}",
                            ver.version
                        ))
                    })?;
                    (v, Some((doc_id.clone(), ver.version)))
                }
            };

            let spec: VideoSpec = serde_json::from_value(spec_value).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "the timeline does not match the expected shape: {e}. See the tool \
                     description for the spec; unknown fields are rejected so typos surface."
                ))
            })?;
            spec.validate()?;

            // Stage the media, then rename each staged file to a generated name
            // so no caller-supplied filename can reach a command line.
            let Staged {
                files: staged_files,
                staged,
                available,
                mut notes,
                documents: attachment_documents,
            } = stage_attachments(&ctx, &args.attachments).await?;
            let mut files = staged_files;
            let _ =
                super::stage_documents(&ctx, &attachment_documents, &mut files, &mut notes).await;

            let (inputs, mut files) = map_sources(&spec, files)?;

            // --- pass 1: probe durations + resolve fonts -------------------
            let fonts = spec.fonts();
            let probe = RunRequest {
                language: Language::Bash,
                code: build_probe_script(&inputs, &fonts),
                files: std::mem::take(&mut files),
                timeout_secs: None,
                network: false,
                container_id: None,
                // Keep the container so pass 2 finds the staged media in /work
                // instead of uploading it twice.
                keep_alive: true,
            };
            let probe_resp = self.0.run_job(probe).await?;
            let lease = probe_resp.container_id.clone();
            let probed = parse_probe_output(&probe_resp.stdout);

            // --- pass 2: render -------------------------------------------
            let plan = match build_render_plan(&spec, &inputs, &probed) {
                Ok(p) => p,
                Err(e) => {
                    if let Some(id) = &lease {
                        self.0.release_container(id).await;
                    }
                    return Err(e);
                }
            };
            let mut script = plan.script;
            let out_name = match &args.filename {
                Some(f) => {
                    let stem = filename_stem(Some(f), "video");
                    let renamed = format!("{stem}.mp4");
                    if renamed != plan.output_name {
                        let _ = writeln!(
                            script,
                            "mv {from:?} {to:?}",
                            from = plan.output_name,
                            to = renamed
                        );
                    }
                    renamed
                }
                None => plan.output_name.clone(),
            };
            let _ = out_name;
            let render = RunRequest {
                language: Language::Bash,
                code: script,
                // Only the graph and the text files: the media is already in
                // /work from pass 1.
                files: plan
                    .files
                    .into_iter()
                    .map(|(name, bytes)| InputFile {
                        name,
                        content_b64: b64::encode(&bytes),
                    })
                    .collect(),
                timeout_secs: None,
                network: false,
                container_id: lease.clone(),
                keep_alive: false,
            };
            let mut out_val = match self.0.execute(&ctx, render).await {
                Ok(v) => v,
                Err(e) => {
                    if let Some(id) = &lease {
                        self.0.release_container(id).await;
                    }
                    return Err(e);
                }
            };
            augment_with_staging(&mut out_val, staged, available, notes);
            if let Some(obj) = out_val.as_object_mut() {
                if let Some((doc_id, ver)) = canvas {
                    obj.insert("canvas_document_id".into(), json!(doc_id));
                    obj.insert("canvas_version".into(), json!(ver));
                    obj.insert(
                        "canvas_note".into(),
                        json!(
                            "Rendered from the canvas document — to change the video, edit \
                             the document and re-render with the SAME document_id."
                        ),
                    );
                } else {
                    obj.insert(
                        "canvas_note".into(),
                        json!(
                            "Rendered from an inline spec. For anything the user will \
                             iterate on, put the timeline in a `format: \"json\"` canvas \
                             document and render by document_id instead."
                        ),
                    );
                }
            }
            Ok(out_val)
        })
    }
}

impl VideoSpec {
    /// Distinct font patterns the overlays ask for, so pass 1 can resolve them.
    pub(crate) fn fonts(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for o in &self.overlays {
            if let Overlay::Text { font, .. } = o
                && !v.contains(font)
            {
                v.push(font.clone());
            }
        }
        v
    }
}

/// Give every staged file a generated name and match it to the spec's sources.
///
/// This is what keeps attachment filenames out of the command line: after this,
/// the script only ever says `in0.mp4`. A source the caller forgot to attach is
/// reported by name, listing what *was* staged, because that is the mistake a
/// model makes most often here.
pub(crate) fn map_sources(
    spec: &VideoSpec,
    files: Vec<InputFile>,
) -> Result<(Vec<SafeInput>, Vec<InputFile>), ToolError> {
    let mut out_files: Vec<InputFile> = Vec::with_capacity(files.len());
    let mut inputs: Vec<SafeInput> = Vec::new();
    let mut used: Vec<usize> = Vec::new();

    for (n, source) in spec.sources().into_iter().enumerate() {
        // Already mapped (the same clip used twice) — reuse the safe name.
        if let Some(prev) = inputs.iter().find(|i| i.source == source) {
            let _ = prev;
            continue;
        }
        let idx = files
            .iter()
            .position(|f| f.name == source || f.name.ends_with(&format!("/{source}")))
            .or_else(|| {
                // Staging may have renamed on collision; fall back to a
                // basename match so `clip.mp4` still finds `clip-2.mp4`.
                let stem = source.rsplit('/').next().unwrap_or(source);
                files.iter().position(|f| f.name == stem)
            })
            .ok_or_else(|| {
                let have: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
                ToolError::InvalidArgs(format!(
                    "the spec references `{source}` but it was not staged — list it in \
                     `attachments`. Staged: {}",
                    if have.is_empty() {
                        "nothing".to_string()
                    } else {
                        have.join(", ")
                    }
                ))
            })?;
        if used.contains(&idx) {
            continue;
        }
        used.push(idx);
        let ext = files[idx]
            .name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or_else(|| "bin".into());
        let safe = format!("in{n}.{ext}");
        inputs.push(SafeInput {
            source: source.to_string(),
            safe_name: safe.clone(),
        });
        out_files.push(InputFile {
            name: safe,
            content_b64: files[idx].content_b64.clone(),
        });
    }
    // Anything staged but not referenced still goes in — a font or a subtitle
    // file the spec doesn't name is harmless and may be wanted later in the turn.
    for (i, f) in files.into_iter().enumerate() {
        if !used.contains(&i) {
            out_files.push(f);
        }
    }
    Ok((inputs, out_files))
}

#[cfg(test)]
mod e2e_dump {
    //! Not a test of behaviour — a way to get the *exact* scripts the tool
    //! generates onto disk so they can be run against real footage in the
    //! sandbox image. Unit tests prove the graph says what we intend; only
    //! ffmpeg can prove it means what we intend.
    //!
    //! ```sh
    //! VIDEO_DUMP_DIR=/tmp/vid \
    //!   VIDEO_DUMP_DURATIONS='in0.mp4=2.31,in1.mp4=2.31' \
    //!   cargo test -p gateway-runtime e2e_dump -- --ignored --nocapture
    //! ```

    use super::*;

    #[test]
    #[ignore = "developer tool: writes generated scripts for a manual sandbox run"]
    fn dump_scripts() {
        let Ok(dir) = std::env::var("VIDEO_DUMP_DIR") else {
            eprintln!("set VIDEO_DUMP_DIR to use this");
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();

        let spec: VideoSpec = serde_json::from_value(serde_json::json!({
            "output": {"width": 480, "height": 832, "fps": 24, "crf": 20},
            "clips": [
                {"source": "a.mp4"},
                {"source": "b.mp4", "transition": {"kind": "wipe_left", "duration": 0.5}},
            ],
            "overlays": [
                {"type": "text", "text": "CROIT STORAGE: 9,99 €/TB \"jetzt\"",
                 "font": "Inter:style=Bold", "size": 34, "color": "#ffffff",
                 "x": "(w-tw)/2", "y": "h-h/5", "start": 0.2, "end": 3.0,
                 "animate_in": {"kind": "slide_left", "duration": 0.4},
                 "animate_out": {"kind": "fade", "duration": 0.5},
                 "box_color": "#00000080", "box_padding": 14},
                {"type": "text", "text": "Ceph. Einfach.", "size": 28,
                 "y": "h/2", "start": 3.2,
                 "animate_in": {"kind": "scale", "duration": 0.5}},
                {"type": "image", "source": "logo.png", "width": 90,
                 "x": "w-100", "y": "30", "start": 0.5},
            ],
            // A music bed plus two narration lines, the second offset onto the
            // scene it belongs to — the mix `amix`/`adelay`/`apad` has to get
            // right, and which no unit test can prove is valid ffmpeg.
            "audio": [
                {"source": "music.mp3", "volume": 0.25, "fade_in": 0.3, "fade_out": 0.8},
                {"source": "line1.mp3", "loudnorm": false},
                {"source": "line2.mp3", "loudnorm": false, "start": 2.5},
            ],
            "fade_out": 0.5,
        }))
        .unwrap();
        spec.validate().unwrap();

        let inputs = vec![
            SafeInput {
                source: "a.mp4".into(),
                safe_name: "in0.mp4".into(),
            },
            SafeInput {
                source: "b.mp4".into(),
                safe_name: "in1.mp4".into(),
            },
            SafeInput {
                source: "logo.png".into(),
                safe_name: "in2.png".into(),
            },
            SafeInput {
                source: "music.mp3".into(),
                safe_name: "in3.mp3".into(),
            },
            SafeInput {
                source: "line1.mp3".into(),
                safe_name: "in4.mp3".into(),
            },
            SafeInput {
                source: "line2.mp3".into(),
                safe_name: "in5.mp3".into(),
            },
        ];
        let mut probed = Probed::default();
        for pair in std::env::var("VIDEO_DUMP_DURATIONS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
        {
            let (k, v) = pair.split_once('=').expect("name=seconds");
            probed.durations.insert(k.to_string(), v.parse().unwrap());
        }
        probed.fonts.insert(
            "Inter:style=Bold".into(),
            std::env::var("VIDEO_DUMP_FONT")
                .unwrap_or_else(|_| "/usr/share/fonts/opentype/inter/Inter-Bold.otf".into()),
        );

        std::fs::write(
            format!("{dir}/probe.sh"),
            build_probe_script(&inputs, &spec.fonts()),
        )
        .unwrap();
        let plan = build_render_plan(&spec, &inputs, &probed).unwrap();
        std::fs::write(format!("{dir}/render.sh"), &plan.script).unwrap();
        for (name, bytes) in &plan.files {
            std::fs::write(format!("{dir}/{name}"), bytes).unwrap();
        }
        eprintln!(
            "wrote probe.sh, render.sh and {} file(s) to {dir}",
            plan.files.len()
        );
    }
}
