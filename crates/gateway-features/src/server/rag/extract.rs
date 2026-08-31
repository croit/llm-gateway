// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Turning a document's bytes into text the indexer can chunk.
//!
//! Before this existed the indexer did `String::from_utf8` and `continue`d on
//! failure, which silently dropped every PDF, scan and office file in a
//! corpus. For a code corpus that was right. For a document corpus it was the
//! whole corpus.
//!
//! The ladder, cheapest rung first:
//!
//! | Input | How |
//! | --- | --- |
//! | text-ish | UTF-8 decode, in-process |
//! | PDF with a text layer | [`pdf::extract_text_pages`], in-process |
//! | PDF that is a scan | the OCR backend, one call per document |
//! | image | the OCR backend |
//! | office | a sandbox extractor, injected (see [`OfficeExtractor`]) |
//!
//! Two properties are worth stating because they are what make a
//! thousands-of-documents corpus affordable:
//!
//!   * **A born-digital PDF never reaches the GPU.** The text layer is read
//!     first and [`ocr::pdf_needs_ocr`] decides, by counting characters per
//!     page — no word lists, so it behaves identically for German and
//!     English.
//!   * **OCR is cached by content hash** inside [`OcrService`], so a full
//!     re-index of an unchanged corpus re-embeds but does not re-recognise.
//!
//! Everything degrades rather than fails: no OCR backend configured means
//! scans are reported [`Unsupported`], not that indexing breaks.
//!
//! [`Unsupported`]: Extracted::Unsupported

use std::sync::Arc;

use crate::server::ocr::{self, OcrError, OcrService, UsageMeta};
use crate::server::pdf;

/// Which rung of the ladder produced a document's text. Recorded per
/// document so an operator can tell a clean text-layer read from an OCR
/// guess when an answer looks wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extractor {
    Text,
    PdfTextLayer,
    Ocr,
    Office,
}

impl Extractor {
    pub fn as_str(self) -> &'static str {
        match self {
            Extractor::Text => "text",
            Extractor::PdfTextLayer => "pdf_text_layer",
            Extractor::Ocr => "ocr",
            Extractor::Office => "office",
        }
    }

    /// Whether text from this extractor is a *recognition* rather than a
    /// read, so a figure taken from it might be a misread character.
    ///
    /// Asked of the enum rather than by comparing the stored string, so a
    /// second recognising extractor is one match arm rather than a hunt for
    /// every `== "ocr"` in the workspace.
    pub fn is_recognised(name: &str) -> bool {
        name == Extractor::Ocr.as_str()
    }
}

/// A document's text, one entry per page.
///
/// Non-paginated input is a single "page", so the chunker has one shape to
/// deal with. `pages_total` vs `pages_processed` is how a caller learns the
/// document was only partly read — a model that got 8 of 40 pages must not
/// answer as though it read the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedDoc {
    pub pages: Vec<String>,
    pub extractor: Extractor,
    pub pages_total: Option<usize>,
    pub pages_processed: Option<usize>,
    pub truncated: bool,
}

impl ExtractedDoc {
    fn text(content: String) -> Self {
        Self {
            pages: vec![content],
            extractor: Extractor::Text,
            pages_total: None,
            pages_processed: None,
            truncated: false,
        }
    }

    /// True when the whole document was read, as far as we can tell.
    pub fn complete(&self) -> bool {
        !self.truncated
            && match (self.pages_total, self.pages_processed) {
                (Some(total), Some(done)) => done >= total,
                _ => true,
            }
    }

    /// One-line note for the index log when coverage is partial. Empty when
    /// there is nothing to qualify.
    pub fn coverage_note(&self) -> String {
        let mut parts = Vec::new();
        if let (Some(total), Some(done)) = (self.pages_total, self.pages_processed)
            && done < total
        {
            parts.push(format!("{done} of {total} pages read"));
        }
        if self.truncated {
            parts.push("text truncated at the extraction limit".to_string());
        }
        parts.join("; ")
    }

    pub fn is_empty(&self) -> bool {
        self.pages.iter().all(|p| p.trim().is_empty())
    }
}

/// What came back from one extraction attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extracted {
    Ok(ExtractedDoc),
    /// Recognised, but nothing configured here can read it. Distinct from
    /// [`Extracted::Failed`] because they mean different things to an
    /// operator: this one says "turn on the OCR backend", the other says
    /// "something is broken".
    Unsupported {
        reason: String,
    },
    Failed(String),
}

/// The coarse type of an input, decided from its MIME type and extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Text,
    Pdf,
    Image,
    Office,
    Unknown,
}

/// Office formats the sandbox extractor understands, by extension.
const OFFICE_EXTS: &[&str] = &["docx", "pptx", "xlsx"];

/// Extensions that are text regardless of what a server claims the MIME type
/// is — file hosts are routinely wrong about `application/octet-stream`.
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "csv", "tsv", "json", "yaml", "yml", "toml", "xml", "html", "htm",
    "rst", "adoc", "log", "ini", "cfg", "conf", "sql", "rs", "py", "js", "ts", "go", "java", "c",
    "h", "cpp", "hpp", "sh", "rb", "php", "pl", "pm", "lua", "css", "tex",
];

fn extension(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Classify by extension first, MIME second.
///
/// That order is deliberate: the extension is chosen by whoever saved the
/// file, while the MIME type is guessed by the server, and file hosts hand
/// back `application/octet-stream` for perfectly ordinary documents often
/// enough that trusting it first loses real content.
pub fn classify(path: &str, mime: Option<&str>) -> DocKind {
    let ext = extension(path);
    if ext == "pdf" {
        return DocKind::Pdf;
    }
    if OFFICE_EXTS.contains(&ext.as_str()) {
        return DocKind::Office;
    }
    if TEXT_EXTS.contains(&ext.as_str()) {
        return DocKind::Text;
    }
    let mime = mime.unwrap_or("").split(';').next().unwrap_or("").trim();
    if mime == "application/pdf" {
        return DocKind::Pdf;
    }
    if mime.starts_with("image/") {
        return DocKind::Image;
    }
    if mime.starts_with("text/") || mime == "application/json" || mime == "application/xml" {
        return DocKind::Text;
    }
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "tif" | "tiff" | "bmp" => DocKind::Image,
        _ => DocKind::Unknown,
    }
}

/// Reads office documents. Implemented above this crate, because the sandbox
/// client lives in `gateway-runtime` and `gateway-features` must never
/// reference upward (see `docs/architecture.md`). The indexer holds an
/// `Option<Arc<dyn OfficeExtractor>>`; `None` degrades office files to
/// [`Extracted::Unsupported`], exactly as a missing OCR pool degrades scans.
///
/// Hand-rolled boxed future rather than `async_trait` to match the `Tool`
/// trait's existing shape in `gateway-runtime`.
pub trait OfficeExtractor: Send + Sync {
    fn extract_office<'a>(
        &'a self,
        ext: &'a str,
        bytes: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>;
}

/// The ladder itself. Cheap to clone.
#[derive(Clone)]
pub struct DocumentExtractor {
    ocr: Option<OcrService>,
    office: Option<Arc<dyn OfficeExtractor>>,
    /// Attributed to on OCR usage rows. Indexing has no end user, so the
    /// cost lands on a synthetic id rather than on whoever happened to
    /// trigger the re-index.
    usage_user: String,
}

impl DocumentExtractor {
    pub fn new(ocr: Option<OcrService>, office: Option<Arc<dyn OfficeExtractor>>) -> Self {
        Self {
            ocr,
            office,
            usage_user: "rag-indexer".to_string(),
        }
    }

    /// Whether a scan or image could be read at all right now.
    pub fn ocr_available(&self) -> bool {
        self.ocr.as_ref().is_some_and(|o| o.available())
    }

    /// Which rungs of the ladder are usable right now.
    ///
    /// Stored with each build so that *gaining* a capability invalidates what
    /// was indexed without it. A file the ladder could not read is recorded
    /// as skipped, and a skip — correctly — does not stop the pass being
    /// authoritative. But it becomes readable the moment the missing backend
    /// is wired up, and without this the next sync would see those
    /// directories unchanged and prune straight past them.
    pub fn fingerprint(&self) -> String {
        format!(
            "ocr={},office={}",
            self.ocr_available(),
            self.office.is_some()
        )
    }

    /// Read one document.
    pub async fn extract(&self, path: &str, mime: Option<&str>, bytes: Vec<u8>) -> Extracted {
        match classify(path, mime) {
            DocKind::Text => Self::decode_text(bytes),
            DocKind::Pdf => self.extract_pdf(path, bytes).await,
            DocKind::Image => {
                self.recognize(path, mime.unwrap_or("image/png"), bytes)
                    .await
            }
            DocKind::Office => self.extract_office(path, bytes).await,
            // Unknown: it may still be text with an extension we don't list
            // (a `.env`, a `Dockerfile`, a bare `LICENSE`). Trying the cheap
            // decode is free and keeps the previous behaviour for those.
            DocKind::Unknown => match Self::decode_text(bytes) {
                Extracted::Ok(doc) => Extracted::Ok(doc),
                _ => Extracted::Unsupported {
                    reason: "not text, and not a document type this gateway can read".into(),
                },
            },
        }
    }

    fn decode_text(bytes: Vec<u8>) -> Extracted {
        match String::from_utf8(bytes) {
            Ok(s) => Extracted::Ok(ExtractedDoc::text(s)),
            Err(_) => Extracted::Unsupported {
                reason: "not valid UTF-8 text".into(),
            },
        }
    }

    /// Text layer first; OCR only for the pages a text layer cannot cover.
    async fn extract_pdf(&self, path: &str, bytes: Vec<u8>) -> Extracted {
        let parsed = {
            let bytes = bytes.clone();
            tokio::task::spawn_blocking(move || pdf::extract_text_pages(&bytes)).await
        };
        let pages = match parsed {
            Ok(Ok(pages)) => pages,
            // A PDF we cannot parse at all may still be readable as an image
            // by the OCR backend, so fall through rather than give up.
            Ok(Err(err)) => {
                tracing::debug!(path, error = %err, "rag: pdf text layer unreadable");
                Vec::new()
            }
            Err(err) => {
                tracing::debug!(path, error = %err, "rag: pdf text layer task failed");
                Vec::new()
            }
        };
        let has_text = pages.iter().any(|p| !p.trim().is_empty());
        // With no OCR backend there is no second rung, so the question is
        // simply "is there any text here" — a deployment without a GPU still
        // indexes every born-digital PDF. With OCR available the bar is the
        // configured one, so a mostly-empty text layer is escalated instead
        // of indexed as a handful of stray characters.
        let min_chars = self
            .ocr
            .as_ref()
            .filter(|o| o.available())
            .map(|o| o.auto_min_text_chars_per_page());
        let use_text_layer =
            has_text && min_chars.is_none_or(|min| !ocr::pdf_needs_ocr(&pages, min));
        if use_text_layer {
            let count = pages.len();
            return Extracted::Ok(ExtractedDoc {
                pages,
                extractor: Extractor::PdfTextLayer,
                pages_total: Some(count),
                pages_processed: Some(count),
                truncated: false,
            });
        }
        if !self.ocr_available() {
            return Extracted::Unsupported {
                reason: "this PDF has no readable text layer and no OCR backend is configured"
                    .into(),
            };
        }
        self.recognize(path, "application/pdf", bytes).await
    }

    async fn recognize(&self, path: &str, mime: &str, bytes: Vec<u8>) -> Extracted {
        let Some(ocr) = self.ocr.as_ref() else {
            return Extracted::Unsupported {
                reason: "no OCR backend is configured".into(),
            };
        };
        let filename = path.rsplit('/').next().unwrap_or(path).to_string();
        let meta = UsageMeta {
            user_id: self.usage_user.clone(),
            source: gateway_core::server::db::usage::UsageSource::Indexer,
        };
        match ocr.recognize(&filename, mime, bytes, &meta).await {
            Ok(outcome) => Extracted::Ok(ExtractedDoc {
                pages: ocr::split_pages(&outcome.markdown),
                extractor: Extractor::Ocr,
                pages_total: outcome.pages_total,
                pages_processed: outcome.pages_processed,
                truncated: outcome.truncated,
            }),
            // "No backend" is a configuration state, not a failure: the
            // operator sees "turn OCR on", not "indexing is broken".
            Err(OcrError::NoBackend) => Extracted::Unsupported {
                reason: "no OCR backend is configured".into(),
            },
            Err(OcrError::TooLarge { bytes, limit }) => Extracted::Unsupported {
                reason: format!("{bytes} bytes is over the {limit}-byte OCR limit"),
            },
            Err(err) => Extracted::Failed(err.to_string()),
        }
    }

    async fn extract_office(&self, path: &str, bytes: Vec<u8>) -> Extracted {
        let Some(office) = self.office.as_ref() else {
            return Extracted::Unsupported {
                reason: "office documents need the sandbox, which is not configured".into(),
            };
        };
        let ext = extension(path);
        match office.extract_office(&ext, bytes).await {
            Ok(text) => Extracted::Ok(ExtractedDoc {
                pages: vec![text],
                extractor: Extractor::Office,
                pages_total: None,
                pages_processed: None,
                truncated: false,
            }),
            Err(err) => Extracted::Failed(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_beats_a_wrong_mime_type() {
        // File hosts hand back octet-stream for ordinary documents; trusting
        // the MIME first would drop them.
        assert_eq!(
            classify("a/b/invoice.pdf", Some("application/octet-stream")),
            DocKind::Pdf
        );
        assert_eq!(
            classify("notes.md", Some("application/octet-stream")),
            DocKind::Text
        );
    }

    #[test]
    fn mime_is_used_when_the_name_carries_no_extension() {
        assert_eq!(classify("scan", Some("image/jpeg")), DocKind::Image);
        assert_eq!(classify("README", Some("text/plain")), DocKind::Text);
        assert_eq!(classify("blob", Some("application/pdf")), DocKind::Pdf);
    }

    #[test]
    fn office_and_image_extensions_are_recognised() {
        assert_eq!(classify("deck.pptx", None), DocKind::Office);
        assert_eq!(classify("sheet.xlsx", None), DocKind::Office);
        assert_eq!(classify("photo.JPG", None), DocKind::Image);
    }

    #[test]
    fn an_unknown_shape_is_unknown_not_text() {
        assert_eq!(classify("archive.zip", None), DocKind::Unknown);
    }

    #[tokio::test]
    async fn text_files_decode_in_process() {
        let x = DocumentExtractor::new(None, None);
        let out = x.extract("a.md", None, b"# hello".to_vec()).await;
        match out {
            Extracted::Ok(doc) => {
                assert_eq!(doc.extractor, Extractor::Text);
                assert_eq!(doc.pages, vec!["# hello".to_string()]);
                assert!(doc.complete());
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_extensionless_text_file_still_reads() {
        let x = DocumentExtractor::new(None, None);
        // No extension, no MIME — the cheap decode is still tried, which is
        // what keeps LICENSE / Dockerfile / .env indexable.
        let out = x
            .extract("Dockerfile", None, b"FROM scratch".to_vec())
            .await;
        assert!(matches!(out, Extracted::Ok(_)), "{out:?}");
    }

    #[tokio::test]
    async fn binary_with_no_backend_is_unsupported_not_failed() {
        let x = DocumentExtractor::new(None, None);
        let out = x.extract("thing.bin", None, vec![0xff, 0xfe, 0x00]).await;
        match out {
            Extracted::Unsupported { .. } => {}
            other => panic!("a missing reader is a configuration state: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_born_digital_pdf_reads_without_any_ocr_backend() {
        // The no-GPU deployment: a PDF with a text layer must still index.
        // Getting this wrong (by treating "no OCR" as an infinitely high
        // scan-detection bar) silently drops every readable PDF.
        let x = DocumentExtractor::new(None, None);
        let pdf = crate::server::pdf::test_support::multipage_pdf(3);
        match x.extract("report.pdf", Some("application/pdf"), pdf).await {
            Extracted::Ok(doc) => {
                assert_eq!(doc.extractor, Extractor::PdfTextLayer);
                assert_eq!(doc.pages.len(), 3, "every page is kept");
                assert!(!doc.is_empty());
            }
            other => panic!("expected the text layer to be read, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_scan_without_an_ocr_backend_says_so() {
        let x = DocumentExtractor::new(None, None);
        // Not a real PDF, so the text layer read fails and it is treated as
        // a scan — the path that needs OCR.
        let out = x
            .extract("scan.pdf", None, b"%PDF-1.4 garbage".to_vec())
            .await;
        match out {
            Extracted::Unsupported { reason } => {
                assert!(
                    reason.to_lowercase().contains("ocr"),
                    "the operator is told what to turn on: {reason}"
                );
            }
            other => panic!("expected unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_office_file_without_the_sandbox_says_so() {
        let x = DocumentExtractor::new(None, None);
        let out = x
            .extract("deck.pptx", None, vec![0x50, 0x4b, 0x03, 0x04])
            .await;
        match out {
            Extracted::Unsupported { reason } => {
                assert!(reason.to_lowercase().contains("sandbox"), "{reason}");
            }
            other => panic!("expected unsupported, got {other:?}"),
        }
    }

    struct FakeOffice;

    impl OfficeExtractor for FakeOffice {
        fn extract_office<'a>(
            &'a self,
            ext: &'a str,
            _bytes: Vec<u8>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
        {
            Box::pin(async move { Ok(format!("extracted {ext}")) })
        }
    }

    #[tokio::test]
    async fn an_injected_office_extractor_is_used() {
        let x = DocumentExtractor::new(None, Some(Arc::new(FakeOffice)));
        let out = x.extract("deck.pptx", None, vec![1, 2, 3]).await;
        match out {
            Extracted::Ok(doc) => {
                assert_eq!(doc.extractor, Extractor::Office);
                assert_eq!(doc.pages, vec!["extracted pptx".to_string()]);
            }
            other => panic!("expected office text, got {other:?}"),
        }
    }

    #[test]
    fn partial_coverage_is_reported_rather_than_hidden() {
        let doc = ExtractedDoc {
            pages: vec!["a".into()],
            extractor: Extractor::Ocr,
            pages_total: Some(40),
            pages_processed: Some(8),
            truncated: false,
        };
        assert!(!doc.complete());
        assert!(doc.coverage_note().contains("8 of 40"));
    }

    #[test]
    fn a_fully_read_document_has_nothing_to_qualify() {
        let doc = ExtractedDoc {
            pages: vec!["a".into()],
            extractor: Extractor::PdfTextLayer,
            pages_total: Some(3),
            pages_processed: Some(3),
            truncated: false,
        };
        assert!(doc.complete());
        assert!(doc.coverage_note().is_empty());
    }
}
