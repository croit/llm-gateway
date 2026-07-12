// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `generate_qr_code` — native QR code generation.
//!
//! Encodes the payload with the pure-Rust `qrcode` crate and renders
//! PNG/SVG in-process, so the tool needs neither a `[sandbox]` deployment
//! nor a network roundtrip — a QR code is deterministic pixel work, not
//! code execution. The model composes the payload itself (URL, WiFi,
//! vCard, EPC/GiroCode, … — the schema description documents the common
//! formats), which keeps the schema small while covering every variant.
//!
//! Styling is deliberately flexible: module colors, scale, quiet zone,
//! and an optional centered logo pulled from a chat attachment (error
//! correction is forced to H then, so up to 30% of the code may be
//! covered and it still scans). Delivery mirrors `generate_image`: the
//! file is uploaded as a chat attachment and a `[gw-attachment …]` marker
//! is spliced into the assistant turn, so the code renders inline in the
//! message bubble; the model only gets compact metadata back.

use std::io::Cursor;

use image::{DynamicImage, Rgba, RgbaImage, imageops};
use qrcode::{Color as QrColor, EcLevel, QrCode};
use serde::Deserialize;
use serde_json::{Value, json};
use session_core::db as chat;
use shared::api::ToolDef;

use super::sandbox::b64;
use super::{Tool, ToolContext, ToolError, ToolFuture};
use crate::server::chat_attachments;

/// Byte-mode capacity of the largest QR code (version 40) at EC level L.
/// Anything longer can never encode; reject it with a clear message
/// before handing it to the encoder.
const MAX_DATA_BYTES: usize = 2953;

/// Default / bounds for `scale` (pixels per module in the PNG; size hint
/// in the SVG). 12 px per module puts a typical URL code around 450 px —
/// crisp on screen and printable.
const DEFAULT_SCALE: u32 = 12;
const MIN_SCALE: u32 = 2;
const MAX_SCALE: u32 = 40;

/// Ceiling on the rendered PNG edge. A version-40 code at max scale would
/// be a ~280 MB bitmap; capping the edge keeps memory bounded while
/// silently shrinking `scale` only in that pathological corner.
const MAX_PNG_EDGE_PX: u32 = 4096;

/// Default / max quiet zone (border) in modules. The QR spec asks for 4;
/// allowing 0 supports embedding into layouts that bring their own margin.
const DEFAULT_BORDER: u32 = 4;
const MAX_BORDER: u32 = 16;

/// Edge of the (square) logo box as a percentage of the code's module
/// area. 22% covers well under the 30% of modules EC level H can
/// reconstruct, leaving margin for the light padding plate around it.
const LOGO_BOX_PCT: u32 = 22;

/// Longest edge of the logo raster embedded into an SVG's data URI.
/// Bounds the base64 blob; the SVG scales it to the logo box anyway.
const SVG_LOGO_MAX_PX: u32 = 512;

#[derive(Deserialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
enum QrFormat {
    #[default]
    Png,
    Svg,
}

impl QrFormat {
    fn ext(self) -> &'static str {
        match self {
            QrFormat::Png => "png",
            QrFormat::Svg => "svg",
        }
    }
    fn mime(self) -> &'static str {
        match self {
            QrFormat::Png => "image/png",
            QrFormat::Svg => "image/svg+xml",
        }
    }
}

/// Model-facing error-correction argument. Mirrors [`EcLevel`] but owns
/// the serde surface (uppercase letters, `M` default) so the schema enum
/// and the encoder level can't drift apart.
#[derive(Deserialize, Clone, Copy, Default, PartialEq, Debug)]
enum EcArg {
    L,
    #[default]
    M,
    Q,
    H,
}

impl EcArg {
    fn level(self) -> EcLevel {
        match self {
            EcArg::L => EcLevel::L,
            EcArg::M => EcLevel::M,
            EcArg::Q => EcLevel::Q,
            EcArg::H => EcLevel::H,
        }
    }
    fn letter(self) -> &'static str {
        match self {
            EcArg::L => "L",
            EcArg::M => "M",
            EcArg::Q => "Q",
            EcArg::H => "H",
        }
    }
}

#[derive(Deserialize)]
struct QrArgs {
    data: String,
    #[serde(default)]
    format: QrFormat,
    #[serde(default)]
    error_correction: EcArg,
    #[serde(default)]
    scale: Option<u32>,
    #[serde(default)]
    border: Option<u32>,
    #[serde(default)]
    dark_color: Option<String>,
    #[serde(default)]
    light_color: Option<String>,
    #[serde(default)]
    logo_attachment_id: Option<String>,
    #[serde(default)]
    filename: Option<String>,
}

/// Resolved render parameters — pure data so the render functions stay
/// unit-testable without a [`ToolContext`].
struct RenderSpec {
    scale: u32,
    border: u32,
    dark: [u8; 4],
    light: [u8; 4],
}

/// Parse `#RGB` / `#RRGGBB` / `#RRGGBBAA` (case-insensitive), plus
/// `transparent` when the caller allows it (only sensible for the light
/// color — a transparent foreground can't scan).
fn parse_color(s: &str, allow_transparent: bool) -> Result<[u8; 4], String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("transparent") {
        if allow_transparent {
            return Ok([0, 0, 0, 0]);
        }
        return Err("only light_color may be `transparent`".into());
    }
    let hex = s
        .strip_prefix('#')
        .ok_or_else(|| format!("`{s}` is not a hex color (`#RGB`, `#RRGGBB`, `#RRGGBBAA`)"))?;
    let nibble = |c: u8| -> Result<u8, String> {
        (c as char)
            .to_digit(16)
            .map(|d| d as u8)
            .ok_or_else(|| format!("`{s}` contains a non-hex digit"))
    };
    let b = hex.as_bytes();
    match b.len() {
        3 => {
            let mut out = [0u8; 4];
            for (i, &c) in b.iter().enumerate() {
                let n = nibble(c)?;
                out[i] = n << 4 | n;
            }
            out[3] = 255;
            Ok(out)
        }
        6 | 8 => {
            let mut out = [0, 0, 0, 255];
            for i in 0..b.len() / 2 {
                out[i] = nibble(b[2 * i])? << 4 | nibble(b[2 * i + 1])?;
            }
            Ok(out)
        }
        _ => Err(format!(
            "`{s}` has {} hex digits — expected 3, 6, or 8",
            b.len()
        )),
    }
}

/// Relative luminance in 0..=1 (sRGB weights, gamma ignored — this feeds
/// a coarse scannability heuristic, not color science).
fn luminance(c: [u8; 4]) -> f32 {
    (0.2126 * f32::from(c[0]) + 0.7152 * f32::from(c[1]) + 0.0722 * f32::from(c[2])) / 255.0
}

/// A human-readable scannability warning for risky color choices, or
/// `None` when the combination is fine. Fully transparent light counts
/// as white (the code usually lands on a light page).
fn contrast_warning(dark: [u8; 4], light: [u8; 4]) -> Option<String> {
    let light = if light[3] == 0 {
        [255, 255, 255, 255]
    } else {
        light
    };
    let (dl, ll) = (luminance(dark), luminance(light));
    if dl >= ll {
        return Some(
            "dark_color is not darker than light_color — inverted QR codes fail on many \
             scanners; swap the colors"
                .into(),
        );
    }
    if ll - dl < 0.4 {
        return Some(
            "low contrast between dark_color and light_color — the code may be hard to scan".into(),
        );
    }
    None
}

/// CSS color + optional opacity for an RGBA value, for the SVG output.
fn css_color(c: [u8; 4]) -> (String, Option<f32>) {
    let hex = format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]);
    let op = if c[3] == 255 {
        None
    } else {
        Some(f32::from(c[3]) / 255.0)
    };
    (hex, op)
}

/// Render the code to an RGBA PNG. `logo` (already decoded) is centered
/// on a light padding plate sized [`LOGO_BOX_PCT`] of the module area.
fn render_png(
    code: &QrCode,
    spec: &RenderSpec,
    logo: Option<&DynamicImage>,
) -> Result<Vec<u8>, String> {
    let w = u32::try_from(code.width()).map_err(|_| "code width overflow".to_string())?;
    let units = w + 2 * spec.border;
    // Keep the bitmap bounded even for version-40 codes at max scale.
    let scale = spec.scale.min((MAX_PNG_EDGE_PX / units).max(1));
    let total = units * scale;
    let mut img = RgbaImage::from_pixel(total, total, Rgba(spec.light));
    for (i, c) in code.to_colors().iter().enumerate() {
        if *c != QrColor::Dark {
            continue;
        }
        let i = u32::try_from(i).expect("module index fits u32");
        let mx = (i % w + spec.border) * scale;
        let my = (i / w + spec.border) * scale;
        for y in my..my + scale {
            for x in mx..mx + scale {
                img.put_pixel(x, y, Rgba(spec.dark));
            }
        }
    }
    if let Some(logo) = logo {
        let area = w * scale;
        let box_px = (area * LOGO_BOX_PCT / 100).max(3);
        // One module of light padding between logo and modules keeps the
        // plate visually separated; a transparent light gets a white
        // plate so the logo never floats on the modules themselves.
        let pad = scale;
        let inner = box_px.saturating_sub(2 * pad).max(1);
        let plate = if spec.light[3] == 0 {
            [255, 255, 255, 255]
        } else {
            spec.light
        };
        let b0 = (total - box_px) / 2;
        for y in b0..b0 + box_px {
            for x in b0..b0 + box_px {
                img.put_pixel(x, y, Rgba(plate));
            }
        }
        let scaled = logo.resize(inner, inner, imageops::FilterType::Lanczos3);
        let lx = i64::from((total - scaled.width()) / 2);
        let ly = i64::from((total - scaled.height()) / 2);
        imageops::overlay(&mut img, &scaled, lx, ly);
    }
    let mut buf = Vec::new();
    DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| format!("PNG encode failed: {e}"))?;
    Ok(buf)
}

/// Render the code as a standalone SVG. Modules become one `<path>` (a
/// square per dark module in a viewBox of module units); a logo is
/// embedded as a base64 PNG `<image>` on a light plate.
fn render_svg(code: &QrCode, spec: &RenderSpec, logo: Option<&DynamicImage>) -> String {
    let w = code.width() as u32;
    let units = w + 2 * spec.border;
    let px = units * spec.scale;
    let (dark_hex, dark_op) = css_color(spec.dark);
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {units} {units}\" \
         width=\"{px}\" height=\"{px}\" shape-rendering=\"crispEdges\">"
    );
    if spec.light[3] > 0 {
        let (hex, op) = css_color(spec.light);
        let op = op
            .map(|o| format!(" fill-opacity=\"{o:.3}\""))
            .unwrap_or_default();
        svg.push_str(&format!(
            "<rect width=\"{units}\" height=\"{units}\" fill=\"{hex}\"{op}/>"
        ));
    }
    let mut d = String::new();
    for (i, c) in code.to_colors().iter().enumerate() {
        if *c == QrColor::Dark {
            let i = i as u32;
            let (x, y) = (i % w + spec.border, i / w + spec.border);
            d.push_str(&format!("M{x} {y}h1v1h-1z"));
        }
    }
    let dark_op = dark_op
        .map(|o| format!(" fill-opacity=\"{o:.3}\""))
        .unwrap_or_default();
    svg.push_str(&format!("<path fill=\"{dark_hex}\"{dark_op} d=\"{d}\"/>"));
    if let Some(logo) = logo {
        // Box + pad in module units, mirroring the PNG proportions.
        let box_u = f64::from(w) * f64::from(LOGO_BOX_PCT) / 100.0;
        let pad = 1.0;
        let inner = (box_u - 2.0 * pad).max(0.5);
        let b0 = (f64::from(units) - box_u) / 2.0;
        let i0 = (f64::from(units) - inner) / 2.0;
        let plate = if spec.light[3] == 0 {
            [255, 255, 255, 255]
        } else {
            spec.light
        };
        let (phex, pop) = css_color(plate);
        let pop = pop
            .map(|o| format!(" fill-opacity=\"{o:.3}\""))
            .unwrap_or_default();
        // Rasterize the logo once at a bounded size; the SVG scales it.
        let scaled = logo.resize(
            SVG_LOGO_MAX_PX,
            SVG_LOGO_MAX_PX,
            imageops::FilterType::Lanczos3,
        );
        let mut png = Vec::new();
        if scaled
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .is_ok()
        {
            svg.push_str(&format!(
                "<rect x=\"{b0:.3}\" y=\"{b0:.3}\" width=\"{box_u:.3}\" height=\"{box_u:.3}\" \
                 fill=\"{phex}\"{pop}/>\
                 <image x=\"{i0:.3}\" y=\"{i0:.3}\" width=\"{inner:.3}\" height=\"{inner:.3}\" \
                 preserveAspectRatio=\"xMidYMid meet\" \
                 href=\"data:image/png;base64,{}\"/>",
                b64::encode(&png)
            ));
        }
    }
    svg.push_str("</svg>");
    svg
}

/// Safe output filename stem from an optional model-supplied name: strip
/// any extension (the format supplies it), reject path separators, fall
/// back to `qr-code`.
fn filename_stem(supplied: Option<&str>) -> String {
    supplied
        .and_then(|f| f.rsplit_once('.').map(|(s, _)| s).or(Some(f)))
        .map(str::trim)
        .filter(|s| {
            !s.is_empty() && *s != "." && *s != ".." && !s.contains('/') && !s.contains('\\')
        })
        .map(str::to_string)
        .unwrap_or_else(|| "qr-code".to_string())
}

pub struct GenerateQrCode;

impl Tool for GenerateQrCode {
    fn id(&self) -> &str {
        "generate_qr_code"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Generate a QR code (PNG or SVG) and attach it inline to your reply — \
             rendered natively by the gateway, instant and always available. Put the \
             EXACT payload a scanner should receive in `data`, composed for the use \
             case: a URL (`https://…`); WiFi access `WIFI:T:WPA;S:<ssid>;P:<password>;;` \
             (backslash-escape `;`, `,`, `:`, `\\\"` inside values); a contact as vCard \
             (`BEGIN:VCARD\\nVERSION:3.0\\nN:Last;First\\nFN:First Last\\nTEL:+49…\\n\
             EMAIL:…\\nEND:VCARD`) or compact `MECARD:N:Last,First;TEL:…;;`; \
             `mailto:user@example.com`, `tel:+49…`, `SMSTO:+49…:<text>`, \
             `geo:52.52,13.405`; a calendar entry as a `BEGIN:VEVENT` block; a SEPA \
             transfer (GiroCode/EPC): the lines `BCD`, `002`, `1`, `SCT`, `<BIC>`, \
             `<recipient>`, `<IBAN>`, `EUR<amount>` joined with `\\n`. Shorter data \
             scans easier. Styling: `dark_color`/`light_color` (hex — keep dark-on-\
             light with strong contrast), `scale` (pixels per module), `border` \
             (quiet-zone modules; keep ≥ 4 for reliable scanning), and an optional \
             centered logo via `logo_attachment_id` (an image attachment from this \
             conversation; error correction is raised to H automatically so the \
             covered modules stay recoverable). The finished file appears inline in \
             your message — do not repeat any marker text.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["data"],
                "properties": {
                    "data": {
                        "type": "string",
                        "description": "The exact payload the scanner receives (see the \
                                        format recipes in the tool description)."
                    },
                    "format": {
                        "type": "string", "enum": ["png", "svg"],
                        "description": "Output file format. Default png; svg stays crisp \
                                        at any print size."
                    },
                    "error_correction": {
                        "type": "string", "enum": ["L", "M", "Q", "H"],
                        "description": "Error-correction level (L≈7% … H≈30% damage \
                                        recoverable). Default M; forced to H when a \
                                        logo is embedded."
                    },
                    "scale": {
                        "type": "integer", "minimum": 2, "maximum": 40,
                        "description": "Pixels per module. Default 12 (≈450 px for a \
                                        typical URL code)."
                    },
                    "border": {
                        "type": "integer", "minimum": 0, "maximum": 16,
                        "description": "Quiet-zone width in modules. Default 4 (the spec \
                                        minimum for reliable scanning)."
                    },
                    "dark_color": {
                        "type": "string",
                        "description": "Module color as `#RGB`/`#RRGGBB`/`#RRGGBBAA`. \
                                        Default #000000."
                    },
                    "light_color": {
                        "type": "string",
                        "description": "Background color (same formats, or `transparent`). \
                                        Default #FFFFFF."
                    },
                    "logo_attachment_id": {
                        "type": "string",
                        "description": "Optional image from this conversation to center on \
                                        the code: an attachment id (`<turn>/<file>`) or just \
                                        its filename — newest match wins (png/jpeg/webp)."
                    },
                    "filename": {
                        "type": "string",
                        "description": "Optional output filename (extension is set from \
                                        `format`)."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: QrArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{data, format?, error_correction?, scale?, border?, \
                     dark_color?, light_color?, logo_attachment_id?, filename?}}: {e}"
                ))
            })?;
            if args.data.is_empty() {
                return Err(ToolError::InvalidArgs("`data` must not be empty".into()));
            }
            if args.data.len() > MAX_DATA_BYTES {
                return Err(ToolError::InvalidArgs(format!(
                    "`data` is {} bytes — no QR code can hold more than {MAX_DATA_BYTES}; \
                     shorten the payload (e.g. use a shorter URL)",
                    args.data.len()
                )));
            }
            let spec = RenderSpec {
                scale: args
                    .scale
                    .unwrap_or(DEFAULT_SCALE)
                    .clamp(MIN_SCALE, MAX_SCALE),
                border: args.border.unwrap_or(DEFAULT_BORDER).min(MAX_BORDER),
                dark: parse_color(args.dark_color.as_deref().unwrap_or("#000000"), false)
                    .map_err(|e| ToolError::InvalidArgs(format!("dark_color: {e}")))?,
                light: parse_color(args.light_color.as_deref().unwrap_or("#FFFFFF"), true)
                    .map_err(|e| ToolError::InvalidArgs(format!("light_color: {e}")))?,
            };

            // Attachment side effects live only on the chat path — same
            // preconditions as `generate_image`, checked before any work.
            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "chat attachments are not configured on this gateway \
                     (operator must set [chat.s3]) — nowhere to store the QR code"
                        .into(),
                )
            })?;
            let turn_id = ctx.assistant_turn_id.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "generate_qr_code is only available inside a chat session — \
                     there's no assistant turn to attach the file to"
                        .into(),
                )
            })?;
            let reservations = ctx.attachment_reservations.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "generate_qr_code requires a per-turn attachment-reservation \
                     set, which is only initialised on the chat-page path"
                        .into(),
                )
            })?;

            let logo = match args.logo_attachment_id.as_deref() {
                Some(id) => Some(fetch_logo(&ctx, s3, id).await?),
                None => None,
            };
            // A logo covers modules; H recovers up to 30% of them, so the
            // requested level is overridden rather than honored-and-broken.
            let (ec, ec_forced) = if logo.is_some() && args.error_correction != EcArg::H {
                (EcArg::H, true)
            } else {
                (args.error_correction, false)
            };

            let code = QrCode::with_error_correction_level(args.data.as_bytes(), ec.level())
                .map_err(|e| {
                    ToolError::InvalidArgs(format!(
                        "could not encode `data` as a QR code: {e} — shorten the payload \
                         or lower `error_correction`"
                    ))
                })?;

            let bytes = match args.format {
                QrFormat::Png => {
                    render_png(&code, &spec, logo.as_ref()).map_err(ToolError::Failed)?
                }
                QrFormat::Svg => render_svg(&code, &spec, logo.as_ref()).into_bytes(),
            };

            let base = format!(
                "{}.{}",
                filename_stem(args.filename.as_deref()),
                args.format.ext()
            );
            let filename =
                chat_attachments::reserve_filename(&ctx.db, turn_id, reservations, &base)
                    .await
                    .map_err(|e| ToolError::Failed(format!("reserve filename: {e}")))?;
            let size = bytes.len() as u64;
            let outcome =
                chat_attachments::upload(s3, turn_id, &filename, args.format.mime(), bytes)
                    .await
                    .map_err(|e| ToolError::Failed(format!("s3 upload failed: {e}")))?;
            let marker = chat_attachments::marker_line(turn_id, &outcome);
            chat::append_content(&ctx.db, turn_id, &format!("\n\n{marker}\n\n"))
                .await
                .map_err(|e| ToolError::Failed(format!("persist marker: {e}")))?;

            let mut out = json!({
                "filename": outcome.filename,
                "mime": args.format.mime(),
                "size": size,
                "id": format!("{turn_id}/{}", outcome.filename),
                "modules": code.width(),
                "error_correction": ec.letter(),
                "rendered": "Inline in your message bubble — do NOT repeat the marker \
                             text or describe the image in your prose.",
            });
            if ec_forced {
                out["note"] =
                    json!("error_correction was raised to H so the embedded logo stays scannable");
            }
            if let Some(w) = contrast_warning(spec.dark, spec.light) {
                out["warning"] = json!(w);
            }
            Ok(out)
        })
    }
}

/// Resolve + decode a logo attachment: the id must belong to this chat
/// session (same scoping rule as the sandbox's attachment staging), and
/// the bytes must decode as an image with an enabled codec.
async fn fetch_logo(
    ctx: &ToolContext,
    s3: &crate::server::config::S3Config,
    id: &str,
) -> Result<DynamicImage, ToolError> {
    let session_id = ctx.session_id.as_deref().ok_or_else(|| {
        ToolError::Failed("logo embedding needs a chat session to resolve attachments".into())
    })?;
    let (session_atts, _) = chat_attachments::session_and_round_attachments(&ctx.db, session_id)
        .await
        .map_err(|e| ToolError::Failed(format!("listing session attachments: {e}")))?;
    // Exact id or bare filename (newest wins) — same loose resolution as
    // the sandbox staging, so a logo from an earlier turn is reusable
    // without the model tracking turn ids.
    let resolved = chat_attachments::resolve_attachment(&session_atts, id)
        .ok_or_else(|| {
            ToolError::InvalidArgs(format!(
                "no attachment with id or filename `{id}` in this conversation"
            ))
        })?
        .clone();
    let (turn, filename) = resolved
        .id
        .split_once('/')
        .ok_or_else(|| ToolError::InvalidArgs(format!("malformed attachment id `{id}`")))?;
    let fetched = chat_attachments::fetch(s3, turn, filename)
        .await
        .map_err(|e| ToolError::Failed(format!("fetching logo `{id}`: {e}")))?;
    image::load_from_memory(&fetched.bytes).map_err(|e| {
        ToolError::InvalidArgs(format!(
            "could not decode logo `{id}` as an image (png/jpeg/webp are supported): {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Decode a rendered PNG back to its payload with an independent
    /// decoder — the real scannability check.
    fn decode_png(png: &[u8]) -> String {
        let img = image::load_from_memory(png)
            .expect("png decodes")
            .to_luma8();
        let mut prepared = rqrr::PreparedImage::prepare(img);
        let grids = prepared.detect_grids();
        assert_eq!(grids.len(), 1, "exactly one QR code detected");
        let (_meta, content) = grids[0].decode().expect("QR decodes");
        content
    }

    fn spec() -> RenderSpec {
        RenderSpec {
            scale: 8,
            border: 4,
            dark: [0, 0, 0, 255],
            light: [255, 255, 255, 255],
        }
    }

    #[test]
    fn schema_names_match_id() {
        assert_eq!(GenerateQrCode.id(), GenerateQrCode.schema().function.name);
    }

    #[test]
    fn png_round_trips_through_an_independent_decoder() {
        let data = "https://example.com/some/path?x=1";
        let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M).unwrap();
        let png = render_png(&code, &spec(), None).unwrap();
        assert_eq!(decode_png(&png), data);
    }

    #[test]
    fn colored_png_still_decodes() {
        let data = "WIFI:T:WPA;S:croit;P:secret;;";
        let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M).unwrap();
        let spec = RenderSpec {
            scale: 8,
            border: 4,
            dark: [0x1d, 0x1d, 0x1b, 255],
            light: [0xf8, 0xfa, 0xfc, 255],
        };
        let png = render_png(&code, &spec, None).unwrap();
        assert_eq!(decode_png(&png), data);
    }

    #[test]
    fn png_with_logo_still_decodes_at_ec_h() {
        let data = "https://croit.io/";
        // EC H is what run() forces whenever a logo is embedded.
        let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::H).unwrap();
        let logo =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(100, 100, Rgba([200, 30, 30, 255])));
        let png = render_png(&code, &spec(), Some(&logo)).unwrap();
        assert_eq!(decode_png(&png), data);
    }

    #[test]
    fn svg_contains_modules_and_embedded_logo() {
        let code = QrCode::with_error_correction_level(b"https://croit.io/", EcLevel::H).unwrap();
        let logo =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(64, 64, Rgba([10, 60, 200, 255])));
        let svg = render_svg(&code, &spec(), Some(&logo));
        assert!(svg.starts_with("<svg "), "{}", &svg[..60]);
        assert!(svg.contains("<path fill=\"#000000\""));
        assert!(svg.contains("data:image/png;base64,"));
        assert!(svg.ends_with("</svg>"));
    }

    #[test]
    fn svg_transparent_light_omits_background_rect() {
        let code = QrCode::with_error_correction_level(b"x", EcLevel::M).unwrap();
        let spec = RenderSpec {
            scale: 8,
            border: 4,
            dark: [0, 0, 0, 255],
            light: [0, 0, 0, 0],
        };
        let svg = render_svg(&code, &spec, None);
        assert!(
            !svg.contains("<rect"),
            "transparent background must not draw a rect: {svg}"
        );
    }

    #[test]
    fn parse_color_accepts_common_forms() {
        assert_eq!(parse_color("#000", false).unwrap(), [0, 0, 0, 255]);
        assert_eq!(
            parse_color("#1D4ed8", false).unwrap(),
            [0x1d, 0x4e, 0xd8, 255]
        );
        assert_eq!(
            parse_color("#11223344", false).unwrap(),
            [0x11, 0x22, 0x33, 0x44]
        );
        assert_eq!(parse_color("transparent", true).unwrap(), [0, 0, 0, 0]);
        assert!(parse_color("transparent", false).is_err());
        assert!(parse_color("red", false).is_err());
        assert!(parse_color("#12345", false).is_err());
        assert!(parse_color("#GGHHII", false).is_err());
    }

    #[test]
    fn contrast_warnings_flag_risky_combinations() {
        let black = [0, 0, 0, 255];
        let white = [255, 255, 255, 255];
        let yellow = [255, 220, 0, 255];
        assert!(contrast_warning(black, white).is_none());
        // Inverted: light modules on dark background.
        assert!(contrast_warning(white, black).unwrap().contains("inverted"));
        // Yellow-on-white: legal but risky.
        assert!(
            contrast_warning(yellow, white)
                .unwrap()
                .contains("low contrast")
        );
        // Transparent light is judged as white.
        assert!(contrast_warning(black, [0, 0, 0, 0]).is_none());
    }

    #[test]
    fn filename_stem_sanitizes_and_defaults() {
        assert_eq!(filename_stem(None), "qr-code");
        assert_eq!(filename_stem(Some("wifi.png")), "wifi");
        assert_eq!(filename_stem(Some("  ")), "qr-code");
        assert_eq!(filename_stem(Some("../evil")), "qr-code");
    }

    async fn chatless_ctx() -> ToolContext {
        ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: crate::server::db::open(std::path::Path::new(":memory:"))
                .await
                .unwrap(),
            s3: None,
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
            image_gen: None,
        }
    }

    #[tokio::test]
    async fn rejects_empty_and_oversized_data() {
        let ctx = chatless_ctx().await;
        let err = GenerateQrCode
            .run(ctx.clone(), json!({ "data": "" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
        let err = GenerateQrCode
            .run(ctx, json!({ "data": "x".repeat(4000) }))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs(msg) => assert!(msg.contains("no QR code can hold"), "{msg}"),
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_bad_colors_before_touching_storage() {
        let ctx = chatless_ctx().await;
        let err = GenerateQrCode
            .run(ctx, json!({ "data": "x", "dark_color": "blue" }))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs(msg) => assert!(msg.contains("dark_color"), "{msg}"),
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn errors_cleanly_off_the_chat_path() {
        let ctx = chatless_ctx().await;
        let err = GenerateQrCode
            .run(ctx, json!({ "data": "https://croit.io/" }))
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => {
                assert!(msg.contains("chat attachments are not configured"), "{msg}")
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
