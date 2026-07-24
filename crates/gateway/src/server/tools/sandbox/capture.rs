// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

#[derive(Deserialize)]
pub(crate) struct CaptureArgs {
    url: String,
    #[serde(default = "default_capture_output")]
    output: CaptureOutput,
    #[serde(default)]
    filename: Option<String>,
}

pub(crate) fn default_capture_output() -> CaptureOutput {
    CaptureOutput::Png
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CaptureOutput {
    Png,
    Pdf,
    Text,
}

impl CaptureOutput {
    fn ext(self) -> &'static str {
        match self {
            CaptureOutput::Png => "png",
            CaptureOutput::Pdf => "pdf",
            CaptureOutput::Text => "txt",
        }
    }
    fn mode(self) -> &'static str {
        match self {
            CaptureOutput::Png => "png",
            CaptureOutput::Pdf => "pdf",
            CaptureOutput::Text => "text",
        }
    }
}

pub struct CaptureWebpage(pub Arc<SandboxClient>);

impl Tool for CaptureWebpage {
    fn id(&self) -> &str {
        "capture_webpage"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Load a web page in a headless browser and capture it as a \
             full-page PNG screenshot, a PDF, or extracted text. Requires the \
             operator to have enabled sandbox network egress. Returns the \
             capture as a file.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["url"],
                "properties": {
                    "url": {"type": "string", "description": "The http(s) URL to load."},
                    "output": {
                        "type": "string", "enum": ["png", "pdf", "text"],
                        "description": "What to capture. Default png."
                    },
                    "filename": {"type": "string", "description": "Optional output filename."}
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: CaptureArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{url, output?, filename?}}: {e}"))
            })?;
            let url = args.url.trim();
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(ToolError::InvalidArgs("url must be http(s)".into()));
            }
            let ext = args.output.ext();
            let stem = filename_stem(args.filename.as_deref(), "capture");
            let out = format!("{stem}.{ext}");
            // The URL rides in as a file (read at runtime) so it's never
            // interpolated into the script; only the validated mode/filename
            // are templated.
            let code = format!(
                "import pathlib\n\
                 from playwright.sync_api import sync_playwright\n\
                 url = pathlib.Path('url.txt').read_text().strip()\n\
                 out = {out:?}\n\
                 with sync_playwright() as p:\n\
                 \x20   b = p.chromium.launch(args=['--no-sandbox'])\n\
                 \x20   pg = b.new_page()\n\
                 \x20   pg.goto(url, wait_until='networkidle', timeout=30000)\n\
                 \x20   mode = {mode:?}\n\
                 \x20   if mode == 'png':\n\
                 \x20       pg.screenshot(path=out, full_page=True)\n\
                 \x20   elif mode == 'pdf':\n\
                 \x20       pg.pdf(path=out)\n\
                 \x20   else:\n\
                 \x20       pathlib.Path(out).write_text(pg.inner_text('body'))\n\
                 \x20   b.close()\n",
                out = out,
                mode = args.output.mode(),
            );
            let req = RunRequest {
                language: Language::Python,
                code,
                files: vec![InputFile {
                    name: "url.txt".into(),
                    content_b64: b64::encode(url.as_bytes()),
                }],
                timeout_secs: None,
                network: true,
                container_id: None,
                keep_alive: false,
            };
            self.0.execute(&ctx, req).await
        })
    }
}

// ---------------------------------------------------------------------------
// Document presets over uploaded files: convert_document + edit_presentation.
//
// Both resolve a single uploaded attachment (the round's file by default,
// or one named by id), fetch it server-side, stage it into `/work`, and
// run a fixed recipe in the sandbox — the file-in / file-out cousins of
// `generate_document`. The generic escape hatch is `run_in_sandbox`.

/// A safe `/work` filename stem from an attachment's name: keep only
/// `[A-Za-z0-9_-]`, drop the extension, fall back to `document`. Used so
/// staged input + produced output names can be interpolated into the
/// recipe without any shell-meta risk.
pub(crate) fn safe_stem(filename: &str) -> String {
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(filename);
    let s: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim_matches('_').to_string();
    if s.is_empty() {
        "document".to_string()
    } else {
        s
    }
}

/// Lowercase alphanumeric extension of `filename`, if it has a clean one.
pub(crate) fn safe_ext(filename: &str) -> Option<String> {
    filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Does this attachment look like a PowerPoint deck?
pub(crate) fn is_pptx(a: &AttachmentRef) -> bool {
    a.filename.to_ascii_lowercase().ends_with(".pptx")
        || a.mime.contains("presentation")
        || a.mime.contains("powerpoint")
}

/// Resolve exactly one uploaded attachment for a preset, then fetch its
/// bytes. Picks the round's file when `explicit_id` is `None` (erroring
/// clearly on none / several so the model chooses), validates a
/// model-named id against the session, and enforces `want` (the file
/// kind the tool handles). Chat-path only — needs a session + storage.
pub(crate) async fn resolve_one_attachment(
    ctx: &ToolContext,
    explicit_id: Option<&str>,
    kind: &str,
    want: impl Fn(&AttachmentRef) -> bool,
) -> Result<(AttachmentRef, Vec<u8>), ToolError> {
    let (Some(session_id), Some(s3)) = (ctx.session_id.as_deref(), ctx.s3.as_ref()) else {
        return Err(ToolError::Failed(
            "this tool works on uploaded chat files and needs the chat path with attachment \
             storage configured ([chat.s3]); it isn't available here."
                .into(),
        ));
    };
    let (session_atts, round) =
        chat_attachments::session_and_round_attachments(&ctx.db, session_id)
            .await
            .map_err(|e| ToolError::Failed(format!("listing session attachments: {e}")))?;

    let chosen: AttachmentRef = match explicit_id {
        Some(id) => {
            // Exact id or bare filename (newest match wins) — same loose
            // resolution as `stage_attachments`, so reuse never depends on
            // the model remembering a turn id.
            let a = chat_attachments::resolve_attachment(&session_atts, id)
                .ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "no attachment with id or filename `{id}` in this conversation"
                    ))
                })?
                .clone();
            if !want(&a) {
                return Err(ToolError::InvalidArgs(format!(
                    "attachment `{}` is not a {kind}",
                    a.filename
                )));
            }
            a
        }
        None => {
            let mut candidates: Vec<AttachmentRef> = round.into_iter().filter(&want).collect();
            match candidates.len() {
                1 => candidates.pop().unwrap(),
                0 => {
                    let avail: Vec<&str> = session_atts
                        .iter()
                        .filter(|a| want(a))
                        .map(|a| a.id.as_str())
                        .collect();
                    let hint = if avail.is_empty() {
                        String::new()
                    } else {
                        format!(" Earlier {kind}s you can pass as attachment_id: {avail:?}.")
                    };
                    return Err(ToolError::InvalidArgs(format!(
                        "no {kind} was uploaded in this message; upload one or pass \
                         attachment_id.{hint}"
                    )));
                }
                _ => {
                    let ids: Vec<&str> = candidates.iter().map(|a| a.id.as_str()).collect();
                    return Err(ToolError::InvalidArgs(format!(
                        "several {kind}s were uploaded this message — pass attachment_id to \
                         choose one of: {ids:?}"
                    )));
                }
            }
        }
    };

    let (turn, filename) = chosen
        .id
        .split_once('/')
        .ok_or_else(|| ToolError::Failed(format!("malformed attachment id `{}`", chosen.id)))?;
    let fetched = chat_attachments::fetch(s3, turn, filename)
        .await
        .map_err(|e| ToolError::Failed(format!("fetching `{}`: {e}", chosen.id)))?;
    Ok((chosen, fetched.bytes))
}

// ---------------------------------------------------------------------------
// convert_document — uploaded office/pdf file -> pdf/docx/txt/html/images
