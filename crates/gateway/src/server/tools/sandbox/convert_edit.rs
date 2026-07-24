// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

#[derive(Deserialize)]
pub(crate) struct ConvertArgs {
    #[serde(default)]
    attachment_id: Option<String>,
    target: ConvertTarget,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ConvertTarget {
    Pdf,
    Docx,
    Txt,
    Html,
    /// One PNG per page/slide (via pdf, rendered at 150 dpi).
    Images,
}

impl ConvertTarget {
    fn as_str(self) -> &'static str {
        match self {
            ConvertTarget::Pdf => "pdf",
            ConvertTarget::Docx => "docx",
            ConvertTarget::Txt => "txt",
            ConvertTarget::Html => "html",
            ConvertTarget::Images => "images",
        }
    }
}

pub struct ConvertDocument(pub Arc<SandboxClient>);

impl ConvertDocument {
    /// The sandbox recipe. `stem`/`ext` are pre-sanitized (safe charset),
    /// so interpolating them carries no shell-meta risk.
    pub(crate) fn script(target: ConvertTarget, stem: &str, ext: &str) -> String {
        let infile = format!("{stem}.{ext}");
        match target {
            ConvertTarget::Images => format!(
                "set -e\n\
                 soffice --headless --convert-to pdf --outdir . {infile}\n\
                 pdftoppm -png -r 150 {stem}.pdf {stem}-slide\n\
                 rm -f {stem}.pdf\n"
            ),
            other => {
                let t = other.as_str();
                format!("set -e\nsoffice --headless --convert-to {t} --outdir . {infile}\n")
            }
        }
    }
}

impl Tool for ConvertDocument {
    fn id(&self) -> &str {
        "convert_document"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Convert a file the user uploaded (PowerPoint, Word, Excel, \
             ODF, PDF, …) to another format and return the result. Targets: \
             `pdf`, `docx`, `txt`, `html`, or `images` (one PNG per \
             page/slide). By default it converts the file uploaded in the \
             current message; pass `attachment_id` (from an attachment stub) \
             to convert a file from earlier in the conversation. Conversion \
             runs through LibreOffice. For edits or anything custom, use \
             `edit_presentation` or `run_in_sandbox`.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["target"],
                "properties": {
                    "attachment_id": {
                        "type": "string",
                        "description": "Optional id (`<turn>/<file>`) or filename of the file \
                                        to convert (newest match wins). Defaults to the file \
                                        uploaded in the current message."
                    },
                    "target": {
                        "type": "string",
                        "enum": ["pdf", "docx", "txt", "html", "images"],
                        "description": "Output format. `images` returns one PNG per page/slide."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: ConvertArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{attachment_id?, target}}: {e}"))
            })?;
            let (att, bytes) =
                resolve_one_attachment(&ctx, args.attachment_id.as_deref(), "file", |_| true)
                    .await?;
            let stem = safe_stem(&att.filename);
            let ext = safe_ext(&att.filename).ok_or_else(|| {
                ToolError::InvalidArgs(format!(
                    "`{}` has no file extension, so its type can't be determined",
                    att.filename
                ))
            })?;
            let infile = format!("{stem}.{ext}");
            let code = Self::script(args.target, &stem, &ext);
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
            let mut out = self.0.execute(&ctx, req).await?;
            if let Some(obj) = out.as_object_mut() {
                obj.insert(
                    "converted".into(),
                    json!({"id": att.id, "target": args.target.as_str()}),
                );
            }
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------------------
// edit_presentation — run python-pptx against an uploaded .pptx

#[derive(Deserialize)]
pub(crate) struct EditPptxArgs {
    #[serde(default)]
    attachment_id: Option<String>,
    code: String,
}

pub struct EditPresentation(pub Arc<SandboxClient>);

impl Tool for EditPresentation {
    fn id(&self) -> &str {
        "edit_presentation"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Modify a PowerPoint (.pptx) the user uploaded, using python-pptx. \
             Your `code` runs in the sandbox with the deck already saved as \
             `input.pptx` in the working directory; load it, make your \
             changes, and save the result as `output.pptx` — it's returned to \
             the user. By default it edits the .pptx uploaded in the current \
             message; pass `attachment_id` to edit one from earlier in the \
             conversation. Example: `from pptx import Presentation; p = \
             Presentation('input.pptx'); p.slides[0].shapes.title.text = 'Hi'; \
             p.save('output.pptx')`. For other file types or free-form work, \
             use `run_in_sandbox`.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["code"],
                "properties": {
                    "attachment_id": {
                        "type": "string",
                        "description": "Optional id (`<turn>/<file>`) or filename of the \
                                        .pptx to edit (newest match wins). Defaults to the \
                                        deck uploaded in the current message."
                    },
                    "code": {
                        "type": "string",
                        "description": "Python (python-pptx) that reads `input.pptx` and writes \
                                        `output.pptx` in the working directory."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: EditPptxArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{attachment_id?, code}}: {e}"))
            })?;
            if args.code.trim().is_empty() {
                return Err(ToolError::InvalidArgs("code must be non-empty".into()));
            }
            let (att, bytes) = resolve_one_attachment(
                &ctx,
                args.attachment_id.as_deref(),
                "PowerPoint (.pptx) file",
                is_pptx,
            )
            .await?;
            // The deck rides in as a fixed-name binary input; the model's
            // code (which references `input.pptx`) is the program.
            let req = RunRequest {
                language: Language::Python,
                code: args.code,
                files: vec![InputFile {
                    name: "input.pptx".into(),
                    content_b64: b64::encode(&bytes),
                }],
                timeout_secs: None,
                network: false,
                container_id: None,
                keep_alive: false,
            };
            let mut out = self.0.execute(&ctx, req).await?;
            if let Some(obj) = out.as_object_mut() {
                obj.insert("edited".into(), json!({"id": att.id}));
            }
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------------------
// render_excalidraw — Excalidraw scene -> svg/png/pdf via excalirender
//
// Two sources, one recipe: the model can pass a `.excalidraw` scene it
// authored inline (Excalidraw's JSON is well within an LLM's reach), or
// point at an `.excalidraw`/`.json` file the user uploaded. Either way the
// bytes land in `/work/diagram.excalidraw` and excalirender (a self-contained
// binary baked into the image, no network/fonts needed) renders them. SVG is
// the default — vector output stays crisp on slides and can be embedded into
// a Typst document with `render_typst`.
