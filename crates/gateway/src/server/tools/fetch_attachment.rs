// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Resolves opaque attachment ids (handed to the model in past-turn
//! replay stubs) back to the live S3 object and returns the bytes.
//!
//! Why: chat attachments only travel in their original user turn —
//! the gateway strips later replays down to `[attached file="..."
//! mime="..." size=N id="<turn_id>/<filename>"]` stubs so the
//! conversation context doesn't bloat. If the model decides it
//! actually needs to look at that file again, it calls this tool
//! with the stub's `id` and the gateway pulls the object server-side
//! (no presigned URL ever crosses the wire to the LLM provider, and
//! TTL expiry is irrelevant because the gateway re-fetches each
//! time).
//!
//! Return shapes depending on the attachment's mime:
//!
//! - **Text-ish** (CSV, JSON, markdown, code, …): decoded UTF-8 in
//!   `content`. The model reads it like any other tool output.
//! - **Image**: the gateway presigns a fresh GET URL and returns a
//!   `tool_content_parts(...)` envelope carrying a text summary plus
//!   an `image_url` part — the driver splices that into the upstream
//!   `role:"tool"` message as array content, which lets a vision
//!   model actually re-see the image. No bytes cross the wire to
//!   the LLM provider in inline form; just the (time-limited)
//!   presigned URL the upstream fetches itself.
//! - **PDF**: two model-driven tiers (see [`crate::server::pdf`]).
//!   `mode="text"` (default) extracts the text layer and returns it
//!   like any text file. `mode="images"` rasterises the pages and
//!   returns them as `image_url` parts — the escalation path the
//!   model takes when the text layer is empty (a scanned PDF).
//! - **Other binary** (zip, audio, …): metadata only, with a note
//!   telling the model the bytes can't be reattached via a tool
//!   result. The model should ask the user to re-upload.

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use std::sync::Arc;

use shared::sandbox::{InputFile, Language, RunRequest};

use super::sandbox::{SandboxClient, b64};
use super::{Tool, ToolContext, ToolError, ToolFuture, tool_content_parts};
use crate::server::chat_attachments::{self, BinaryDisposition, PayloadLimits};
use crate::server::pdf::{self, PdfError};

/// Hard cap on text returned to the model — shared with `fetch_url`
/// so both tools have the same contract. 4 MB is generous enough
/// that essentially no real attachment is truncated in practice
/// (modern context windows handle ~1M tokens of text), while still
/// bounding the gateway's per-call memory footprint. The caller
/// can request less via `max_bytes`.
const HARD_MAX_BYTES: usize = 4 * 1024 * 1024;
const HARD_MAX_BYTES_DEFAULT: usize = HARD_MAX_BYTES;
/// Image ceiling — generous enough for phone photos (typically
/// 5–15 MB) and screenshots. Above this we surface a
/// `kind: "image-too-large"` payload so the model knows why it
/// didn't get the bytes inline.
const MAX_IMAGE_BYTES: usize = 25 * 1024 * 1024;

pub struct FetchAttachment {
    /// Sandbox client for reading Office formats (docx/pptx/xlsx), which
    /// aren't text and aren't PDFs — a python extractor in the sandbox
    /// turns them into verbatim structured content. `None` when no
    /// `[sandbox]` is configured; those uploads then fall back to the
    /// generic "binary — re-upload" stub.
    sandbox: Option<Arc<SandboxClient>>,
}

impl FetchAttachment {
    pub fn new(sandbox: Option<Arc<SandboxClient>>) -> Self {
        Self { sandbox }
    }
}

/// Office extension (`docx`/`pptx`/`xlsx`) for an attachment, from its mime
/// or filename. PDFs are deliberately excluded — they keep the richer
/// two-tier text/vision path. `None` for everything else.
fn office_ext(mime: &str, filename: &str) -> Option<&'static str> {
    let lname = filename.to_ascii_lowercase();
    let has = |ext: &str, m: &str| lname.ends_with(ext) || mime.contains(m);
    if has(".pptx", "presentationml") {
        Some("pptx")
    } else if has(".docx", "wordprocessingml") {
        Some("docx")
    } else if has(".xlsx", "spreadsheetml") {
        Some("xlsx")
    } else {
        None
    }
}

/// MIME for an embedded image the extractor pulled out of a document, keyed
/// off the filename the extractor chose (`.png`/`.jpg`/…). Vector blobs
/// python-pptx emits (`.emf`/`.wmf`) return `None` — typst can't place them,
/// so we skip carrying them rather than ship an unusable attachment.
fn image_mime(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".png") {
        Some("image/png")
    } else if n.ends_with(".jpg") || n.ends_with(".jpeg") {
        Some("image/jpeg")
    } else if n.ends_with(".gif") {
        Some("image/gif")
    } else if n.ends_with(".svg") {
        Some("image/svg+xml")
    } else if n.ends_with(".webp") {
        Some("image/webp")
    } else if n.ends_with(".bmp") {
        Some("image/bmp")
    } else if n.ends_with(".tiff") || n.ends_with(".tif") {
        Some("image/tiff")
    } else {
        None
    }
}

/// Verbatim structured extractor run in the sandbox. Dispatches by the
/// input file's extension (python-pptx / python-docx / openpyxl). Prints one
/// JSON object of the document's content — titles, text, bullets, tables,
/// notes, image filenames — with NO modification, so the model can re-author
/// it into a template (letter / presentation / one-pager) losslessly. Each
/// embedded image is written to `/work` top-level (NOT a subdir) so the
/// sandbox agent returns it as an artifact for the gateway to re-attach.
const EXTRACT_PY: &str = r#"import sys, json, os
src, imgdir = sys.argv[1], sys.argv[2]
os.makedirs(imgdir, exist_ok=True)
ext = os.path.splitext(src)[1].lower()
def extract_pptx():
    from pptx import Presentation
    from pptx.enum.shapes import MSO_SHAPE_TYPE
    prs = Presentation(src); out=[]
    def walk(shapes, tshape, acc):
        for sh in shapes:
            if sh.shape_type==MSO_SHAPE_TYPE.GROUP: walk(sh.shapes,tshape,acc); continue
            if sh.shape_type==MSO_SHAPE_TYPE.PICTURE: acc["_p"].append(sh); continue
            if sh.has_table: acc["tables"].append([[c.text for c in r.cells] for r in sh.table.rows]); continue
            if sh.has_text_frame:
                ps=[p.text for p in sh.text_frame.paragraphs if p.text.strip()]
                if not ps: continue
                if sh is tshape: acc["title"]=sh.text_frame.text.strip()
                elif len(ps)>1: acc["bullets"].append(ps)
                else: acc["text"].append(ps[0])
    for i,sl in enumerate(prs.slides):
        acc={"title":"","text":[],"bullets":[],"tables":[],"_p":[]}
        walk(sl.shapes, sl.shapes.title, acc)
        imgs=[]
        for j,sh in enumerate(acc.pop("_p")):
            im=sh.image; fn="slide%d_img%d.%s"%(i+1,j+1,im.ext); open(os.path.join(imgdir,fn),"wb").write(im.blob); imgs.append(fn)
        notes=sl.notes_slide.notes_text_frame.text.strip() if sl.has_notes_slide else ""
        s={"index":i+1}
        for k in ("title","text","bullets","tables"):
            if acc[k]: s[k]=acc[k]
        if imgs: s["images"]=imgs
        if notes: s["notes"]=notes
        out.append(s)
    return {"kind":"presentation","units":"slides","content":out}
def extract_docx():
    import docx
    d=docx.Document(src); blocks=[]
    for p in d.paragraphs:
        t=p.text.strip()
        if t: blocks.append({"style":p.style.name if p.style else "", "text":t})
    tables=[[[c.text for c in r.cells] for r in t.rows] for t in d.tables]
    imgs=[]
    for i,rel in enumerate(d.part.rels.values()):
        if "image" in rel.reltype:
            blob=rel.target_part.blob; fn="img%d.%s"%(i+1,(rel.target_part.content_type.split("/")[-1] or "png"))
            open(os.path.join(imgdir,fn),"wb").write(blob); imgs.append(fn)
    r={"kind":"document","paragraphs":blocks}
    if tables: r["tables"]=tables
    if imgs: r["images"]=imgs
    return r
def extract_xlsx():
    from openpyxl import load_workbook
    wb=load_workbook(src, data_only=True); sheets=[]
    for ws in wb.worksheets:
        rows=[[("" if c is None else str(c)) for c in row] for row in ws.iter_rows(values_only=True)]
        rows=[r for r in rows if any(x.strip() for x in r)]
        sheets.append({"name":ws.title,"rows":rows})
    return {"kind":"spreadsheet","sheets":sheets}
fn={".pptx":extract_pptx,".docx":extract_docx,".xlsx":extract_xlsx}.get(ext)
if not fn: print(json.dumps({"error":"unsupported: "+ext})); sys.exit(1)
res=fn(); res["source"]=os.path.basename(src)
print(json.dumps(res, ensure_ascii=False))
"#;

/// Run [`EXTRACT_PY`] over `bytes` in the sandbox and return the parsed
/// structured content. The document rides in as `upload.<ext>` so the
/// extractor dispatches on the real format.
async fn extract_office(
    sandbox: &SandboxClient,
    ctx: &ToolContext,
    s3: &crate::server::config::S3Config,
    turn_id: &str,
    ext: &str,
    bytes: Vec<u8>,
) -> Result<Value, ToolError> {
    let infile = format!("upload.{ext}");
    // `EXTRACT_PY` is concatenated (not `format!`-interpolated) so its Python
    // dict/set braces don't collide with format placeholders. Images go to
    // `.` (== `/work`, the cwd) so the agent collects them as artifacts.
    let code =
        format!("set -e\ncd /work\npython3 - {infile} . <<'PYEOF'\n") + EXTRACT_PY + "PYEOF\n";
    let req = RunRequest {
        language: Language::Bash,
        code,
        files: vec![InputFile {
            name: infile,
            content_b64: b64::encode(&bytes),
        }],
        timeout_secs: None,
        network: false,
    };
    let resp = sandbox.run_job(req).await?;
    if resp.exit_code != 0 || resp.timed_out {
        return Err(ToolError::Failed(format!(
            "document extraction failed (exit {}): {}",
            resp.exit_code,
            resp.stderr.chars().rev().take(400).collect::<String>()
        )));
    }
    let mut document: Value = serde_json::from_str(resp.stdout.trim())
        .map_err(|e| ToolError::Failed(format!("extractor did not return JSON: {e}")))?;

    // Re-attach each embedded image the extractor pulled out. They ride back
    // as sandbox artifacts (top-level `/work` files); store each as a
    // markerless attachment of this turn (invisible — source material, not a
    // deliverable, like the typst `.json` edit-base) and hand the model a
    // ready-to-use `att:<id>` ref. Dropping such a ref into any image field of
    // a render makes typst stage the real pixels (see `typst_render`). The
    // `file` key matches the filename listed on each slide/paragraph, so the
    // model can map "slide 3 had slide3_img1.png" → the ref to use.
    let mut image_refs = Vec::new();
    for art in &resp.artifacts {
        let Some(mime) = image_mime(&art.name) else {
            continue;
        };
        let Some(img) = b64::decode(&art.content_b64) else {
            continue;
        };
        let stored = match ctx.attachment_reservations.as_ref() {
            Some(res) => chat_attachments::reserve_filename(&ctx.db, turn_id, res, &art.name)
                .await
                .map_err(|e| ToolError::Failed(format!("reserve image name: {e}")))?,
            None => art.name.clone(),
        };
        chat_attachments::upload(s3, turn_id, &stored, mime, img)
            .await
            .map_err(|e| ToolError::Failed(format!("upload extracted image: {e}")))?;
        image_refs.push(json!({
            "file": art.name,
            "ref": format!("att:{turn_id}/{stored}"),
        }));
    }
    if !image_refs.is_empty()
        && let Value::Object(map) = &mut document
    {
        map.insert("image_refs".into(), json!(image_refs));
    }
    Ok(document)
}

#[derive(Deserialize)]
struct FetchArgs {
    id: String,
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    mode: FetchMode,
}

/// How to read a PDF attachment. Ignored for non-PDF files (their
/// shape is decided by mime). The model starts with the cheap
/// [`FetchMode::Text`] tier and escalates to [`FetchMode::Images`]
/// only when the text layer turns out to be empty or unusable
/// (scanned / image-only PDFs).
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
#[serde(rename_all = "lowercase")]
enum FetchMode {
    /// Pull the PDF's text layer out as UTF-8. Cheap; the default.
    #[default]
    Text,
    /// Rasterise the PDF's pages to images for a vision model.
    Images,
}

impl Tool for FetchAttachment {
    fn id(&self) -> &str {
        "fetch_attachment"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Fetch the contents of a chat attachment by its opaque id. \
             User messages with attachments show them as `[attached file=… \
             mime=… size=… id=\"<turn_id>/<filename>\"]` stubs — call this \
             tool with the stub's id when you actually need the bytes. \
             Text-ish files (code, JSON, CSV, markdown, plain text, …) are \
             returned as UTF-8 in `content`. Images are re-attached as a \
             visible `image_url` part you can look at. PDFs are read in two \
             tiers: the default `mode=\"text\"` extracts the text layer \
             (cheap — use this first); if the result comes back empty or \
             garbled (a scanned / image-only PDF), call again with \
             `mode=\"images\"` to get the pages rendered as images you can \
             actually see. Office files (`.docx`/`.pptx`/`.xlsx`) return \
             `kind:\"document_structure\"` — the verbatim content (titles, \
             text, bullets, tables, notes) plus `image_refs` for any embedded \
             images; use this to re-author an upload into a branded template \
             (typst_presentation / typst_letter / typst_onepager), carrying \
             images over by their `att:` ref. Other binary files (zip, audio, \
             …) return metadata only; ask the user to re-upload if you need \
             them. Skip calling this if the user's question doesn't depend on \
             the attachment's contents.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Opaque attachment id of the form \
                                        `<turn_id>/<filename>` exactly as it \
                                        appeared in the replay stub."
                    },
                    "max_bytes": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": HARD_MAX_BYTES,
                        "description": "Optional cap on bytes returned for \
                                        text content. Defaults to the full \
                                        attachment up to 4 MB (the hard cap)."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["text", "images"],
                        "description": "PDF read mode (ignored for non-PDF \
                                        files). `text` (default) extracts the \
                                        text layer. `images` rasterises the \
                                        pages to images for you to look at — \
                                        use it only when `text` returned no \
                                        usable text (a scanned PDF)."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        let sandbox = self.sandbox.clone();
        Box::pin(async move {
            let args: FetchArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{id, max_bytes?}}: {e}")))?;

            let (turn_id, filename) = split_id(&args.id)?;
            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "chat attachments are not configured on this gateway \
                     (operator must set [chat.s3])"
                        .into(),
                )
            })?;

            let fetched = chat_attachments::fetch(s3, turn_id, filename)
                .await
                .map_err(|e| ToolError::Failed(format!("s3 GET failed: {e}")))?;

            let cap = args
                .max_bytes
                .unwrap_or(HARD_MAX_BYTES_DEFAULT)
                .min(HARD_MAX_BYTES);
            let mime = fetched.mime.clone();

            // Office formats (docx/pptx/xlsx): not text, not PDF. Route to the
            // sandbox extractor for verbatim structured content the model can
            // re-author into a template (letter/presentation/one-pager) without
            // changing wording. Falls through to the binary stub with no sandbox.
            if let (Some(ext), Some(sb)) = (office_ext(&mime, filename), sandbox.as_ref()) {
                let document = extract_office(sb, &ctx, s3, turn_id, ext, fetched.bytes).await?;
                return Ok(json!({
                    "id": args.id,
                    "filename": filename,
                    "mime": mime,
                    "kind": "document_structure",
                    "note": "Verbatim structured content of the uploaded file. \
                             Re-author it into a template (typst_presentation / \
                             typst_letter / typst_onepager) WITHOUT changing the \
                             wording. Any embedded images are listed under \
                             `image_refs` (file → `att:` ref); to carry an image \
                             into a render, copy its `ref` string verbatim into \
                             the matching image field (image / bg_image / avatar \
                             / banner). The renderer resolves `att:` refs to the \
                             real pixels — do NOT invent file paths for them.",
                    "document": document,
                }));
            }

            // PDFs get their own two-tier path (text layer, then
            // page-images on escalation) instead of the generic
            // "binary — ask the user to re-upload" stub.
            if chat_attachments::is_pdf(&mime, filename) {
                return read_pdf(&args.id, filename, &mime, fetched.bytes, args.mode, cap).await;
            }

            let limits = PayloadLimits {
                max_text_bytes: cap,
                max_image_bytes: MAX_IMAGE_BYTES,
            };

            match chat_attachments::classify_payload(&mime, filename, fetched.bytes, limits) {
                BinaryDisposition::Text {
                    content,
                    bytes_returned,
                    truncated,
                    original_len,
                } => Ok(json!({
                    "id": args.id,
                    "filename": filename,
                    "mime": mime,
                    "size": original_len,
                    "kind": "text",
                    "content": content,
                    "bytes_returned": bytes_returned,
                    "bytes_original": original_len,
                    "truncated": truncated,
                })),
                BinaryDisposition::Image {
                    data_uri,
                    original_len,
                } => {
                    // `tool_content_parts(...)` envelope: the driver
                    // splices this into the upstream `role:"tool"`
                    // message as an array-of-parts so vision models
                    // actually see the image. OpenAI Chat Completions
                    // accepts `data:` URIs in `image_url`.
                    let summary = format!(
                        "Re-attached image `{filename}` ({mime}, {original_len} bytes, id={id}).",
                        id = args.id,
                    );
                    Ok(tool_content_parts(vec![
                        json!({"type": "text", "text": summary}),
                        json!({"type": "image_url", "image_url": {"url": data_uri}}),
                    ]))
                }
                BinaryDisposition::Binary { original_len } => {
                    // Two cases land here: actual binary (zip/audio/…;
                    // PDFs are handled above) and over-cap images.
                    // Differentiate via mime so
                    // the model gets a precise reason rather than a
                    // generic "binary" stub for what is in fact an
                    // image.
                    let (kind, note) = if mime.starts_with("image/") {
                        (
                            "image-too-large",
                            format!(
                                "Image is {original_len} bytes; ceiling is \
                                 {MAX_IMAGE_BYTES} bytes for inline return. \
                                 Ask the user for a downscaled version if \
                                 you need to look at it."
                            ),
                        )
                    } else {
                        (
                            "binary",
                            "Non-image binary attachment — bytes can't be \
                             reattached via a tool result. Ask the user to \
                             re-upload if you need to inspect this file."
                                .to_string(),
                        )
                    };
                    Ok(json!({
                        "id": args.id,
                        "filename": filename,
                        "mime": mime,
                        "size": original_len,
                        "kind": kind,
                        "note": note,
                    }))
                }
            }
        })
    }
}

/// Hard cap on pages rasterised in one `mode="images"` call — pages
/// ride back as inline images, which is expensive, so we bound it and
/// tell the model how many of how many it got. Mirrors
/// [`pdf::DEFAULT_MAX_RENDER_PAGES`].
const MAX_RENDER_PAGES: usize = pdf::DEFAULT_MAX_RENDER_PAGES;

/// Read a PDF attachment in the requested [`FetchMode`]. CPU-bound
/// PDF work runs on a blocking thread (`pdfium`'s handles are `!Send`
/// and text extraction is synchronous).
async fn read_pdf(
    id: &str,
    filename: &str,
    mime: &str,
    bytes: Vec<u8>,
    mode: FetchMode,
    text_cap: usize,
) -> Result<Value, ToolError> {
    let original_len = bytes.len();
    match mode {
        FetchMode::Text => {
            let text = tokio::task::spawn_blocking(move || pdf::extract_text(&bytes))
                .await
                .map_err(|e| ToolError::Failed(format!("pdf text extraction panicked: {e}")))?;
            match text {
                // No usable text layer — almost always a scanned /
                // image-only PDF. Steer the model to the image tier
                // instead of letting it give up.
                Ok(text) if text.trim().is_empty() => Ok(json!({
                    "id": id,
                    "filename": filename,
                    "mime": mime,
                    "size": original_len,
                    "kind": "pdf-no-text",
                    "note": "This PDF has no extractable text layer — it is \
                             most likely scanned or image-only. Call \
                             fetch_attachment again with the same id and \
                             mode=\"images\" to read it as rendered page \
                             images.",
                })),
                Ok(text) => {
                    let (slice, truncated) = truncate_on_char_boundary(&text, text_cap);
                    Ok(json!({
                        "id": id,
                        "filename": filename,
                        "mime": mime,
                        "size": original_len,
                        "kind": "text",
                        "content": slice,
                        "bytes_returned": slice.len(),
                        "truncated": truncated,
                        "extracted_from": "pdf-text-layer",
                        "note": "Extracted from the PDF text layer. If this \
                                 looks incomplete or garbled (e.g. a scanned \
                                 document), call again with mode=\"images\" \
                                 to read the pages as rendered images.",
                    }))
                }
                // Parse failure: the text crate choked on the document.
                // Rendering uses a different (pdfium) parser, so it may
                // still succeed — point the model there.
                Err(e) => Ok(json!({
                    "id": id,
                    "filename": filename,
                    "mime": mime,
                    "size": original_len,
                    "kind": "pdf-error",
                    "note": format!(
                        "Could not extract text from this PDF ({e}). Try \
                         calling again with mode=\"images\" to render the \
                         pages, or ask the user to re-upload."
                    ),
                })),
            }
        }
        FetchMode::Images => {
            let rendered =
                tokio::task::spawn_blocking(move || pdf::render_pages(&bytes, MAX_RENDER_PAGES))
                    .await
                    .map_err(|e| ToolError::Failed(format!("pdf rendering panicked: {e}")))?;
            match rendered {
                Ok(rendered) if !rendered.pages.is_empty() => {
                    let shown = rendered.pages.len();
                    let summary = if rendered.total_pages > shown {
                        format!(
                            "Rendered the first {shown} of {} pages of `{filename}` \
                             as images (id={id}).",
                            rendered.total_pages,
                        )
                    } else {
                        format!(
                            "Rendered all {shown} page(s) of `{filename}` as images \
                             (id={id})."
                        )
                    };
                    let mut parts = vec![json!({"type": "text", "text": summary})];
                    for page in &rendered.pages {
                        let uri = chat_attachments::to_data_uri("image/png", &page.png);
                        parts.push(
                            json!({"type": "text", "text": format!("Page {}:", page.page_number)}),
                        );
                        parts.push(json!({"type": "image_url", "image_url": {"url": uri}}));
                    }
                    Ok(tool_content_parts(parts))
                }
                // Empty document — nothing to show.
                Ok(_) => Ok(json!({
                    "id": id,
                    "filename": filename,
                    "mime": mime,
                    "size": original_len,
                    "kind": "pdf-empty",
                    "note": "This PDF has no pages to render.",
                })),
                // The native pdfium library isn't deployed on this
                // gateway. The text tier still works; tell the model so
                // it can fall back to that or ask the user.
                Err(PdfError::RendererUnavailable(_)) => Ok(json!({
                    "id": id,
                    "filename": filename,
                    "mime": mime,
                    "size": original_len,
                    "kind": "pdf-render-unavailable",
                    "note": "PDF page rendering isn't enabled on this gateway. \
                             Try mode=\"text\" instead, or ask the user to send \
                             a text version or a screenshot.",
                })),
                Err(e) => Ok(json!({
                    "id": id,
                    "filename": filename,
                    "mime": mime,
                    "size": original_len,
                    "kind": "pdf-error",
                    "note": format!("Could not render this PDF ({e})."),
                })),
            }
        }
    }
}

/// Truncate `s` to at most `max_bytes`, snapping back to the nearest
/// UTF-8 char boundary so we never split a multibyte codepoint.
/// Returns the slice plus whether anything was dropped.
fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

/// Split `<turn_id>/<filename>` into its parts. Rejects ids with
/// multiple slashes, leading slashes, or empty components — keeps
/// the surface tight against a model that hallucinates a different
/// shape than the replay stub.
fn split_id(id: &str) -> Result<(&str, &str), ToolError> {
    let (turn_id, filename) = id.split_once('/').ok_or_else(|| {
        ToolError::InvalidArgs(format!(
            "id `{id}` is not of the form `<turn_id>/<filename>`"
        ))
    })?;
    if turn_id.is_empty() || filename.is_empty() || filename.contains('/') {
        return Err(ToolError::InvalidArgs(format!(
            "id `{id}` has empty or nested segments"
        )));
    }
    Ok((turn_id, filename))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_id_accepts_simple_form() {
        assert_eq!(split_id("t-1/x.csv").unwrap(), ("t-1", "x.csv"));
    }

    #[test]
    fn split_id_rejects_nested_filename() {
        assert!(matches!(
            split_id("t-1/sub/x.csv").unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn split_id_rejects_missing_slash() {
        assert!(matches!(
            split_id("bareword").unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn split_id_rejects_empty_segments() {
        assert!(matches!(
            split_id("/x.csv").unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
        assert!(matches!(
            split_id("t-1/").unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn schema_names_match_id() {
        assert_eq!(
            FetchAttachment::new(None).id(),
            FetchAttachment::new(None).schema().function.name
        );
    }

    #[tokio::test]
    async fn errors_cleanly_when_s3_not_configured() {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let ctx = ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: pool,
            s3: None,
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
            image_gen: None,
        };
        let err = FetchAttachment::new(None)
            .run(ctx, json!({"id": "t-1/x.csv"}))
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("not configured"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_malformed_id_before_touching_s3() {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let ctx = ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: pool,
            // Deliberately Some so we'd reach the s3 call if validation slipped —
            // the test asserts we don't get that far.
            s3: Some(std::sync::Arc::new(crate::server::config::S3Config {
                endpoint: "http://127.0.0.1:1".into(),
                region: "us-east-1".into(),
                bucket: "b".into(),
                access_key_env: "FETCH_ATTACHMENT_TEST_NOT_SET".into(),
                secret_key_env: "FETCH_ATTACHMENT_TEST_NOT_SET".into(),
                key_prefix: "chat-attachments".into(),
            })),
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
            image_gen: None,
        };
        let err = FetchAttachment::new(None)
            .run(ctx, json!({"id": "nope"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }

    // --- PDF tier wiring ---------------------------------------------------
    //
    // `read_pdf` takes bytes directly (the S3 fetch happens upstream of it),
    // so these pin the text/images contract end-to-end without a bucket.

    use crate::server::pdf::test_support::{blank_pdf, hello_pdf};

    #[test]
    fn mode_defaults_to_text_and_parses_images() {
        let default: FetchArgs = serde_json::from_value(json!({"id": "t/x.pdf"})).unwrap();
        assert_eq!(default.mode, FetchMode::Text);
        let images: FetchArgs =
            serde_json::from_value(json!({"id": "t/x.pdf", "mode": "images"})).unwrap();
        assert_eq!(images.mode, FetchMode::Images);
        // Unknown mode is rejected at the arg-parse boundary.
        assert!(
            serde_json::from_value::<FetchArgs>(json!({"id": "t/x.pdf", "mode": "ocr"})).is_err()
        );
    }

    #[test]
    fn schema_advertises_pdf_mode() {
        let schema = FetchAttachment::new(None).schema();
        let modes = &schema.function.parameters["properties"]["mode"]["enum"];
        assert_eq!(*modes, json!(["text", "images"]));
    }

    #[tokio::test]
    async fn pdf_text_mode_returns_extracted_text() {
        let out = read_pdf(
            "t/x.pdf",
            "x.pdf",
            "application/pdf",
            hello_pdf(),
            FetchMode::Text,
            4096,
        )
        .await
        .unwrap();
        assert_eq!(out["kind"], "text");
        assert!(
            out["content"].as_str().unwrap().contains("Hello PDF"),
            "{out:?}"
        );
        assert_eq!(out["extracted_from"], "pdf-text-layer");
    }

    #[tokio::test]
    async fn pdf_text_mode_flags_scanned_when_no_text_layer() {
        // A page with an empty content stream looks like a scanned PDF to
        // the text tier — the model must be steered to mode="images".
        let out = read_pdf(
            "t/scan.pdf",
            "scan.pdf",
            "application/pdf",
            blank_pdf(),
            FetchMode::Text,
            4096,
        )
        .await
        .unwrap();
        assert_eq!(out["kind"], "pdf-no-text", "{out:?}");
        assert!(
            out["note"].as_str().unwrap().contains("mode=\"images\""),
            "the note must point the model at the image tier: {out:?}"
        );
    }

    #[tokio::test]
    async fn pdf_images_mode_renders_or_degrades_cleanly() {
        // With pdfium installed this returns a tool_content_parts envelope
        // (text summary + image_url parts); without it, a clean
        // pdf-render-unavailable note. Both are valid — never an error.
        let out = read_pdf(
            "t/x.pdf",
            "x.pdf",
            "application/pdf",
            hello_pdf(),
            FetchMode::Images,
            4096,
        )
        .await
        .unwrap();
        if let Some(parts) = out.get("__gateway_tool_content_parts") {
            let parts = parts.as_array().unwrap();
            assert!(
                parts.iter().any(|p| p["type"] == "image_url"),
                "rendered output must carry an image_url part: {out:?}"
            );
        } else {
            assert_eq!(out["kind"], "pdf-render-unavailable", "{out:?}");
        }
    }

    #[test]
    fn truncate_on_char_boundary_never_splits_a_codepoint() {
        // "é" is 2 bytes; capping at 1 byte must snap back to 0, not panic.
        let (slice, truncated) = truncate_on_char_boundary("é", 1);
        assert_eq!(slice, "");
        assert!(truncated);
        let (slice, truncated) = truncate_on_char_boundary("abc", 10);
        assert_eq!(slice, "abc");
        assert!(!truncated);
    }
}
