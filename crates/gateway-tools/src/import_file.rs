// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `import_file` — turn a file in the conversation into an editable canvas
//! document.
//!
//! The conversation holds files in two shapes, and until now they were one-way
//! streets:
//!
//! - **Attachments** (`<turn_id>/<filename>`) are immutable blobs — uploads,
//!   sandbox artifacts, rendered output. Great for delivery, useless for
//!   iteration: changing one line meant the model reading the whole file and
//!   writing the whole file back out through `upload_attachment`, paying for
//!   every byte twice and drifting on the parts it retyped.
//! - **Canvas documents** are mutable and versioned: anchored find/replace or
//!   a JSON Patch changes one passage, every change keeps history, the user
//!   sees it in the panel, `run_in_sandbox` stages it into `/work`, and
//!   `typst_*` renders straight from it.
//!
//! This tool is the on-ramp: an uploaded `.typ`, `.csv`, `.json` or `.md`
//! becomes a document, server-side, without its content passing through the
//! model at all. `offer_download` is the exit ramp in the other direction.
//!
//! Text only, by construction: a canvas document is TEXT in SQLite. Binary
//! attachments (images, PDFs, archives) stay attachments — they are already
//! usable by id (`att:` refs in templates, sandbox staging, `fetch_attachment`),
//! and there is nothing to hand-edit in a PNG.

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use gateway_core::server::db::documents::{self, DocumentFormat};
use gateway_features::server::chat_attachments;
use gateway_features::server::file_refs::{self, FileRef};
use gateway_runtime::server::tools::{Tool, ToolContext, ToolError, ToolFuture};

/// Cap on the imported content — the same one every other document writer
/// uses: the tools refuse to *write* more than this, so importing a bigger
/// file would create a document no edit could ever save.
const MAX_IMPORT_BYTES: usize = documents::MAX_CONTENT_BYTES;

pub struct ImportFile;

#[derive(Deserialize)]
struct ImportArgs {
    id: String,
    #[serde(default)]
    title: Option<String>,
    /// Override the format when the extension lies (a `.txt` that is really
    /// Typst source, say). Omitted → inferred from the filename, then the
    /// mime type.
    #[serde(default)]
    format: Option<String>,
}

impl Tool for ImportFile {
    fn id(&self) -> &str {
        "import_file"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Turn a text file in this conversation into an editable canvas \
             document, so it can be changed a passage at a time instead of \
             rewritten. Pass the `id` of an attachment (from `list_attachments`, \
             a `<turn_id>/<filename>` id, or just a filename — newest match \
             wins). The content is copied inside the gateway: you do NOT read \
             or re-emit it. Use this when the user uploads a file you will \
             work ON (a Typst source, a CSV, a JSON config, a draft in \
             markdown), or to make a produced artifact editable. Afterwards \
             `edit_document` changes one part of it, `read_document` reads it \
             back, the user sees and can hand-edit it in their document panel, \
             `run_in_sandbox` can stage it with `documents`, `typst_*` can \
             render from it with `document_id`, and `offer_download` hands it \
             back as a file. Text formats only — images, PDFs and archives \
             stay attachments (use them by id).",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Attachment id (`<turn_id>/<filename>`) or a \
                                        filename from this conversation."
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional title for the document panel. \
                                        Defaults to the filename."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["markdown", "text", "html", "json", "toml", "yaml", "typst"],
                        "description": "Optional. Override the format inferred from \
                                        the file's extension/mime. Structured \
                                        formats (json/toml) are edited with JSON \
                                        Patch, the rest with find/replace — so a \
                                        wrong guess only changes which edit \
                                        vocabulary applies."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: ImportArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{id, title?, format?}}: {e}"))
            })?;
            let session_id = ctx.session_id.as_deref().ok_or_else(|| {
                ToolError::Failed(
                    "import_file only works inside a chat session — there is no \
                     conversation to take a file from, and no canvas to put it in"
                        .into(),
                )
            })?;
            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "chat attachments are not configured on this gateway \
                     (operator must set [chat.s3])"
                        .into(),
                )
            })?;

            // The shared resolver: any spelling of an attachment reference,
            // session-scoped. A document id resolves too — and is refused
            // below, because importing a document into the canvas it already
            // lives in would fork it into two copies that drift apart.
            let att = match file_refs::resolve(&ctx.db, Some(session_id), &args.id, None).await? {
                FileRef::Attachment(a) => a,
                FileRef::UnlistedAttachment { turn_id, filename } => {
                    chat_attachments::AttachmentRef {
                        id: format!("{turn_id}/{filename}"),
                        turn_id,
                        filename,
                        // Unknown until the fetch below; only the extension drives
                        // the format guess for these (a hidden `.json` data base).
                        mime: "application/octet-stream".to_string(),
                        size: 0,
                    }
                }
                FileRef::Document { doc, version } => {
                    return Err(ToolError::InvalidArgs(format!(
                        "`{}` is already canvas document `{}` (v{}) — read it with \
                         `read_document` and change it with `edit_document`. Importing it \
                         again would fork it into a second copy that drifts from the one \
                         the user sees.",
                        args.id, doc.id, version.version
                    )));
                }
            };

            let format = match args.format.as_deref() {
                Some(s) => DocumentFormat::parse(s).ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "`{s}` is not a document format (markdown, text, html, json, \
                         toml, yaml, typst)"
                    ))
                })?,
                None => infer_format(&att.filename, &att.mime).ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "`{}` is `{}`, which isn't text the canvas can hold — images, \
                         PDFs and archives stay attachments and are already usable by \
                         id (`att:` refs, sandbox staging, fetch_attachment). Pass \
                         `format` if the file really is text under an unusual type.",
                        att.filename, att.mime
                    ))
                })?,
            };

            let fetched = chat_attachments::fetch(s3, &att.turn_id, &att.filename)
                .await
                .map_err(|e| ToolError::Failed(format!("could not read `{}`: {e}", att.id)))?;
            if fetched.bytes.len() > MAX_IMPORT_BYTES {
                return Err(ToolError::InvalidArgs(format!(
                    "`{}` is {} bytes; the canvas holds at most {MAX_IMPORT_BYTES}. \
                     Work on it with `run_in_sandbox` instead (it can stage the \
                     attachment into /work by id).",
                    att.filename,
                    fetched.bytes.len()
                )));
            }
            let content = String::from_utf8(fetched.bytes).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "`{}` is not valid UTF-8 text ({e}), so it can't become a \
                     canvas document",
                    att.filename
                ))
            })?;

            let title = args
                .title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or(&att.filename)
                .to_string();
            let id = documents::new_id();
            documents::create(
                &ctx.db,
                &id,
                session_id,
                &ctx.user_id,
                &title,
                format,
                &content,
                ctx.assistant_turn_id.as_deref(),
            )
            .await
            .map_err(|e| ToolError::Failed(format!("creating canvas document: {e}")))?;

            Ok(json!({
                "document_id": id,
                "title": title,
                "format": format.as_str(),
                "version": 1,
                "bytes": content.len(),
                "source_id": att.id,
                "note": format!(
                    "`{}` is now canvas document `{id}` (v1) — it appears in the \
                     user's document panel, where they can read and hand-edit it. \
                     Change it with `edit_document` ({edit}), read it with \
                     `read_document`, stage it into a sandbox run with \
                     `documents: [\"{id}\"]`, or hand it back as a file with \
                     `offer_download`. Do NOT rewrite the whole content to change \
                     part of it, and do not re-import it — the document is now the \
                     live copy.",
                    att.filename,
                    edit = match format.edit_kind() {
                        documents::EditKind::Structured => "RFC 6902 JSON Patch",
                        documents::EditKind::Text => "anchored find/replace",
                    },
                ),
            }))
        })
    }
}

/// Best-effort format for an imported file: extension first (it carries the
/// author's intent — a `.typ` served as `text/plain` is still Typst), mime as
/// the fallback. `None` means "not text the canvas can hold", which is the
/// signal to refuse rather than to store bytes as a lossy string.
fn infer_format(filename: &str, mime: &str) -> Option<DocumentFormat> {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // `DocumentFormat::parse` already understands `md`, `txt`, `yml`, `typ`.
    if let Some(f) = DocumentFormat::parse(&ext) {
        return Some(f);
    }
    match ext.as_str() {
        // Not canvas formats of their own, but plain text a human edits — and
        // the extension survives in the title, so nothing is lost.
        "csv" | "tsv" | "log" | "rs" | "py" | "sh" | "sql" | "js" | "ts" | "css" | "conf"
        | "ini" | "env" | "tex" | "rst" | "org" => Some(DocumentFormat::Text),
        _ => match mime {
            "text/markdown" => Some(DocumentFormat::Markdown),
            "application/json" => Some(DocumentFormat::Json),
            "application/toml" => Some(DocumentFormat::Toml),
            "application/yaml" | "application/x-yaml" => Some(DocumentFormat::Yaml),
            "text/html" => Some(DocumentFormat::Html),
            m if m.starts_with("text/") => Some(DocumentFormat::Text),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names_match_id() {
        assert_eq!(ImportFile.id(), ImportFile.schema().function.name);
    }

    #[test]
    fn format_follows_the_extension_then_the_mime() {
        // Extension wins: a Typst source served as plain text is still Typst,
        // and getting this wrong would put the wrong edit vocabulary on it.
        assert_eq!(
            infer_format("deck.typ", "text/plain"),
            Some(DocumentFormat::Typst)
        );
        assert_eq!(
            infer_format("data.json", "application/octet-stream"),
            Some(DocumentFormat::Json)
        );
        assert_eq!(
            infer_format("notes.md", "text/plain"),
            Some(DocumentFormat::Markdown)
        );
        // No useful extension → mime decides.
        assert_eq!(
            infer_format("dump", "application/json"),
            Some(DocumentFormat::Json)
        );
        assert_eq!(
            infer_format("readme", "text/plain"),
            Some(DocumentFormat::Text)
        );
        // Text-ish code/data without a canvas format of its own is `text`.
        assert_eq!(
            infer_format("rows.csv", "text/csv"),
            Some(DocumentFormat::Text)
        );
        // Binary: refused, so the caller can explain that images and PDFs
        // stay attachments instead of landing as mojibake.
        assert_eq!(infer_format("logo.png", "image/png"), None);
        assert_eq!(infer_format("report.pdf", "application/pdf"), None);
    }

    #[tokio::test]
    async fn errors_cleanly_without_a_session() {
        let pool = gateway_core::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let ctx = ToolContext {
            user_id: "u1".into(),
            roles: vec![],
            pool_access: gateway_core::server::upstreams::PoolAccess::all(),
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
            ocr: None,
            sandbox_lease: None,
            browser_lease: None,
            crypto: None,
            push: None,
            model: None,
        };
        let err = ImportFile
            .run(ctx, json!({"id": "t1/deck.typ"}))
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("chat session"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
