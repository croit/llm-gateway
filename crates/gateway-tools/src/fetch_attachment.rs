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
//! - **PDF**: two model-driven tiers (see [`gateway_features::server::pdf`]).
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

use gateway_features::server::chat_attachments::{self, BinaryDisposition, PayloadLimits};
use gateway_features::server::pdf::{self, PdfError};
use gateway_runtime::server::tools::sandbox::{SandboxClient, b64};
use gateway_runtime::server::tools::{
    Tool, ToolContext, ToolError, ToolFuture, tool_content_parts, truncate_on_char_boundary,
};

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
    s3: &gateway_core::server::config::S3Config,
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
        container_id: None,
        keep_alive: false,
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
    /// 1-based first PDF page to read (inclusive). PDF-only.
    #[serde(default)]
    page_from: Option<usize>,
    /// 1-based last PDF page to read (inclusive). PDF-only.
    #[serde(default)]
    page_to: Option<usize>,
}

/// A validated, 1-based inclusive PDF page window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageRange {
    from: usize,
    to: Option<usize>,
}

impl PageRange {
    /// Validate the model-supplied window. Rejects a zero `page_from`
    /// (pages are 1-based, and a 0 almost always means the model was
    /// counting from zero — better to say so than to silently read a
    /// different range) and an inverted range.
    fn parse(from: Option<usize>, to: Option<usize>) -> Result<Self, ToolError> {
        if from == Some(0) || to == Some(0) {
            return Err(ToolError::InvalidArgs(
                "page numbers are 1-based — page_from/page_to must be >= 1".into(),
            ));
        }
        if let (Some(f), Some(t)) = (from, to)
            && f > t
        {
            return Err(ToolError::InvalidArgs(format!(
                "page_from ({f}) must not be greater than page_to ({t})"
            )));
        }
        Ok(Self {
            from: from.unwrap_or(1),
            to,
        })
    }

    /// Whether the caller asked for a specific window (rather than
    /// "start at the beginning, give me what fits").
    fn is_explicit(&self) -> bool {
        self.from > 1 || self.to.is_some()
    }

    /// How many pages this window spans, capped at `ceiling`.
    fn len_capped(&self, ceiling: usize) -> usize {
        match self.to {
            Some(to) => (to + 1).saturating_sub(self.from).min(ceiling),
            None => ceiling,
        }
    }
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
             actually see. Both PDF tiers accept `page_from`/`page_to` \
             (1-based, inclusive) — a long document comes back one window at \
             a time and the result tells you which pages you got out of how \
             many, so page on with `page_from` set past the last one you \
             read. Office files (`.docx`/`.pptx`/`.xlsx`) return \
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
                                        appeared in the replay stub — or just a \
                                        filename from this conversation (newest \
                                        match wins; chat sessions only)."
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
                    },
                    "page_from": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "PDF only: 1-based first page to read \
                                        (inclusive). Use this to page through \
                                        a long document — a result that says \
                                        it returned pages 1–8 of 200 is read \
                                        further with page_from: 9."
                    },
                    "page_to": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "PDF only: 1-based last page to read \
                                        (inclusive). Omit to read as far as \
                                        the per-call limit allows."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        let sandbox = self.sandbox.clone();
        Box::pin(async move {
            let mut args: FetchArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{id, max_bytes?}}: {e}")))?;

            // A bare filename (no `/`) resolves against the session's
            // attachments, newest match first — so the model can re-read a
            // file it produced earlier without tracking turn ids. Only the
            // chat path has a session to resolve against.
            if !args.id.contains('/') {
                let Some(session_id) = ctx.session_id.as_deref() else {
                    return Err(ToolError::InvalidArgs(format!(
                        "`{}` is not a `<turn_id>/<filename>` id — bare filenames resolve \
                         only inside a chat session",
                        args.id
                    )));
                };
                let atts = chat_attachments::list_session_attachments(&ctx.db, session_id)
                    .await
                    .map_err(|e| ToolError::Failed(format!("listing session attachments: {e}")))?;
                args.id = chat_attachments::resolve_attachment(&atts, &args.id)
                    .map(|a| a.id.clone())
                    .ok_or_else(|| {
                        ToolError::InvalidArgs(format!(
                            "no attachment named `{}` in this conversation — call \
                             `list_attachments` to see what exists",
                            args.id
                        ))
                    })?;
            }
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
                let pages = PageRange::parse(args.page_from, args.page_to)?;
                return read_pdf(
                    &args.id,
                    filename,
                    &mime,
                    fetched.bytes,
                    args.mode,
                    cap,
                    pages,
                )
                .await;
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
    pages: PageRange,
) -> Result<Value, ToolError> {
    let original_len = bytes.len();
    match mode {
        // An explicit window uses the per-page extractor so the requested
        // pages are what gets returned. Without a window we stay on the
        // whole-document path — same parse cost, but it keeps the default
        // result byte-identical to what it has always been.
        FetchMode::Text if pages.is_explicit() => {
            read_pdf_text_range(id, filename, mime, bytes, text_cap, pages).await
        }
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
                    // Only mention paging when it actually applies —
                    // an untruncated document needs no follow-up call.
                    let note = if truncated {
                        "Extracted from the PDF text layer, and cut off at the \
                         byte cap. Read further with page_from/page_to (1-based, \
                         inclusive) rather than raising max_bytes. If the text \
                         looks garbled (e.g. a scanned document), call again \
                         with mode=\"images\"."
                    } else {
                        "Extracted from the PDF text layer. If this looks \
                         incomplete or garbled (e.g. a scanned document), call \
                         again with mode=\"images\" to read the pages as \
                         rendered images."
                    };
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
                        "note": note,
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
            let settings = pdf::RenderSettings {
                first_page: pages.from,
                max_pages: pages.len_capped(MAX_RENDER_PAGES),
                ..pdf::RenderSettings::default()
            };
            let rendered = tokio::task::spawn_blocking(move || {
                pdf::render_pages_with_settings(&bytes, settings)
            })
            .await
            .map_err(|e| ToolError::Failed(format!("pdf rendering panicked: {e}")))?;
            match rendered {
                Ok(rendered) if !rendered.pages.is_empty() => {
                    let total = rendered.total_pages;
                    let first = rendered.pages[0].page_number;
                    let last = rendered.pages[rendered.pages.len() - 1].page_number;
                    let mut summary = if first == 1 && last == total {
                        format!(
                            "Rendered all {total} page(s) of `{filename}` as images \
                             (id={id})."
                        )
                    } else {
                        format!(
                            "Rendered pages {first}–{last} of {total} of `{filename}` \
                             as images (id={id})."
                        )
                    };
                    // Spell out the exact next call. A bare "N of M" leaves
                    // the model to infer that paging is possible at all,
                    // which it reliably does not do.
                    if last < total {
                        summary.push_str(&format!(
                            " To continue, call fetch_attachment again with \
                             mode=\"images\" and page_from={}.",
                            last + 1
                        ));
                    }
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
                // Nothing rendered. Distinguish "the document is empty" from
                // "you asked for a window past its end" — the second is a
                // fixable mistake and must not read as the first.
                Ok(rendered) if rendered.total_pages > 0 => Err(ToolError::InvalidArgs(format!(
                    "`{filename}` has {} page(s); page_from={} is past the end",
                    rendered.total_pages, pages.from
                ))),
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

/// Serve an explicit page window from a PDF's text layer.
///
/// Split out from [`read_pdf`] because it answers a different question: not
/// "what does this document say" but "what do pages 40–60 say". The page
/// markers matter — a model asked to cite a page number can only do that if
/// the result says which page each passage came from.
async fn read_pdf_text_range(
    id: &str,
    filename: &str,
    mime: &str,
    bytes: Vec<u8>,
    text_cap: usize,
    range: PageRange,
) -> Result<Value, ToolError> {
    let original_len = bytes.len();
    let extracted = tokio::task::spawn_blocking(move || pdf::extract_text_pages(&bytes))
        .await
        .map_err(|e| ToolError::Failed(format!("pdf text extraction panicked: {e}")))?;
    let page_texts = match extracted {
        Ok(pages) => pages,
        // Same reasoning as the whole-document path: the render tier uses a
        // different parser and may still manage this file.
        Err(e) => {
            return Ok(json!({
                "id": id,
                "filename": filename,
                "mime": mime,
                "size": original_len,
                "kind": "pdf-error",
                "note": format!(
                    "Could not extract text from this PDF ({e}). Try calling \
                     again with mode=\"images\" to render the pages, or ask the \
                     user to re-upload."
                ),
            }));
        }
    };

    let total_pages = page_texts.len();
    if total_pages == 0 {
        return Ok(json!({
            "id": id,
            "filename": filename,
            "mime": mime,
            "size": original_len,
            "kind": "pdf-empty",
            "note": "This PDF has no pages.",
        }));
    }
    if range.from > total_pages {
        return Err(ToolError::InvalidArgs(format!(
            "`{filename}` has {total_pages} page(s); page_from={} is past the end",
            range.from
        )));
    }
    let last_requested = range.to.unwrap_or(total_pages).min(total_pages);

    // Build the window, stopping early if the byte cap fills up — an
    // arbitrarily wide `page_from: 1, page_to: 500` must not blow the cap.
    let mut content = String::new();
    let mut last_included = range.from - 1;
    for page_no in range.from..=last_requested {
        let body = page_texts[page_no - 1].trim();
        let mut chunk = String::with_capacity(body.len() + 16);
        if !content.is_empty() {
            chunk.push_str("\n\n");
        }
        chunk.push_str(&format!("[page {page_no}]\n"));
        chunk.push_str(body);
        // Always take at least one page, even if it alone exceeds the cap;
        // the truncation below then trims it. Returning zero pages for a
        // valid request would be worse than returning a clipped one.
        if !content.is_empty() && content.len() + chunk.len() > text_cap {
            break;
        }
        content.push_str(&chunk);
        last_included = page_no;
    }

    let (slice, clipped) = truncate_on_char_boundary(&content, text_cap);
    let more = last_included < total_pages;
    let mut note = format!(
        "Text layer of pages {}–{last_included} of {total_pages}.",
        range.from
    );
    if last_included < last_requested {
        note.push_str(
            " Stopped early at the byte cap — the requested range did not fit \
             in one call.",
        );
    }
    if more {
        note.push_str(&format!(" Continue with page_from={}.", last_included + 1));
    }
    if slice.trim_start().starts_with("[page") && slice.trim().lines().count() <= 1 {
        note.push_str(
            " These pages carry no text layer — if the document is scanned, \
             call again with mode=\"images\".",
        );
    }

    Ok(json!({
        "id": id,
        "filename": filename,
        "mime": mime,
        "size": original_len,
        "kind": "text",
        "content": slice,
        "bytes_returned": slice.len(),
        "truncated": clipped || more,
        "pages_returned": [range.from, last_included],
        "total_pages": total_pages,
        "extracted_from": "pdf-text-layer",
        "note": note,
    }))
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
        let pool = gateway_core::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let ctx = ToolContext::for_test(pool);
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
        let pool = gateway_core::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let ctx = ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: pool,
            // Deliberately Some so we'd reach the s3 call if validation slipped —
            // the test asserts we don't get that far.
            s3: Some(std::sync::Arc::new(
                gateway_core::server::config::S3Config {
                    endpoint: "http://127.0.0.1:1".into(),
                    region: "us-east-1".into(),
                    bucket: "b".into(),
                    access_key_env: "FETCH_ATTACHMENT_TEST_NOT_SET".into(),
                    secret_key_env: "FETCH_ATTACHMENT_TEST_NOT_SET".into(),
                    key_prefix: "chat-attachments".into(),
                },
            )),
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
            image_gen: None,
            sandbox_lease: None,
            browser_lease: None,
            crypto: None,
            push: None,
            model: None,
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

    use gateway_features::server::pdf::test_support::{blank_pdf, hello_pdf};

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

    #[test]
    fn schema_advertises_page_range() {
        let schema = FetchAttachment::new(None).schema();
        let props = &schema.function.parameters["properties"];
        assert_eq!(props["page_from"]["type"], "integer");
        assert_eq!(props["page_from"]["minimum"], 1);
        assert_eq!(props["page_to"]["type"], "integer");
    }

    #[test]
    fn page_range_rejects_zero_and_inverted_ranges() {
        assert!(matches!(
            PageRange::parse(Some(0), None).unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
        assert!(matches!(
            PageRange::parse(None, Some(0)).unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
        assert!(matches!(
            PageRange::parse(Some(9), Some(4)).unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
        // Equal bounds are a valid single-page window.
        assert!(PageRange::parse(Some(4), Some(4)).is_ok());
    }

    #[test]
    fn page_range_defaults_are_not_explicit() {
        // The unwindowed default must keep taking the whole-document path,
        // so existing callers see byte-identical results.
        assert!(!PageRange::parse(None, None).unwrap().is_explicit());
        assert!(PageRange::parse(Some(2), None).unwrap().is_explicit());
        assert!(PageRange::parse(None, Some(2)).unwrap().is_explicit());
    }

    #[test]
    fn page_range_len_is_capped() {
        let r = PageRange::parse(Some(3), Some(6)).unwrap();
        assert_eq!(r.len_capped(100), 4);
        assert_eq!(r.len_capped(2), 2);
        // Open-ended window falls back to the caller's ceiling.
        let open = PageRange::parse(Some(3), None).unwrap();
        assert_eq!(open.len_capped(8), 8);
    }

    #[tokio::test]
    async fn pdf_text_range_returns_only_the_requested_pages() {
        let out = read_pdf(
            "t/doc.pdf",
            "doc.pdf",
            "application/pdf",
            gateway_features::server::pdf::test_support::multipage_pdf(6),
            FetchMode::Text,
            4096,
            PageRange::parse(Some(3), Some(4)).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(out["kind"], "text");
        let content = out["content"].as_str().unwrap();
        assert!(content.contains("Page 3 body"), "{content}");
        assert!(content.contains("Page 4 body"), "{content}");
        assert!(!content.contains("Page 2 body"), "{content}");
        assert!(!content.contains("Page 5 body"), "{content}");
        assert_eq!(out["pages_returned"], json!([3, 4]));
        assert_eq!(out["total_pages"], 6);
    }

    #[tokio::test]
    async fn pdf_text_range_marks_page_numbers_for_citation() {
        let out = read_pdf(
            "t/doc.pdf",
            "doc.pdf",
            "application/pdf",
            gateway_features::server::pdf::test_support::multipage_pdf(3),
            FetchMode::Text,
            4096,
            PageRange::parse(Some(2), Some(2)).unwrap(),
        )
        .await
        .unwrap();
        assert!(
            out["content"].as_str().unwrap().contains("[page 2]"),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn pdf_text_range_tells_the_model_how_to_continue() {
        let out = read_pdf(
            "t/doc.pdf",
            "doc.pdf",
            "application/pdf",
            gateway_features::server::pdf::test_support::multipage_pdf(10),
            FetchMode::Text,
            4096,
            PageRange::parse(Some(1), Some(2)).unwrap(),
        )
        .await
        .unwrap();
        let note = out["note"].as_str().unwrap();
        assert!(note.contains("of 10"), "{note}");
        assert!(note.contains("page_from=3"), "{note}");
        assert_eq!(out["truncated"], true, "more pages remain: {out:?}");
    }

    #[tokio::test]
    async fn pdf_text_range_past_the_end_is_an_arg_error() {
        let err = read_pdf(
            "t/doc.pdf",
            "doc.pdf",
            "application/pdf",
            gateway_features::server::pdf::test_support::multipage_pdf(2),
            FetchMode::Text,
            4096,
            PageRange::parse(Some(50), None).unwrap(),
        )
        .await
        .unwrap_err();
        match err {
            ToolError::InvalidArgs(msg) => {
                assert!(msg.contains("2 page(s)"), "{msg}");
                assert!(msg.contains("past the end"), "{msg}");
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pdf_text_range_clamps_page_to_at_the_document_end() {
        // Asking for more than exists is not an error — it reads to the end.
        let out = read_pdf(
            "t/doc.pdf",
            "doc.pdf",
            "application/pdf",
            gateway_features::server::pdf::test_support::multipage_pdf(3),
            FetchMode::Text,
            4096,
            PageRange::parse(Some(2), Some(99)).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(out["pages_returned"], json!([2, 3]));
        assert_eq!(out["truncated"], false, "read to the end: {out:?}");
    }

    #[tokio::test]
    async fn pdf_text_range_stops_at_the_byte_cap_and_says_so() {
        // A wide window must not blow the cap; it returns what fits and
        // names the page to resume from.
        let out = read_pdf(
            "t/doc.pdf",
            "doc.pdf",
            "application/pdf",
            gateway_features::server::pdf::test_support::multipage_pdf(20),
            FetchMode::Text,
            60,
            PageRange::parse(Some(1), Some(20)).unwrap(),
        )
        .await
        .unwrap();
        let last = out["pages_returned"][1].as_u64().unwrap();
        assert!(last < 20, "should have stopped early, got {last}");
        let note = out["note"].as_str().unwrap();
        assert!(note.contains("byte cap"), "{note}");
        assert!(note.contains(&format!("page_from={}", last + 1)), "{note}");
    }

    #[tokio::test]
    async fn pdf_default_text_path_reports_paging_when_truncated() {
        // No explicit window: the whole-document path still has to tell the
        // model that page_from/page_to exist, or it will just raise max_bytes.
        let out = read_pdf(
            "t/doc.pdf",
            "doc.pdf",
            "application/pdf",
            gateway_features::server::pdf::test_support::multipage_pdf(10),
            FetchMode::Text,
            20,
            PageRange::parse(None, None).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(out["truncated"], true);
        assert!(
            out["note"].as_str().unwrap().contains("page_from"),
            "{out:?}"
        );
        // Whole-document path: no per-page bookkeeping in the result.
        assert!(out.get("pages_returned").is_none(), "{out:?}");
    }

    #[tokio::test]
    async fn pdf_images_range_past_the_end_is_an_arg_error_or_unavailable() {
        // With pdfium present this must be an InvalidArgs (not "pdf-empty",
        // which would read as "the document has no pages"). Without the
        // native library the render tier is unavailable — also acceptable.
        let out = read_pdf(
            "t/doc.pdf",
            "doc.pdf",
            "application/pdf",
            gateway_features::server::pdf::test_support::multipage_pdf(2),
            FetchMode::Images,
            4096,
            PageRange::parse(Some(50), None).unwrap(),
        )
        .await;
        match out {
            Err(ToolError::InvalidArgs(msg)) => {
                assert!(msg.contains("past the end"), "{msg}");
            }
            Ok(v) => assert_eq!(
                v["kind"], "pdf-render-unavailable",
                "only the missing-pdfium degradation is acceptable here: {v:?}"
            ),
            Err(other) => panic!("unexpected error: {other:?}"),
        }
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
            PageRange::parse(None, None).unwrap(),
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
            PageRange::parse(None, None).unwrap(),
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
            PageRange::parse(None, None).unwrap(),
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
