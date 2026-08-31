// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Reading `.docx` / `.pptx` / `.xlsx` through the sandbox.
//!
//! One python extractor, two consumers with different needs:
//!
//!   * `fetch_attachment` wants the **structured** result — titles, bullets,
//!     tables, notes, plus the embedded images re-attached as `att:` refs —
//!     so a model can re-author an upload into one of our templates without
//!     losing anything.
//!   * The RAG indexer wants **flat text** to chunk and embed. Images and
//!     structure are noise there; what matters is that the words in a
//!     contract or a deck become searchable.
//!
//! The script lives here, below both, because it is the thing that must not
//! be written twice: two copies of the office-parsing logic would drift, and
//! the one used for indexing would be the one nobody noticed had rotted.
//! `gateway-tools` sits above this crate, so this is the lowest layer both
//! callers can share.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as b64;
use serde_json::Value;
use shared::sandbox::{InputFile, Language, RunRequest};

use super::SandboxClient;
use crate::server::tools::ToolError;

/// Verbatim structured extractor run in the sandbox. Dispatches by the
/// input file's extension (python-pptx / python-docx / openpyxl). Prints one
/// JSON object of the document's content — titles, text, bullets, tables,
/// notes, image filenames — with NO modification, so the model can re-author
/// it into a template (letter / presentation / one-pager) losslessly. Each
/// embedded image is written to `/work` top-level (NOT a subdir) so the
/// sandbox agent returns it as an artifact for the gateway to re-attach.
pub const EXTRACT_PY: &str = r#"import sys, json, os
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

/// The last `n` characters of `s`, in reading order.
///
/// The tail is what matters in a python traceback — the exception is on the
/// last line. (The previous `.rev().take(n)` returned it *backwards*, which
/// made every extraction failure unreadable.)
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// What one extractor run produced: the parsed JSON plus whatever images the
/// script wrote to `/work`.
pub struct OfficeExtraction {
    pub document: Value,
    pub artifacts: Vec<shared::sandbox::Artifact>,
}

/// Run [`EXTRACT_PY`] over `bytes` in the sandbox. The document rides in as
/// `upload.<ext>` so the extractor dispatches on the real format.
pub async fn run_office_extractor(
    sandbox: &SandboxClient,
    ext: &str,
    bytes: Vec<u8>,
) -> Result<OfficeExtraction, ToolError> {
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
            content_b64: b64.encode(&bytes),
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
            tail_chars(&resp.stderr, 400)
        )));
    }
    // A truncated stdout is not malformed JSON, it is a document too big to
    // come back through the runner's output cap — and it will be just as big
    // next time. Saying so turns "extractor did not return JSON", which reads
    // like a bug to chase, into a limit an operator can act on.
    if resp.output_truncated {
        return Err(ToolError::Failed(format!(
            "the extracted content of this document exceeded the sandbox output limit \
             ({} KiB returned). Very large spreadsheets and decks hit this; the document \
             is skipped rather than indexed in part.",
            resp.stdout.len() / 1024
        )));
    }
    let document: Value = serde_json::from_str(resp.stdout.trim())
        .map_err(|e| ToolError::Failed(format!("extractor did not return JSON: {e}")))?;
    Ok(OfficeExtraction {
        document,
        artifacts: resp.artifacts,
    })
}

/// The array at `key`, or empty — the extractor omits sections a document
/// does not have, and a missing section is not an error.
fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Flatten the extractor's structured output into plain text for indexing.
///
/// Deliberately lossy in the direction that helps retrieval: headings,
/// paragraphs, bullets, table cells and speaker notes all become lines of
/// text, because a chunk of a contract is searched for its words, not its
/// shape. Image filenames are dropped — they carry no searchable meaning.
pub fn office_text(document: &Value) -> String {
    let mut out = String::new();
    let mut push = |s: &str| {
        let s = s.trim();
        if !s.is_empty() {
            out.push_str(s);
            out.push('\n');
        }
    };
    fn rows_text(value: &Value, push: &mut impl FnMut(&str)) {
        // Table rows arrive as [[cell, cell], …]; a tab-joined row keeps
        // columns distinguishable without inventing markdown.
        if let Some(rows) = value.as_array() {
            for row in rows {
                if let Some(cells) = row.as_array() {
                    let line: Vec<&str> = cells.iter().filter_map(|c| c.as_str()).collect();
                    push(&line.join("\t"));
                }
            }
        }
    }

    match document.get("kind").and_then(Value::as_str) {
        Some("presentation") => {
            for slide in arr(document, "content") {
                if let Some(t) = slide.get("title").and_then(Value::as_str) {
                    push(t);
                }
                for t in arr(slide, "text") {
                    if let Some(t) = t.as_str() {
                        push(t);
                    }
                }
                for group in arr(slide, "bullets") {
                    for b in group.as_array().map(Vec::as_slice).unwrap_or_default() {
                        if let Some(b) = b.as_str() {
                            push(b);
                        }
                    }
                }
                for table in arr(slide, "tables") {
                    rows_text(table, &mut push);
                }
                if let Some(n) = slide.get("notes").and_then(Value::as_str) {
                    push(n);
                }
            }
        }
        Some("document") => {
            for p in arr(document, "paragraphs") {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    push(t);
                }
            }
            for table in arr(document, "tables") {
                rows_text(table, &mut push);
            }
        }
        Some("spreadsheet") => {
            for sheet in arr(document, "sheets") {
                if let Some(name) = sheet.get("name").and_then(Value::as_str) {
                    push(name);
                }
                if let Some(rows) = sheet.get("rows") {
                    rows_text(rows, &mut push);
                }
            }
        }
        _ => {}
    }
    out
}

/// Adapts the sandbox into the indexer's `OfficeExtractor` port.
///
/// The indexer lives in `gateway-features`, which sits *below* this crate and
/// must never name it (see `docs/architecture.md`). So the capability is
/// declared down there as a trait and implemented up here, injected at boot.
pub struct SandboxOfficeExtractor {
    sandbox: std::sync::Arc<SandboxClient>,
}

impl SandboxOfficeExtractor {
    pub fn new(sandbox: std::sync::Arc<SandboxClient>) -> Self {
        Self { sandbox }
    }
}

impl gateway_features::server::rag::extract::OfficeExtractor for SandboxOfficeExtractor {
    fn extract_office<'a>(
        &'a self,
        ext: &'a str,
        bytes: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        Box::pin(async move {
            let extracted = run_office_extractor(&self.sandbox, ext, bytes)
                .await
                .map_err(|e| e.to_string())?;
            Ok(office_text(&extracted.document))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_error_tail_reads_forwards() {
        // A traceback's exception is on the last line; reversing the string
        // makes the only diagnostic an operator gets unreadable.
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("short", 99), "short");
    }

    #[test]
    fn a_deck_flattens_to_its_words_in_reading_order() {
        let doc = json!({
            "kind": "presentation",
            "content": [{
                "index": 1,
                "title": "Q3 Results",
                "text": ["Revenue is up"],
                "bullets": [["EMEA grew 12%", "APAC flat"]],
                "tables": [[["Region", "Growth"], ["EMEA", "12%"]]],
                "notes": "Mention the pipeline",
                "images": ["slide1_img1.png"]
            }]
        });
        let text = office_text(&doc);
        for expected in [
            "Q3 Results",
            "Revenue is up",
            "EMEA grew 12%",
            "APAC flat",
            "Region\tGrowth",
            "Mention the pipeline",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text:?}");
        }
        assert!(
            !text.contains("slide1_img1.png"),
            "an image filename is not searchable content"
        );
    }

    #[test]
    fn a_word_document_keeps_paragraphs_and_tables() {
        let doc = json!({
            "kind": "document",
            "paragraphs": [
                {"style": "Heading 1", "text": "Service Agreement"},
                {"style": "Normal", "text": "The supplier shall…"}
            ],
            "tables": [[["Term", "Value"], ["Notice", "30 days"]]]
        });
        let text = office_text(&doc);
        assert!(text.contains("Service Agreement"));
        assert!(text.contains("The supplier shall…"));
        assert!(text.contains("Notice\t30 days"));
    }

    #[test]
    fn a_spreadsheet_keeps_sheet_names_and_cells() {
        let doc = json!({
            "kind": "spreadsheet",
            "sheets": [{"name": "Invoices", "rows": [["Vendor", "Total"], ["ACME", "1234.56"]]}]
        });
        let text = office_text(&doc);
        assert!(text.contains("Invoices"));
        assert!(text.contains("ACME\t1234.56"));
    }

    #[test]
    fn an_unrecognised_shape_yields_nothing_rather_than_garbage() {
        assert!(office_text(&json!({"error": "unsupported: .odt"})).is_empty());
    }
}
