// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! PDF reading for the `fetch_attachment` tool. Two model-driven
//! tiers, both operating on the raw bytes the gateway already pulled
//! from S3 (no presigned URL ever crosses the wire to the LLM):
//!
//! - [`extract_text`] — the cheap default. `pdf-extract` (pure Rust,
//!   via `lopdf`) lifts the PDF's text layer out in memory. A normal
//!   "born-digital" PDF reads like any other text attachment. A
//!   scanned / image-only PDF has no text layer, so this returns an
//!   empty (or near-empty) string — the caller surfaces that as a
//!   signal that the model should escalate.
//! - [`render_pages`] — the escalation tier. `pdfium-render` drives
//!   Chromium's `pdfium` to rasterise each page to a PNG, which the
//!   caller ships as `image_url` content parts so a vision model can
//!   *look* at a page the text layer couldn't describe.
//!
//! Both calls are CPU-bound and synchronous (and `pdfium`'s handles
//! are `!Send`), so callers must run them on a blocking thread via
//! `tokio::task::spawn_blocking`.
//!
//! `pdfium` is bound *dynamically at runtime* — the build never needs
//! the native library. When it's absent we return
//! [`PdfError::RendererUnavailable`] so the tool degrades to a clean
//! note instead of crashing the request.

use std::io::Cursor;

use pdfium_render::prelude::{PdfRenderConfig, Pdfium};

/// Default page-count ceiling for [`render_pages`]. Rasterised pages
/// are base64'd into the model's context as inline images, which is
/// expensive; capping keeps a 200-page PDF from blowing the context
/// (and the gateway's memory) in one tool call. The caller tells the
/// model how many of how many pages it actually got back.
pub const DEFAULT_MAX_RENDER_PAGES: usize = 8;

/// Target raster width in pixels per page. ~1240px is roughly 150 DPI
/// for an A4/Letter page — legible for a vision model without
/// ballooning the PNG. Height scales to preserve aspect ratio
/// (`set_maximum_height` only clamps absurdly tall pages).
const RENDER_TARGET_WIDTH: i32 = 1240;
const RENDER_MAX_HEIGHT: i32 = 1754;
const MAX_RENDER_TARGET_WIDTH: i32 = 4096;
const MAX_RENDER_HEIGHT: i32 = 8192;
const MAX_RENDER_PAGES: usize = 64;

/// Bounded rasterization settings shared by vision and OCR consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderSettings {
    pub max_pages: usize,
    pub target_width: i32,
    pub max_height: i32,
    /// 1-based page to start at. Together with `max_pages` this is the
    /// window the caller gets. Without it a document longer than
    /// `max_pages` had its tail permanently out of reach — the renderer
    /// always started at page 1, so page 150 of a scan could not be
    /// looked at by any sequence of calls. `0` is treated as `1`.
    pub first_page: usize,
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            max_pages: DEFAULT_MAX_RENDER_PAGES,
            target_width: RENDER_TARGET_WIDTH,
            max_height: RENDER_MAX_HEIGHT,
            first_page: 1,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    /// `pdf-extract` failed to parse the document (corrupt / encrypted
    /// / unsupported structure).
    #[error("could not parse PDF text layer: {0}")]
    TextExtraction(String),
    /// The `pdfium` dynamic library could not be loaded — the
    /// operator hasn't deployed it next to the gateway (or pointed
    /// `PDFIUM_LIB_PATH` at it). The text tier still works without it.
    #[error("PDF page rendering is unavailable: {0}")]
    RendererUnavailable(String),
    /// `pdfium` loaded but failed on this specific document.
    #[error("could not render PDF pages: {0}")]
    Render(String),
}

/// Lift the text layer out of a PDF held in memory. Returns the raw
/// extracted text (possibly empty for a scanned PDF — the caller
/// decides what an empty result means). Run on a blocking thread.
pub fn extract_text(bytes: &[u8]) -> Result<String, PdfError> {
    pdf_extract::extract_text_from_mem(bytes).map_err(|e| PdfError::TextExtraction(e.to_string()))
}

/// Lift the text layer out page by page: entry `i` is page `i + 1`. Lets
/// the caller serve a page range instead of the whole document, so a long
/// PDF's tail is reachable rather than being cut off by a byte cap. Run on
/// a blocking thread.
///
/// Costs the same parse as [`extract_text`], so callers that don't need
/// per-page granularity should stay on the simpler function.
pub fn extract_text_pages(bytes: &[u8]) -> Result<Vec<String>, PdfError> {
    pdf_extract::extract_text_from_mem_by_pages(bytes)
        .map_err(|e| PdfError::TextExtraction(e.to_string()))
}

/// One rasterised PDF page, PNG-encoded.
pub struct RenderedPage {
    /// 1-based page number in the source document.
    pub page_number: usize,
    /// PNG bytes ready to base64 into a `data:` URI.
    pub png: Vec<u8>,
}

/// Result of [`render_pages`]: the pages we actually rasterised plus
/// the document's true page count, so the caller can tell the model
/// "rendered N of M pages".
pub struct RenderedPages {
    pub pages: Vec<RenderedPage>,
    pub total_pages: usize,
}

/// Rasterise up to `max_pages` pages of a PDF to PNG. Run on a
/// blocking thread (CPU-bound; `pdfium` handles are `!Send`).
///
/// Binds `pdfium` per call: an operator that hasn't shipped the
/// native library gets [`PdfError::RendererUnavailable`] and the tool
/// falls back to a note rather than killing the request.
pub fn render_pages(bytes: &[u8], max_pages: usize) -> Result<RenderedPages, PdfError> {
    render_pages_with_settings(
        bytes,
        RenderSettings {
            max_pages,
            ..RenderSettings::default()
        },
    )
}

/// Rasterise PDF pages with explicit output bounds.
///
/// The settings are deliberately bounded by the caller so a malformed or
/// unexpectedly large document cannot turn one attachment fetch into an
/// unbounded memory allocation. Run on a blocking thread; pdfium handles are
/// not `Send`.
pub fn render_pages_with_settings(
    bytes: &[u8],
    settings: RenderSettings,
) -> Result<RenderedPages, PdfError> {
    if settings.target_width <= 0
        || settings.target_width > MAX_RENDER_TARGET_WIDTH
        || settings.max_height <= 0
        || settings.max_height > MAX_RENDER_HEIGHT
        || settings.max_pages > MAX_RENDER_PAGES
    {
        return Err(PdfError::Render(format!(
            "PDF render settings must stay within {MAX_RENDER_PAGES} pages and {MAX_RENDER_TARGET_WIDTH}x{MAX_RENDER_HEIGHT} pixels"
        )));
    }

    let pdfium = bind_pdfium()?;
    let doc = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|e| PdfError::Render(e.to_string()))?;

    let config = PdfRenderConfig::new()
        .set_target_width(settings.target_width)
        .set_maximum_height(settings.max_height);

    let total_pages = doc.pages().len() as usize;
    let first_page = settings.first_page.max(1);
    let mut pages = Vec::new();
    for (idx, page) in doc.pages().iter().enumerate() {
        if pages.len() >= settings.max_pages {
            break;
        }
        // Skip ahead to the requested window. Cheap: no rendering happens
        // for skipped pages, only the iterator advances.
        if idx + 1 < first_page {
            continue;
        }
        let image = page
            .render_with_config(&config)
            .map_err(|e| PdfError::Render(e.to_string()))?
            .as_image()
            .map_err(|e| PdfError::Render(e.to_string()))?;
        let mut png = Vec::new();
        image
            .into_rgb8()
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|e| PdfError::Render(e.to_string()))?;
        pages.push(RenderedPage {
            page_number: idx + 1,
            png,
        });
    }

    Ok(RenderedPages { pages, total_pages })
}

/// Bind the `pdfium` dynamic library. Prefers an explicit
/// `PDFIUM_LIB_PATH` (operator drop-in next to the binary), then the
/// system search path. Any failure → [`PdfError::RendererUnavailable`].
fn bind_pdfium() -> Result<Pdfium, PdfError> {
    if let Ok(path) = std::env::var("PDFIUM_LIB_PATH")
        && let Ok(bindings) = Pdfium::bind_to_library(&path)
    {
        return Ok(Pdfium::new(bindings));
    }
    Pdfium::bind_to_system_library()
        .map(Pdfium::new)
        .map_err(|e| PdfError::RendererUnavailable(e.to_string()))
}

/// Hand-built, valid PDF fixtures shared by this module's tests and
/// the `fetch_attachment` tool tests (same crate, so a `#[cfg(test)]`
/// helper is visible to both). Built byte-by-byte — with a correct
/// xref table + `startxref`, which lopdf (under `pdf-extract`)
/// requires — so the tests need no fixture file or installed renderer.
#[cfg(test)]
pub(crate) mod test_support {
    /// Assemble a PDF from object bodies (1-indexed), computing the
    /// xref offsets from the real byte layout. Object 1 must be the
    /// `/Catalog`.
    fn assemble(objs: &[String]) -> Vec<u8> {
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        let mut offsets = Vec::with_capacity(objs.len());
        for (i, body) in objs.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            pdf.extend_from_slice(body.as_bytes());
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref_start = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Root 1 0 R /Size {} >>\nstartxref\n{xref_start}\n%%EOF",
                objs.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    /// A born-digital PDF whose single page carries the literal text
    /// "Hello PDF" in its content stream.
    pub(crate) fn hello_pdf() -> Vec<u8> {
        let stream = "BT /F1 24 Tf 72 700 Td (Hello PDF) Tj ET\n";
        assemble(&[
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_string(),
            format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ])
    }

    /// A born-digital PDF with `n` pages, page `i` carrying the literal
    /// text `Page <i> body`. Used to exercise page-range reads, where a
    /// single-page fixture can't tell "returned the right window" from
    /// "returned everything".
    pub(crate) fn multipage_pdf(n: usize) -> Vec<u8> {
        assert!(n >= 1, "need at least one page");
        // Object layout: 1 = catalog, 2 = pages tree, 3 = font, then one
        // /Page + one content stream per page.
        let first_page_obj = 4;
        let kids: Vec<String> = (0..n)
            .map(|i| format!("{} 0 R", first_page_obj + i * 2))
            .collect();
        let mut objs = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        for i in 0..n {
            let content_obj = first_page_obj + i * 2 + 1;
            objs.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Contents {content_obj} 0 R \
                 /Resources << /Font << /F1 3 0 R >> >> >>"
            ));
            let stream = format!("BT /F1 24 Tf 72 700 Td (Page {} body) Tj ET\n", i + 1);
            objs.push(format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ));
        }
        assemble(&objs)
    }

    /// A valid one-page PDF with an empty content stream — i.e. no
    /// extractable text, the way a scanned/image-only PDF looks to the
    /// text tier.
    pub(crate) fn blank_pdf() -> Vec<u8> {
        assemble(&[
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>".to_string(),
            "<< /Length 0 >>\nstream\n\nendstream".to_string(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::hello_pdf;

    #[test]
    fn extract_text_reads_text_layer() {
        let pdf = hello_pdf();
        let text = extract_text(&pdf).expect("minimal PDF should parse");
        assert!(
            text.contains("Hello PDF"),
            "expected extracted text to contain the content stream literal, got: {text:?}"
        );
    }

    #[test]
    fn extract_text_errors_on_garbage() {
        let err = extract_text(b"this is not a pdf at all").unwrap_err();
        assert!(matches!(err, PdfError::TextExtraction(_)), "{err:?}");
    }

    #[test]
    fn render_settings_have_safe_defaults() {
        let settings = RenderSettings::default();
        assert_eq!(settings.max_pages, DEFAULT_MAX_RENDER_PAGES);
        assert_eq!(settings.target_width, 1240);
        assert_eq!(settings.max_height, 1754);
        assert_eq!(settings.first_page, 1);
    }

    #[test]
    fn extract_text_pages_splits_per_page() {
        let pages = extract_text_pages(&test_support::multipage_pdf(5))
            .expect("multipage fixture should parse");
        assert_eq!(pages.len(), 5);
        for (i, page) in pages.iter().enumerate() {
            assert!(
                page.contains(&format!("Page {} body", i + 1)),
                "page {} was {page:?}",
                i + 1
            );
        }
    }

    #[test]
    fn extract_text_pages_errors_on_garbage() {
        let err = extract_text_pages(b"not a pdf").unwrap_err();
        assert!(matches!(err, PdfError::TextExtraction(_)), "{err:?}");
    }

    #[test]
    fn render_honours_first_page_window() {
        // Without the renderer we can't assert on pixels, but the window
        // arithmetic is what regressed before — assert it whenever pdfium
        // happens to be available, and skip cleanly when it isn't.
        let pdf = test_support::multipage_pdf(6);
        match render_pages_with_settings(
            &pdf,
            RenderSettings {
                first_page: 3,
                max_pages: 2,
                ..RenderSettings::default()
            },
        ) {
            Ok(rendered) => {
                assert_eq!(rendered.total_pages, 6);
                let numbers: Vec<usize> = rendered.pages.iter().map(|p| p.page_number).collect();
                assert_eq!(numbers, vec![3, 4]);
            }
            Err(PdfError::RendererUnavailable(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn first_page_beyond_the_document_renders_nothing() {
        let pdf = test_support::multipage_pdf(2);
        match render_pages_with_settings(
            &pdf,
            RenderSettings {
                first_page: 99,
                ..RenderSettings::default()
            },
        ) {
            Ok(rendered) => {
                assert_eq!(rendered.total_pages, 2);
                assert!(rendered.pages.is_empty());
            }
            Err(PdfError::RendererUnavailable(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn first_page_zero_behaves_like_one() {
        let pdf = test_support::multipage_pdf(3);
        match render_pages_with_settings(
            &pdf,
            RenderSettings {
                first_page: 0,
                max_pages: 1,
                ..RenderSettings::default()
            },
        ) {
            Ok(rendered) => {
                assert_eq!(
                    rendered.pages.first().map(|p| p.page_number),
                    Some(1),
                    "0 must be normalised to the first page"
                );
            }
            Err(PdfError::RendererUnavailable(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn render_settings_reject_unbounded_dimensions_before_loading_pdfium() {
        let err = match render_pages_with_settings(
            b"not a pdf",
            RenderSettings {
                target_width: MAX_RENDER_TARGET_WIDTH + 1,
                ..RenderSettings::default()
            },
        ) {
            Ok(_) => panic!("unbounded dimensions must be rejected"),
            Err(err) => err,
        };
        assert!(matches!(err, PdfError::Render(message) if message.contains("within")));
    }

    #[test]
    fn render_pages_degrades_when_pdfium_absent() {
        // CI doesn't ship the native pdfium library, so unless an
        // operator set PDFIUM_LIB_PATH this must surface a clean
        // RendererUnavailable rather than panicking. When the lib *is*
        // present (a dev box with it installed) rendering succeeds and
        // returns at least one page — both outcomes are acceptable.
        match render_pages(&hello_pdf(), 4) {
            Ok(rendered) => {
                assert_eq!(rendered.total_pages, 1);
                assert_eq!(rendered.pages.len(), 1);
                assert!(!rendered.pages[0].png.is_empty());
            }
            Err(PdfError::RendererUnavailable(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}
