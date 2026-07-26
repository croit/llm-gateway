// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

#[derive(Deserialize)]
pub(crate) struct DocArgs {
    markdown: String,
    format: DocFormat,
    #[serde(default)]
    filename: Option<String>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DocFormat {
    Pdf,
    Docx,
    Pptx,
}

impl DocFormat {
    fn ext(self) -> &'static str {
        match self {
            DocFormat::Pdf => "pdf",
            DocFormat::Docx => "docx",
            DocFormat::Pptx => "pptx",
        }
    }
}

pub struct GenerateDocument(pub Arc<SandboxClient>);

impl Tool for GenerateDocument {
    fn id(&self) -> &str {
        "generate_document"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Turn Markdown into a finished PDF, Word (.docx), or PowerPoint \
             (.pptx) document and return it to the user. Write normal Markdown; \
             for slides, separate them with `---`. This is the easy path for \
             ONE-OFF document generation — no code required. If the user is \
             likely to iterate on the wording, draft the document in the \
             canvas instead (`create_document`, then `export_document` when \
             it's final): the user sees it live and you can edit single \
             passages across turns. For anything Markdown can't \
             express, use `run_in_sandbox` directly.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["markdown", "format"],
                "properties": {
                    "markdown": {"type": "string", "description": "Document content as Markdown."},
                    "format": {
                        "type": "string", "enum": ["pdf", "docx", "pptx"],
                        "description": "Output format."
                    },
                    "filename": {
                        "type": "string",
                        "description": "Optional output filename (extension is set from `format`)."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: DocArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{markdown, format, filename?}}: {e}"))
            })?;
            if args.markdown.trim().is_empty() {
                return Err(ToolError::InvalidArgs("markdown must be non-empty".into()));
            }
            let ext = args.format.ext();
            let stem = filename_stem(args.filename.as_deref(), "document");
            let out = format!("{stem}.{ext}");
            // The markdown rides in as an input file (never interpolated into
            // the command), so its content can't break out into the shell.
            let pdf_engine = if matches!(args.format, DocFormat::Pdf) {
                " --pdf-engine=weasyprint"
            } else {
                ""
            };
            let code = format!("set -e\npandoc input.md -o {out:?}{pdf_engine}\n");
            let req = RunRequest {
                language: Language::Bash,
                code,
                files: vec![InputFile {
                    name: "input.md".into(),
                    content_b64: b64::encode(args.markdown.as_bytes()),
                }],
                timeout_secs: None,
                network: false,
                container_id: None,
                keep_alive: false,
            };
            self.0.execute(&ctx, req).await
        })
    }
}

// ---------------------------------------------------------------------------
// Wrapper: export_document — render a canvas document to a downloadable file

#[derive(Deserialize)]
pub(crate) struct ExportArgs {
    document_id: String,
    format: DocFormat,
    #[serde(default)]
    filename: Option<String>,
}

/// Bridge the document canvas to the sandbox's pandoc path: take a
/// document the model built with `create_document`/`edit_document` and
/// render its current content to a downloadable PDF/DOCX/PPTX. Reuses the
/// exact `generate_document` recipe — the only difference is the Markdown
/// comes from the `documents` store rather than the tool args.
pub struct ExportDocument(pub Arc<SandboxClient>);

impl Tool for ExportDocument {
    fn id(&self) -> &str {
        "export_document"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Export a document from the canvas (one you created with \
             `create_document`) to a finished PDF, Word (.docx), or PowerPoint \
             (.pptx) file and attach it for the user to download. Give the \
             `document_id` and a `format`. Works on markdown/text documents \
             (via pandoc) and `typst` documents (pdf via `typst compile`, \
             docx via pandoc's typst reader; pptx is not supported for typst \
             — use run_in_sandbox with typ2pptx for an editable deck). For \
             one-off Markdown you haven't put in the canvas, use \
             `generate_document` instead.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["document_id", "format"],
                "properties": {
                    "document_id": {"type": "string", "description": "The id from `create_document`."},
                    "format": {
                        "type": "string", "enum": ["pdf", "docx", "pptx"],
                        "description": "Output format."
                    },
                    "filename": {
                        "type": "string",
                        "description": "Optional output filename (extension is set from `format`)."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            use gateway_core::server::db::documents::{self, DocumentFormat};
            let session_id = ctx.session_id.as_deref().ok_or_else(|| {
                ToolError::Failed("export_document is only available inside a chat session".into())
            })?;
            let args: ExportArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{document_id, format, filename?}}: {e}"))
            })?;
            let (doc, ver) = documents::get_version(&ctx.db, session_id, &args.document_id, None)
                .await
                .map_err(|e| ToolError::Failed(format!("reading document: {e}")))?
                .ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "no document `{}` in this conversation",
                        args.document_id
                    ))
                })?;
            let ext = args.format.ext();
            let stem = filename_stem(args.filename.as_deref(), "document");
            let out = format!("{stem}.{ext}");
            // Per-format recipe. Markdown/text go through pandoc; typst
            // compiles natively (pdf) or rides pandoc's typst reader (docx).
            // Structured/HTML docs would come out garbled — reject those.
            let (code, input_name) = match doc.format {
                DocumentFormat::Markdown | DocumentFormat::Text => {
                    let pdf_engine = if matches!(args.format, DocFormat::Pdf) {
                        " --pdf-engine=weasyprint"
                    } else {
                        ""
                    };
                    (
                        format!("set -e\npandoc input.md -o {out:?}{pdf_engine}\n"),
                        "input.md",
                    )
                }
                DocumentFormat::Typst => match args.format {
                    DocFormat::Pdf => (
                        format!("set -e\ntypst compile input.typ {out:?}\n"),
                        "input.typ",
                    ),
                    DocFormat::Docx => (
                        format!("set -e\npandoc -f typst input.typ -o {out:?}\n"),
                        "input.typ",
                    ),
                    DocFormat::Pptx => {
                        return Err(ToolError::InvalidArgs(
                            "pptx export of a typst document isn't supported here — use \
                             run_in_sandbox with typ2pptx for an editable deck"
                                .into(),
                        ));
                    }
                },
                other => {
                    return Err(ToolError::InvalidArgs(format!(
                        "only markdown, text, or typst documents can be exported; `{}` is {}",
                        args.document_id,
                        other.as_str()
                    )));
                }
            };
            let req = RunRequest {
                language: Language::Bash,
                code,
                files: vec![InputFile {
                    name: input_name.into(),
                    content_b64: b64::encode(ver.content.as_bytes()),
                }],
                timeout_secs: None,
                network: false,
                container_id: None,
                keep_alive: false,
            };
            self.0.execute(&ctx, req).await
        })
    }
}

// ---------------------------------------------------------------------------
// Wrapper: capture_webpage (headless chromium)
