// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-template `typst_<id>` tool wrapper.
//!
//! Two tools per discovered template:
//!
//! - [`TypstRenderTool`] (`typst_<id>`) — the manifest's declared
//!   fields become its JSON schema; on invocation we hand the validated
//!   key/value pairs to [`gateway_core::server::typst::compile`], then splice
//!   the `.pdf` (deliverable) + a `.png` page-1 preview into the
//!   assistant's `content`. The preview clicks through to the PDF. The
//!   field values are also uploaded as a hidden `.json` (no chat chip)
//!   that backs the edit tool below.
//! - [`TypstEditTool`] (`typst_<id>_edit`) — re-renders after applying
//!   a small RFC 6902 JSON Patch to that stored `.json`, so the model
//!   can fix one headline without resending the whole input.
//!
//! The static template `.typ` is intentionally NOT attached: it can't
//! be edited through these tools and can't recompile without its
//! fonts/assets, so it only adds clutter.
//!
//! These are registered dynamically at startup (see `main.rs`); the
//! tool ids are leaked into `'static str`s so the `Tool` trait's
//! signature is happy. Tools registered once at startup never get
//! dropped, so the leak is bounded.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use session_core::db as chat;
use shared::api::ToolDef;
use shared::sandbox::{InputFile, Language, RunRequest};

use gateway_core::server::chat_attachments;
use gateway_core::server::db::documents::{self, DocumentFormat};
use gateway_core::server::tools::sandbox::{SandboxClient, b64};
use gateway_core::server::tools::{Tool, ToolContext, ToolError, ToolFuture, ToolResult};
use gateway_core::server::typst::{
    self, DefaultSource, DocxExport, FieldType, PptxExport, Template,
};

pub struct TypstRenderTool {
    template: Arc<Template>,
    /// Sandbox client for the optional editable-PowerPoint export
    /// (typ2pptx). `None` when the deployment has no `[sandbox]`
    /// configured — the render still produces the PDF + preview, just
    /// no `.pptx`.
    sandbox: Option<Arc<SandboxClient>>,
    /// Leaked `Box<str>` so the trait's `&'static str` return is
    /// satisfied for runtime-constructed tools. The Tool lives for
    /// the whole process; the leak is single-allocation-per-template
    /// at startup.
    id: &'static str,
}

impl TypstRenderTool {
    pub fn new(template: Arc<Template>, sandbox: Option<Arc<SandboxClient>>) -> Self {
        let id: &'static str = Box::leak(format!("typst_{}", template.id).into_boxed_str());
        Self {
            template,
            sandbox,
            id,
        }
    }
}

impl Tool for TypstRenderTool {
    fn id(&self) -> &str {
        self.id
    }

    /// A render is fast (PDF+PNG on the host, ~seconds) UNLESS the template
    /// runs a sandbox export (pptx/docx via typ2pptx/pdf2docx), which can take
    /// tens of seconds. Without this the 30 s default `TOOL_TIMEOUT` kills the
    /// whole call mid-export — discarding the PDF that already rendered. Size
    /// to the sandbox's own ceiling (+ margin for the host render) so the slow
    /// export finishes, or fails cleanly on its own, before the runner gives up.
    fn max_duration(&self) -> Option<std::time::Duration> {
        self.sandbox
            .as_ref()
            .map(|s| s.loop_timeout() + std::time::Duration::from_secs(30))
    }

    fn schema(&self) -> ToolDef {
        let t = &self.template;
        let mut props = Map::new();
        for f in &t.fields {
            props.insert(
                f.name.clone(),
                json!({
                    "type": f.ty.json_schema_name(),
                    "description": f.description,
                }),
            );
        }
        // Alternative data source: a canvas JSON document. `required` is left
        // empty at the schema level because the input is either-or (inline
        // fields OR `document_id`); the runtime enforces exactly one path
        // (`stringify_args` rejects a missing required field on the inline
        // path). Each field's own description still says whether it's required.
        props.insert(
            "document_id".to_string(),
            json!({
                "type": "string",
                "description": "Optional. Render from a saved canvas document \
                    instead of inline fields. Pass the `document_id` of a JSON \
                    document (create it with `create_document`, format \"json\") \
                    holding THIS template's field map — for a deck the slides \
                    live under the `deck` key, exactly the shape this template's \
                    `_read` tool shows. When set, inline field args are ignored \
                    and the document is the single source of truth: it appears \
                    in the user's document panel, you change it with \
                    `edit_document` (JSON Patch — same vocabulary as `_edit`), \
                    and you re-render with the SAME document_id to pick up the \
                    edits. Use this for a deck/letter you'll iterate on; omit it \
                    for a one-shot render.",
            }),
        );
        props.insert(
            "version".to_string(),
            json!({
                "type": "integer",
                "minimum": 1,
                "description": "Optional. With `document_id`, render a specific \
                    document version instead of the latest.",
            }),
        );
        // A non-field control: which page to show as the inline preview.
        props.insert(
            "preview_page".to_string(),
            json!({
                "type": "integer",
                "minimum": 1,
                "description": "Optional. Which PDF page (1-based) to render as \
                                the inline PNG preview. Defaults to 1 (the cover/\
                                first page). For a multi-page document or deck, set \
                                this to preview the page you actually changed rather \
                                than always the cover.",
            }),
        );
        let mut properties = Value::Object(props);
        // Stable iteration order so generated schemas are reproducible
        // across rebuilds — useful for caching the tool list upstream.
        if let Value::Object(map) = &mut properties {
            let sorted: Map<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .collect();
            *map = sorted;
        }
        ToolDef::function(
            self.id(),
            format!(
                "{descr} Splices two things into your reply: `{base}.pdf` \
                 (the final document — the deliverable) and a PNG preview of \
                 one page you can visually inspect (clicking it opens the \
                 PDF; defaults to the first page, override with \
                 `preview_page`). The tool result also returns a `data_id` \
                 referencing the exact field values you supplied. To change \
                 something afterwards, do NOT resend the whole input and do \
                 NOT re-render repeatedly to hunt for what to change: call \
                 `{id}_read` with the `data_id` to see the stored content and \
                 locate the exact text/field, then `{id}_edit` with that \
                 `data_id` and either a small JSON Patch or a find/replace.",
                descr = t.description,
                base = t.output_basename,
                id = self.id(),
            ),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": properties,
                "required": [],
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        let template = self.template.clone();
        Box::pin(async move {
            // `assistant_turn_id` gates this tool the same way
            // upload_attachment does — the renders only make sense
            // inside a chat session where we can attach them.
            let turn_id = ctx.assistant_turn_id.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "typst tools are only available inside a chat session \
                     (no assistant turn to attach the rendered PDF/PNG to)"
                        .into(),
                )
            })?;
            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "chat attachments are not configured on this gateway \
                     (operator must set [chat.s3]); typst output has nowhere \
                     to land"
                        .into(),
                )
            })?;

            // Fill any `default_from` field the model left out (or blank)
            // with the signed-in user's identity — so e.g. the letter's
            // sender name/email is guaranteed from the token rather than
            // riding on the model remembering to copy it. One DB read, and
            // only when a defaultable field is actually missing.
            let mut arg_map = match args {
                Value::Object(m) => m,
                _ => {
                    return Err(ToolError::InvalidArgs(
                        "expected a JSON object of field values".into(),
                    ));
                }
            };
            // Pull the non-field `preview_page` control out before field
            // validation (which rejects anything not declared in the
            // manifest).
            let preview_page = take_preview_page(&mut arg_map)?;

            // Canvas-document source. Pull these control keys out before field
            // validation (they aren't template fields). When `document_id` is
            // given the document IS the data — inline fields are ignored, and
            // the document (not a hidden edit-base) is the single edit surface.
            let document_id = arg_map
                .remove("document_id")
                .and_then(|v| v.as_str().map(str::to_owned));
            let doc_version = arg_map.remove("version").and_then(|v| v.as_i64());
            if let Some(doc_id) = document_id {
                let session_id = ctx.session_id.as_deref().ok_or_else(|| {
                    ToolError::Failed(
                        "canvas documents are only available inside a chat session".into(),
                    )
                })?;
                let (doc, ver) = documents::get_version(&ctx.db, session_id, &doc_id, doc_version)
                    .await
                    .map_err(|e| ToolError::Failed(format!("reading canvas document: {e}")))?
                    .ok_or_else(|| {
                        ToolError::InvalidArgs(format!(
                            "no canvas document `{doc_id}` in this conversation — create it \
                                 with create_document (format \"json\"), or check list_documents"
                        ))
                    })?;
                if doc.format != DocumentFormat::Json {
                    return Err(ToolError::InvalidArgs(format!(
                        "canvas document `{doc_id}` is `{}` — a typst render needs a JSON document \
                         holding the field map (a deck's slides under the `deck` key)",
                        doc.format.as_str()
                    )));
                }
                let data: Value = serde_json::from_str(&ver.content).map_err(|e| {
                    ToolError::InvalidArgs(format!(
                        "canvas document `{doc_id}` is not valid JSON: {e}"
                    ))
                })?;
                let inputs = inputs_from_data(&template, &data)?;
                let mut result = render_and_attach(
                    &ctx,
                    turn_id,
                    s3,
                    &template,
                    inputs,
                    &data,
                    self.sandbox.as_ref(),
                    preview_page,
                )
                .await?;
                // The canvas document is the source of truth, so drop the
                // hidden edit-base pointer and steer edits back to the document
                // (editing the stale base would diverge from the panel).
                if let Value::Object(m) = &mut result {
                    m.remove("data_id");
                    m.insert("canvas_document_id".into(), json!(doc_id));
                    m.insert("canvas_version".into(), json!(ver.version));
                    m.insert("note".into(), json!(format!(
                        "Rendered from canvas document `{doc_id}` (v{ver}), shown in the user's \
                         document panel. To change it, call edit_document on `{doc_id}` with a \
                         JSON Patch (same vocabulary as {id}_edit), then re-render with the same \
                         document_id — do NOT use the _read/_edit data_id path for this render.",
                        ver = ver.version,
                        id = self.id(),
                    )));
                }
                return Ok(result);
            }

            if wants_identity(&template, &arg_map) {
                match gateway_core::server::db::users::find_by_id(&ctx.db, &ctx.user_id).await {
                    Ok(Some(u)) => apply_identity_defaults(
                        &template,
                        &mut arg_map,
                        &Identity {
                            name: u.name,
                            email: Some(u.email),
                        },
                    ),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(
                        error = %e,
                        "typst default_from: user lookup failed; leaving fields to the model"
                    ),
                }
            }
            let args = Value::Object(arg_map);

            // Strict validation of the model's inputs (unknown / missing /
            // wrong-typed fields). The data file stores the embedded form
            // (`deck` as nested JSON, not an escaped string) so it doubles
            // as the editable base for `_edit`.
            let inputs = stringify_args(&template, &args)?;
            let data = data_value(&args);
            render_and_attach(
                &ctx,
                turn_id,
                s3,
                &template,
                inputs,
                &data,
                self.sandbox.as_ref(),
                preview_page,
            )
            .await
        })
    }
}

/// Render `inputs` through `template` and attach the result to the
/// current turn: the PDF (deliverable) + a page-1 PNG preview that
/// clicks through to the PDF. The full input `data` is uploaded
/// alongside as a hidden `{base}.json` (no chat chip) so a later
/// `_edit` call can fetch it as the patch base; its id comes back in
/// the result. Shared by the render and edit tools.
#[allow(clippy::too_many_arguments)]
async fn render_and_attach(
    ctx: &ToolContext,
    turn_id: &str,
    s3: &gateway_core::server::config::S3Config,
    template: &Template,
    inputs: Vec<(String, String)>,
    data: &Value,
    sandbox: Option<&Arc<SandboxClient>>,
    preview_page: u32,
) -> ToolResult {
    // Resolve any `att:` image refs the model dropped into image fields
    // (uploaded / extracted images): fetch the bytes, stage them under
    // `uploads/`, and rewrite each ref to that path. The ORIGINAL `data` (refs
    // intact) is what we persist as the `.json` edit-base below, so a later
    // `_edit` / `_pptx` re-stages from scratch — staged files are ephemeral,
    // scoped to this one render.
    let (staged, reps) = stage_att_refs(s3, data).await?;
    let inputs: Vec<(String, String)> = inputs
        .into_iter()
        .map(|(k, v)| (k, apply_replacements(&v, &reps)))
        .collect();
    let render_data = apply_reps_to_value(data, &reps)?;

    // With staged images, compile against a temp root = a copy of the template
    // dir + the staged files, so `image("uploads/…")` resolves alongside the
    // shipped `assets/`. No staging → compile straight against the template
    // (no copy). The tempdir must outlive the compile, hence the binding.
    let staging = if staged.is_empty() {
        None
    } else {
        Some(build_staging_root(&template.root, &staged)?)
    };
    let staging_path = staging.as_ref().map(|d| d.path());

    // Compile. If it fails on the unescaped-`@` signature — Typst reads an
    // email/@-handle as a cross-reference and the WHOLE render crashes, by far
    // the most common failure and the one the tool descriptions warn hardest
    // about — escape the stray `@`s and recompile ONCE. On success we adopt the
    // escaped content for every downstream consumer (the `.json` edit base, the
    // pptx bundle, the chips) so a later `_edit` stays consistent, and note the
    // fix-up in the result so the model can spot the rare collateral (a `@` in a
    // raw, non-markup field picking up a stray backslash). Any other failure —
    // and a retry that still fails — surfaces the ORIGINAL stderr unchanged, so
    // the model sees the real problem and iterates. Compile errors map to
    // InvalidArgs (nudge the model to fix its input); other errors to Failed.
    let (rendered, render_data, edit_base_escaped, auto_escaped) = compile_with_at_fixup(
        template,
        &inputs,
        render_data,
        data,
        preview_page,
        staging_path,
    )
    .await?;

    // Same-turn dedup, race-safe across concurrent tool calls: a second
    // typst call (or a sibling `upload_attachment` claiming e.g.
    // `letter.png` mid-render) would otherwise overwrite this render's
    // objects in S3 and leave earlier markers pointing at the new bytes.
    // The reservation mutex serializes the pick across the `join_all` of
    // parallel tool calls, and the group is reserved as a unit so the
    // files stay in sync (chart.pdf+png+json → chart-2.pdf+png+json).
    let reservations = ctx.attachment_reservations.as_ref().ok_or_else(|| {
        ToolError::Failed(
            "typst tools require a per-turn attachment-reservation set, \
             which is only initialised on the chat-page path"
                .into(),
        )
    })?;
    let base = chat_attachments::reserve_basename(
        &ctx.db,
        turn_id,
        reservations,
        &template.output_basename,
        TYPST_EXTS,
    )
    .await
    .map_err(|e| ToolError::Failed(format!("reserve basename: {e}")))?;
    let pdf_name = format!("{base}.pdf");
    let png_name = format!("{base}.png");
    let json_name = format!("{base}.json");
    let pptx_name = format!("{base}.pptx");
    let docx_name = format!("{base}.docx");
    let odt_name = format!("{base}.odt");

    // Persist the escaped variant as the edit base when the auto-escape retry
    // fired, so a later `_edit`/`_read` sees exactly what rendered (and doesn't
    // re-crash on the same `@`); otherwise store the model's data untouched.
    let data_bytes = serialize_data(edit_base_escaped.as_ref().unwrap_or(data));

    let pdf_out = chat_attachments::upload(s3, turn_id, &pdf_name, "application/pdf", rendered.pdf)
        .await
        .map_err(|e| ToolError::Failed(format!("upload pdf: {e}")))?;
    let png_out = chat_attachments::upload(s3, turn_id, &png_name, "image/png", rendered.png)
        .await
        .map_err(|e| ToolError::Failed(format!("upload png: {e}")))?;
    // The data file backs the `_edit` patch base and lets the model
    // re-read its own input; it is NOT shown as a chat chip (the user
    // only wants the PDF + preview), so no marker is spliced for it.
    let json_out =
        chat_attachments::upload(s3, turn_id, &json_name, "application/json", data_bytes)
            .await
            .map_err(|e| ToolError::Failed(format!("upload data json: {e}")))?;

    // Optional editable-PowerPoint export, when the template opts in
    // (`[pptx]`) and a sandbox is configured. Best-effort: a sandbox
    // hiccup must NOT fail the render — the PDF + preview already
    // landed — so we log + note the error and still return success.
    let mut pptx_out = None;
    let mut pptx_error: Option<String> = None;
    if let (Some(cfg), Some(sandbox)) = (template.pptx.as_ref(), sandbox) {
        (pptx_out, pptx_error) = export_pptx(
            s3,
            turn_id,
            &pptx_name,
            sandbox,
            template,
            cfg,
            &render_data,
            &staged,
        )
        .await;
    }

    // Optional editable-Word export (`[docx]`): compile the template to HTML
    // and convert it to an editable .docx with pandoc + brand-font embedding.
    // Best-effort, same as pptx — the PDF/preview already landed, so a failure
    // only notes an error rather than failing the render.
    let mut docx_out = None;
    let mut odt_out = None;
    let mut docx_error: Option<String> = None;
    if let (Some(cfg), Some(sandbox)) = (template.docx.as_ref(), sandbox) {
        (docx_out, odt_out, docx_error) = export_docx(
            s3, turn_id, &docx_name, &odt_name, sandbox, template, cfg, &inputs, &staged,
        )
        .await;
    }

    // Visible markers in one chunk: the PDF chip, the PNG preview
    // (linking through to the PDF so clicking the inline image opens the
    // document, not a bigger copy of the preview), and — when produced —
    // the editable PPTX chip.
    let pdf_url = chat_attachments::proxy_url(turn_id, &pdf_name);
    let mut chunk = format!(
        "\n\n{}\n{}",
        chat_attachments::marker_line(turn_id, &pdf_out),
        chat_attachments::marker_line_linked(turn_id, &png_out, &pdf_url),
    );
    if let Some(p) = &pptx_out {
        chunk.push('\n');
        chunk.push_str(&chat_attachments::marker_line(turn_id, p));
    }
    if let Some(d) = &docx_out {
        chunk.push('\n');
        chunk.push_str(&chat_attachments::marker_line(turn_id, d));
    }
    if let Some(o) = &odt_out {
        chunk.push('\n');
        chunk.push_str(&chat_attachments::marker_line(turn_id, o));
    }
    chunk.push_str("\n\n");

    // Per-turn supersede: a chat turn should show only the LATEST render
    // of this template, not every intermediate variant produced while the
    // model iterates. So before splicing the new chips, strip this
    // template's earlier chips (pdf/png/pptx of the same basename family)
    // from the current turn's content. Previous *turns'* deliverables are
    // untouched — the conversation's version history stays intact — and
    // the S3 objects + the `.json` edit base are never deleted, so an
    // `_edit` chain still resolves. Done under the reservation mutex so
    // two concurrent same-turn renders can't lose each other's write.
    {
        let _guard = reservations.lock().await;
        let existing = chat::get_content(&ctx.db, turn_id)
            .await
            .map_err(|e| ToolError::Failed(format!("read turn content: {e}")))?
            .unwrap_or_default();
        let stripped = session_core::attachments::remove_markers_where(&existing, |a| {
            is_template_typst_chip(template, &a.filename)
        });
        let new_content = format!("{stripped}{chunk}");
        chat::set_content(&ctx.db, turn_id, &new_content)
            .await
            .map_err(|e| ToolError::Failed(format!("persist markers: {e}")))?;
    }

    let data_id = format!("{turn_id}/{}", json_out.filename);
    let mut result = json!({
        "template": template.id,
        "pdf": { "filename": pdf_out.filename, "size": pdf_out.bytes,
                 "id": format!("{turn_id}/{}", pdf_out.filename) },
        "preview_png": { "filename": png_out.filename, "size": png_out.bytes,
                         "id": format!("{turn_id}/{}", png_out.filename) },
        "data_id": data_id,
        "rendered": "The PDF and its page-1 preview are now inline in your \
                     reply (the preview links to the PDF) — do NOT repeat the \
                     marker text in your prose. Look at the PNG to verify the \
                     layout. To change one thing afterwards, call this \
                     template's `_edit` tool with base=<the data_id above> \
                     and a JSON Patch — don't resend the whole input.",
    });
    if let Some(p) = &pptx_out {
        result["pptx"] = json!({
            "filename": p.filename, "size": p.bytes,
            "id": format!("{turn_id}/{}", p.filename),
            "note": "Editable PowerPoint — import it into Google Slides for a \
                     native editable deck.",
        });
    } else if let Some(err) = pptx_error {
        result["pptx_error"] = json!(format!(
            "Editable .pptx export failed (PDF/preview are fine): {err}"
        ));
    }
    if let Some(d) = &docx_out {
        result["docx"] = json!({
            "filename": d.filename, "size": d.bytes,
            "id": format!("{turn_id}/{}", d.filename),
            "note": "Editable Word document (.docx) — same content as the PDF, \
                     openable and editable in Word.",
        });
    } else if let Some(err) = docx_error {
        result["docx_error"] = json!(format!(
            "Editable .docx export failed (PDF/preview are fine): {err}"
        ));
    }
    if auto_escaped > 0 {
        result["auto_escaped"] = json!(format!(
            "The render first failed because {auto_escaped} unescaped '@' \
             character(s) were read as Typst cross-references; I escaped them \
             (\\@) and re-rendered successfully. Check the preview: if a '@' now \
             shows a stray backslash (e.g. in a title or other non-markup \
             field), call this template's `_edit` tool to fix just that text. \
             Escape '@' yourself next time to avoid the retry.",
        ));
    }
    if let Some(o) = &odt_out {
        result["odt"] = json!({
            "filename": o.filename, "size": o.bytes,
            "id": format!("{turn_id}/{}", o.filename),
            "note": "Editable OpenDocument text (.odt) — the same content for \
                     LibreOffice / OpenOffice users.",
        });
    }
    Ok(result)
}

/// Compile `template` with `inputs`, with the one auto-recovery the tool
/// descriptions warn hardest about: if the compile fails because Typst read an
/// unescaped `@` (an email / @-handle) as a cross-reference — by far the most
/// common failure — escape the stray `@`s in the data and recompile ONCE. On
/// that recovery the escaped variant becomes the source of truth for every
/// downstream consumer, so a later `_edit` stays consistent: it is returned as
/// the new render data plus an escaped edit-base (`Some` only when the retry
/// fired). Any other failure — and a retry that still fails — surfaces the
/// ORIGINAL stderr unchanged so the model sees the real problem. Compile
/// errors map to `InvalidArgs` (nudge the model to fix its input); anything
/// else to `Failed`. Returns `(rendered, render_data, edit_base_escaped,
/// auto_escaped_count)`.
async fn compile_with_at_fixup(
    template: &Template,
    inputs: &[(String, String)],
    render_data: Value,
    data: &Value,
    preview_page: u32,
    staging_path: Option<&Path>,
) -> Result<(typst::Rendered, Value, Option<Value>, usize), ToolError> {
    let compile_failed =
        |msg: String| ToolError::InvalidArgs(format!("typst compile failed:\n{msg}"));
    match typst::compile(template, inputs, preview_page, staging_path).await {
        Ok(r) => Ok((r, render_data, None, 0)),
        Err(typst::CompileError::Failed(msg)) if is_unescaped_at_error(&msg) => {
            let (esc_render, n) = escape_unescaped_ats(&render_data);
            if n == 0 {
                // Signature matched but there was no unescaped `@` to fix
                // (some other unresolved reference) — don't retry blindly.
                return Err(compile_failed(msg));
            }
            let esc_inputs = inputs_from_data(template, &esc_render)?;
            match typst::compile(template, &esc_inputs, preview_page, staging_path).await {
                Ok(r) => {
                    let (esc_base, _) = escape_unescaped_ats(data);
                    Ok((r, esc_render, Some(esc_base), n))
                }
                Err(_) => Err(compile_failed(msg)),
            }
        }
        Err(typst::CompileError::Failed(msg)) => Err(compile_failed(msg)),
        Err(other) => Err(ToolError::Failed(other.to_string())),
    }
}

/// Best-effort editable-PowerPoint export for a template that opts in
/// (`[pptx]`). Returns `(uploaded, error_note)`: a sandbox or upload hiccup
/// must never fail the render — the PDF + preview already landed — so it comes
/// back as a note string instead of an `Err`.
#[allow(clippy::too_many_arguments)]
async fn export_pptx(
    s3: &gateway_core::server::config::S3Config,
    turn_id: &str,
    pptx_name: &str,
    sandbox: &SandboxClient,
    template: &Template,
    cfg: &PptxExport,
    render_data: &Value,
    staged: &[(String, Vec<u8>)],
) -> (Option<chat_attachments::UploadOutcome>, Option<String>) {
    match convert_to_pptx(sandbox, template, cfg, render_data, staged).await {
        Ok(bytes) => match chat_attachments::upload(s3, turn_id, pptx_name, PPTX_MIME, bytes).await
        {
            Ok(out) => (Some(out), None),
            Err(e) => (None, Some(format!("upload pptx: {e}"))),
        },
        Err(e) => {
            tracing::warn!(error = %e, template = %template.id, "pptx export failed");
            (None, Some(e.to_string()))
        }
    }
}

/// Best-effort editable-Word export (`[docx]`): compile the template to HTML
/// and convert to `.docx` (+ a bonus `.odt`) via pandoc + brand-font embedding.
/// Returns `(docx, odt, error_note)` — same best-effort contract as
/// [`export_pptx`]; a failed `.odt` upload is only logged (the `.docx` already
/// landed).
#[allow(clippy::too_many_arguments)]
async fn export_docx(
    s3: &gateway_core::server::config::S3Config,
    turn_id: &str,
    docx_name: &str,
    odt_name: &str,
    sandbox: &SandboxClient,
    template: &Template,
    cfg: &DocxExport,
    inputs: &[(String, String)],
    staged: &[(String, Vec<u8>)],
) -> (
    Option<chat_attachments::UploadOutcome>,
    Option<chat_attachments::UploadOutcome>,
    Option<String>,
) {
    match convert_to_docx(sandbox, template, cfg, inputs, staged).await {
        Ok((docx_bytes, odt_bytes)) => {
            let mut docx_out = None;
            let mut docx_error = None;
            match chat_attachments::upload(s3, turn_id, docx_name, DOCX_MIME, docx_bytes).await {
                Ok(out) => docx_out = Some(out),
                Err(e) => docx_error = Some(format!("upload docx: {e}")),
            }
            let mut odt_out = None;
            if let Some(odt_bytes) = odt_bytes {
                match chat_attachments::upload(s3, turn_id, odt_name, ODT_MIME, odt_bytes).await {
                    Ok(out) => odt_out = Some(out),
                    Err(e) => tracing::warn!(error = %e, "upload odt"),
                }
            }
            (docx_out, odt_out, docx_error)
        }
        Err(e) => {
            tracing::warn!(error = %e, template = %template.id, "docx export failed");
            (None, None, Some(e.to_string()))
        }
    }
}

/// MIME for a `.pptx` (OOXML presentation).
const PPTX_MIME: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
/// MIME for a `.docx` (OOXML word-processing document).
const DOCX_MIME: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
/// MIME for a `.odt` (OpenDocument text).
const ODT_MIME: &str = "application/vnd.oasis.opendocument.text";
/// The `.odt` the sandbox script leaves in `/work` (best-effort — a LibreOffice
/// conversion of the `.docx`, so it inherits the brand font).
const ODT_OUT: &str = "out.odt";

/// Single input file name carrying the zipped template + deck.
const BUNDLE_NAME: &str = "bundle.zip";
/// The lone `.pptx` the sandbox script leaves in `/work`.
const PPTX_OUT: &str = "presentation.pptx";
/// The `.docx` the sandbox script leaves in `/work`.
const DOCX_OUT: &str = "out.docx";

/// Convert a document template to an editable `.docx` in the sandbox via
/// `typst --format html` → pandoc → brand-font post-process.
///
/// typst has no native `.docx` export, so we compile the template to HTML
/// (the real compiler — content, headings, lists, tables, and the logo/brand
/// bar images all come through; only fixed-layout chrome like a `place()`d
/// footer is dropped) and let pandoc turn that into an editable Word doc. When
/// the template declares a `[docx] font`, we set it as the document default and
/// **embed** it as an obfuscated `.odttf` (sourced from the template's own
/// `fonts/<font>-Regular.ttf` / `-Bold.ttf`), so the output renders on-brand
/// even where the font isn't installed. Single-source: no reference.docx.
///
/// `inputs` are the same `--input k=v` pairs the PDF render used; they ride in
/// as `inputs.json` and drive the sandbox-side HTML compile.
async fn convert_to_docx(
    sandbox: &SandboxClient,
    template: &Template,
    cfg: &DocxExport,
    inputs: &[(String, String)],
    staged: &[(String, Vec<u8>)],
) -> Result<(Vec<u8>, Option<Vec<u8>>), ToolError> {
    let map: serde_json::Map<String, Value> = inputs
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    let inputs_json = serde_json::to_vec(&Value::Object(map))
        .map_err(|e| ToolError::Failed(format!("docx export: serialize inputs: {e}")))?;
    // Reuse the pptx bundler: zip the template dir + inject `inputs.json` +
    // any staged upload images.
    let bundle = build_bundle_zip(&template.root, "inputs.json", &inputs_json, staged)?;

    let req = RunRequest {
        language: Language::Bash,
        code: docx_script(&template.source_file, cfg.font.as_deref()),
        files: vec![InputFile {
            name: BUNDLE_NAME.to_string(),
            content_b64: b64::encode(&bundle),
        }],
        timeout_secs: None,
        network: false,
        container_id: None,
        keep_alive: false,
    };
    let resp = sandbox.run_job(req).await?;
    if resp.exit_code != 0 || resp.timed_out {
        return Err(ToolError::Failed(format!(
            "sandbox docx conversion failed (exit {}{}): {}",
            resp.exit_code,
            if resp.timed_out { ", timed out" } else { "" },
            tail(&resp.stderr, 600),
        )));
    }
    let art = resp
        .artifacts
        .iter()
        .find(|a| a.name == DOCX_OUT)
        .ok_or_else(|| {
            ToolError::Failed(format!(
                "sandbox produced no {DOCX_OUT}; stderr: {}",
                tail(&resp.stderr, 600)
            ))
        })?;
    let docx = b64::decode(&art.content_b64)
        .ok_or_else(|| ToolError::Failed("docx artifact base64 invalid".into()))?;
    // ODT is best-effort (a LibreOffice conversion of the docx); if it didn't
    // materialise, the docx still stands.
    let odt = resp
        .artifacts
        .iter()
        .find(|a| a.name == ODT_OUT)
        .and_then(|a| b64::decode(&a.content_b64));
    Ok((docx, odt))
}

/// The bash wrapper for the docx recipe: unzip the bundle, hand `SRC`/`FONT`
/// to the python driver ([`DOCX_PY`]), and surface `out.docx` at `/work`.
/// `DOCX_PY` is concatenated (not `format!`-interpolated) so its Python
/// braces don't collide with format placeholders.
fn docx_script(source_file: &str, font: Option<&str>) -> String {
    format!(
        "set -e\nexport HOME=/tmp\nmkdir -p /work/build && cd /work/build\n\
         unzip -q /work/{bundle}\nexport SRC='{src}' FONT='{font}'\npython3 - <<'PYEOF'\n",
        bundle = BUNDLE_NAME,
        src = source_file,
        font = font.unwrap_or(""),
    ) + DOCX_PY
        + &format!(
            "PYEOF\n\
             mv -f /work/build/{DOCX_OUT} /work/{DOCX_OUT}\n\
             [ -f /work/build/{ODT_OUT} ] && mv -f /work/build/{ODT_OUT} /work/{ODT_OUT} || true\n"
        )
}

/// Python driver for the docx export (reads env `SRC` = template `.typ`,
/// `FONT` = brand font name or ""): compile the template to HTML, pandoc it to
/// `out.docx`, then — when a font is set — make it the document default and
/// embed it as obfuscated `.odttf` (ECMA-376: first 32 bytes XOR the reversed
/// fontKey GUID) from the template's `fonts/<font>-{Regular,Bold}.ttf`.
const DOCX_PY: &str = r#"import json, subprocess, os, zipfile, uuid, re
src = os.environ["SRC"]
font = os.environ.get("FONT") or None
inp = json.load(open("inputs.json"))
cmd = ["typst","compile","--format","html","--features","html","--root",".",src,"doc.html"]
for k,v in inp.items():
    cmd += ["--input","%s=%s"%(k,v)]
subprocess.run(cmd, check=True)
subprocess.run(["pandoc","-f","html","-t","docx","doc.html","-o","out.docx"], check=True)
# ODT as a bonus format for LibreOffice/OpenOffice users (small, from the same
# HTML; best-effort — a failure just skips it).
subprocess.run(["pandoc","-f","html","-t","odt","doc.html","-o","out.odt"], check=False)
if font:
    from docx import Document
    from docx.oxml.ns import qn
    from docx.oxml import OxmlElement
    d = Document("out.docx")
    d.styles["Normal"].font.name = font
    rpr = d.styles["Normal"].element.get_or_add_rPr()
    rf = rpr.find(qn("w:rFonts"))
    if rf is None:
        rf = OxmlElement("w:rFonts"); rpr.append(rf)
    for a in ("w:ascii","w:hAnsi","w:cs","w:eastAsia"):
        rf.set(qn(a), font)
    d.save("out.docx")
    def obf(data, guid_hex):
        key = bytes.fromhex(guid_hex)[::-1]
        out = bytearray(data)
        for i in range(32): out[i] ^= key[i % 16]
        return bytes(out)
    NS_R = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
    fonts = []
    for i,(fn,tag) in enumerate(((font+"-Regular.ttf","embedRegular"),(font+"-Bold.ttf","embedBold")), 1):
        p = os.path.join("fonts", fn)
        if os.path.exists(p):
            g = uuid.uuid4()
            fonts.append((i, tag, "{%s}"%str(g).upper(), "font%d.odttf"%i, obf(open(p,"rb").read(), g.hex)))
    if fonts:
        zin = zipfile.ZipFile("out.docx","r"); items = {n: zin.read(n) for n in zin.namelist()}; zin.close()
        for i,tag,guid,fname,data in fonts:
            items["word/fonts/"+fname] = data
        ft = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<w:fonts xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="%s"><w:font w:name="%s">' % (NS_R, font)
        for i,tag,guid,fname,data in fonts:
            ft += '<w:%s r:id="rIdF%d" w:fontKey="%s" w:subsetted="false"/>' % (tag, i, guid)
        ft += '</w:font></w:fonts>'
        items["word/fontTable.xml"] = ft.encode()
        rels = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        for i,tag,guid,fname,data in fonts:
            rels += '<Relationship Id="rIdF%d" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/font" Target="fonts/%s"/>' % (i, fname)
        rels += '</Relationships>'
        items["word/_rels/fontTable.xml.rels"] = rels.encode()
        ct = items["[Content_Types].xml"].decode()
        if "obfuscatedFont" not in ct:
            ct = ct.replace("</Types>", '<Default Extension="odttf" ContentType="application/vnd.openxmlformats-officedocument.obfuscatedFont"/><Override PartName="/word/fontTable.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.fontTable+xml"/></Types>')
            items["[Content_Types].xml"] = ct.encode()
        if "word/settings.xml" in items:
            st = items["word/settings.xml"].decode()
            if "embedTrueTypeFonts" not in st:
                st = re.sub(r"(<w:settings[^>]*>)", r"\1<w:embedTrueTypeFonts/><w:embedSystemFonts/><w:saveSubsetFonts/>", st, count=1)
                items["word/settings.xml"] = st.encode()
        dr = items["word/_rels/document.xml.rels"].decode()
        if "fontTable" not in dr:
            dr = dr.replace("</Relationships>", '<Relationship Id="rIdFontTable" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/fontTable" Target="fontTable.xml"/></Relationships>')
            items["word/_rels/document.xml.rels"] = dr.encode()
        zout = zipfile.ZipFile("out.docx","w",zipfile.ZIP_DEFLATED)
        for n,data in items.items(): zout.writestr(n, data)
        zout.close()
"#;

/// Convert a rendered deck to an editable `.pptx` in the sandbox.
///
/// Bundles the template dir + the freshly-serialized deck data, ships it
/// as one zip (the runner forbids `/` in input filenames, so a directory
/// tree can't go in as separate files), and runs the validated recipe:
/// typ2pptx compiles the `.typ` to a typst.ts SVG (text as
/// `<foreignObject>` overlays → editable PowerPoint text, shapes +
/// gradients as native DrawingML), then we stamp the brand font over
/// typ2pptx's monospace misclassification. Returns the `.pptx` bytes.
async fn convert_to_pptx(
    sandbox: &SandboxClient,
    template: &Template,
    cfg: &PptxExport,
    data: &Value,
    staged: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, ToolError> {
    let deck = data.get(&cfg.data_field).ok_or_else(|| {
        ToolError::Failed(format!(
            "pptx export: data has no `{}` field to write as {}",
            cfg.data_field, cfg.data_file
        ))
    })?;
    let deck_bytes = serde_json::to_vec_pretty(deck)
        .map_err(|e| ToolError::Failed(format!("pptx export: serialize deck: {e}")))?;

    let bundle = build_bundle_zip(&template.root, &cfg.data_file, &deck_bytes, staged)?;
    let script = pptx_script(&template.source_file, cfg.font.as_deref());

    let req = RunRequest {
        language: Language::Bash,
        code: script,
        files: vec![InputFile {
            name: BUNDLE_NAME.to_string(),
            content_b64: b64::encode(&bundle),
        }],
        timeout_secs: None,
        network: false,
        container_id: None,
        keep_alive: false,
    };
    let resp = sandbox.run_job(req).await?;
    if resp.exit_code != 0 || resp.timed_out {
        return Err(ToolError::Failed(format!(
            "sandbox conversion failed (exit {}{}): {}",
            resp.exit_code,
            if resp.timed_out { ", timed out" } else { "" },
            tail(&resp.stderr, 600),
        )));
    }
    let art = resp
        .artifacts
        .iter()
        .find(|a| a.name == PPTX_OUT)
        .ok_or_else(|| {
            ToolError::Failed(format!(
                "sandbox produced no {PPTX_OUT}; stderr: {}",
                tail(&resp.stderr, 600)
            ))
        })?;
    b64::decode(&art.content_b64)
        .ok_or_else(|| ToolError::Failed("pptx artifact base64 invalid".into()))
}

/// Zip the template directory (template.typ + fonts + assets), swapping
/// in the freshly-serialized deck as `data_file` (any on-disk sample of
/// that name is skipped). Nested paths are preserved as ZIP entry names.
fn build_bundle_zip(
    root: &Path,
    data_file: &str,
    deck_bytes: &[u8],
    staged: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, ToolError> {
    let mut buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir)
                .map_err(|e| ToolError::Failed(format!("pptx bundle: read dir {dir:?}: {e}")))?;
            for entry in entries {
                let entry =
                    entry.map_err(|e| ToolError::Failed(format!("pptx bundle: entry: {e}")))?;
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path
                    .strip_prefix(root)
                    .map_err(|_| ToolError::Failed("pptx bundle: strip prefix".into()))?
                    .to_string_lossy()
                    .replace('\\', "/");
                // Skip any on-disk sample of the data file; the real deck
                // content is injected below.
                if rel == data_file {
                    continue;
                }
                let bytes = std::fs::read(&path)
                    .map_err(|e| ToolError::Failed(format!("pptx bundle: read {path:?}: {e}")))?;
                zw.start_file(rel, opts)
                    .map_err(|e| ToolError::Failed(format!("pptx bundle: start: {e}")))?;
                zw.write_all(&bytes)
                    .map_err(|e| ToolError::Failed(format!("pptx bundle: write: {e}")))?;
            }
        }
        zw.start_file(data_file.to_string(), opts)
            .map_err(|e| ToolError::Failed(format!("pptx bundle: start deck: {e}")))?;
        zw.write_all(deck_bytes)
            .map_err(|e| ToolError::Failed(format!("pptx bundle: write deck: {e}")))?;
        // Staged uploads (e.g. `uploads/…` images the deck references) so
        // typ2pptx can place them from the unzipped bundle root, same as the
        // shipped `assets/`.
        for (rel, bytes) in staged {
            zw.start_file(rel.clone(), opts)
                .map_err(|e| ToolError::Failed(format!("pptx bundle: start staged {rel}: {e}")))?;
            zw.write_all(bytes)
                .map_err(|e| ToolError::Failed(format!("pptx bundle: write staged {rel}: {e}")))?;
        }
        zw.finish()
            .map_err(|e| ToolError::Failed(format!("pptx bundle: finish: {e}")))?;
    }
    Ok(buf)
}

// --- Uploaded-image staging (the `att:` image pipeline) --------------------
//
// `fetch_attachment` re-attaches images it pulls out of an uploaded document
// and hands the model `att:<turn>/<file>` refs. When such a ref lands in an
// image field of a render, we fetch the bytes and stage them under `uploads/`
// in the compile root (host PDF/PNG) and the pptx bundle, rewriting the ref to
// that path so `image("uploads/…")` resolves. Refs are NEVER persisted as
// paths — the `.json` edit-base keeps the `att:` form and re-stages each run.

/// Prefix marking an image-field value as a reference to a chat attachment
/// (an uploaded / extracted image) rather than a template-relative path.
const ATT_REF_PREFIX: &str = "att:";

/// Recursively copy `src` into `dst` (files + subdirs; symlinks/others
/// skipped) to build a staging root that overlays the template dir with the
/// per-render uploaded images.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Build a temp compile root: a copy of the template dir plus the `staged`
/// files written at their relative paths. The returned [`tempfile::TempDir`]
/// must outlive the compile that reads from it.
fn build_staging_root(
    template_root: &Path,
    staged: &[(String, Vec<u8>)],
) -> Result<tempfile::TempDir, ToolError> {
    let dir = tempfile::tempdir()
        .map_err(|e| ToolError::Failed(format!("staging root: tempdir: {e}")))?;
    copy_dir_all(template_root, dir.path())
        .map_err(|e| ToolError::Failed(format!("staging root: copy template: {e}")))?;
    for (rel, bytes) in staged {
        let dest = dir.path().join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::Failed(format!("staging root: mkdir for {rel}: {e}")))?;
        }
        std::fs::write(&dest, bytes)
            .map_err(|e| ToolError::Failed(format!("staging root: write {rel}: {e}")))?;
    }
    Ok(dir)
}

/// Collect the unique `att:<turn>/<file>` refs appearing as string values
/// anywhere in `v` (fields, nested slides, arrays).
fn collect_att_tokens(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if s.starts_with(ATT_REF_PREFIX) && !out.contains(s) => {
            out.push(s.clone());
        }
        Value::Array(a) => a.iter().for_each(|x| collect_att_tokens(x, out)),
        Value::Object(m) => m.values().for_each(|x| collect_att_tokens(x, out)),
        _ => {}
    }
}

/// Sanitize an attachment filename into one safe path component for the
/// staging `uploads/` dir (no separators, no traversal).
fn sanitize_component(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let s: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() { "img".to_string() } else { s }
}

/// Apply the staging replacements (`att:` token → `uploads/…` path) to a raw
/// string. `reps` is sorted longest-token-first by [`stage_att_refs`] so a
/// shorter ref can't be a textual prefix that partially clobbers a longer one.
fn apply_replacements(s: &str, reps: &[(String, String)]) -> String {
    let mut out = s.to_string();
    for (from, to) in reps {
        out = out.replace(from.as_str(), to.as_str());
    }
    out
}

/// Apply the staging replacements to every string in a JSON value by
/// round-tripping its serialized form (the tokens are opaque and unique, so a
/// textual replace is safe and simplest).
fn apply_reps_to_value(data: &Value, reps: &[(String, String)]) -> Result<Value, ToolError> {
    if reps.is_empty() {
        return Ok(data.clone());
    }
    let s = serde_json::to_string(data)
        .map_err(|e| ToolError::Failed(format!("stage images: serialize data: {e}")))?;
    let s = apply_replacements(&s, reps);
    serde_json::from_str(&s)
        .map_err(|e| ToolError::Failed(format!("stage images: reparse data: {e}")))
}

/// Resolve `att:` image refs in `data`: fetch each referenced attachment's
/// bytes and assign a unique `uploads/u<n>_<file>` staging path. Returns the
/// staged files (path → bytes) and the token→path replacements to apply to the
/// render inputs/data. A missing / malformed ref is a hard error so the model
/// fixes the ref instead of silently rendering a broken image.
async fn stage_att_refs(
    s3: &gateway_core::server::config::S3Config,
    data: &Value,
) -> Result<(Vec<(String, Vec<u8>)>, Vec<(String, String)>), ToolError> {
    let mut tokens = Vec::new();
    collect_att_tokens(data, &mut tokens);
    let mut staged = Vec::new();
    let mut reps = Vec::new();
    for (idx, tok) in tokens.iter().enumerate() {
        let id = tok.strip_prefix(ATT_REF_PREFIX).unwrap_or(tok);
        let (turn, file) = id.split_once('/').ok_or_else(|| {
            ToolError::InvalidArgs(format!(
                "image ref {tok:?} is malformed; expected `att:<turn_id>/<filename>`"
            ))
        })?;
        let fetched = chat_attachments::fetch(s3, turn, file).await.map_err(|e| {
            ToolError::InvalidArgs(format!(
                "image ref {tok:?} could not be fetched ({e}); use the exact `ref` \
                 string from fetch_attachment's image_refs"
            ))
        })?;
        let relpath = format!("uploads/u{idx}_{}", sanitize_component(file));
        staged.push((relpath.clone(), fetched.bytes));
        reps.push((tok.clone(), relpath));
    }
    reps.sort_by_key(|r| std::cmp::Reverse(r.0.len()));
    Ok((staged, reps))
}

/// The bash recipe run in the sandbox. All messy work happens in a
/// subdir; only the final `.pptx` is copied to `/work` so it is the sole
/// returned artifact. Post-processing on the typ2pptx output:
///   1. stamp the brand `font` over typ2pptx's `Consolas` misclassification;
///   2. switch text bodies to `normAutofit` (shrink-to-fit) so a renderer's
///      slightly-different metrics can't overflow/overlap the tight boxes;
///   3. embed the brand font (from the bundle's `fonts/`) so the deck
///      renders correctly even where the font isn't installed.
fn pptx_script(source_file: &str, font: Option<&str>) -> String {
    // Python string literal for the font (or None).
    let font_py = match font {
        Some(f) => format!("{f:?}"),
        None => "None".to_string(),
    };
    format!(
        r#"set -e
mkdir -p /work/build
cd /work/build
unzip -q /work/{bundle}
export TYPST_FONT_PATHS="$PWD/fonts"
typ2pptx {src} --root "$PWD" --detect-paragraphs -o deck.pptx
python3 - <<'PYEOF'
import zipfile, glob, os, tempfile, re, shutil
FONT = {font_py}
d = tempfile.mkdtemp()
with zipfile.ZipFile("deck.pptx") as z:
    z.extractall(d)
# Font fixup + shrink-to-fit autofit, per slide.
for f in glob.glob(os.path.join(d, "ppt", "slides", "*.xml")):
    s = open(f, encoding="utf-8").read()
    if FONT:
        s = s.replace('typeface="Consolas"', 'typeface="%s"' % FONT)
    s = s.replace("<a:spAutoFit/>", "<a:normAutofit/>").replace("<a:noAutofit/>", "<a:normAutofit/>")
    s = re.sub(r'<a:bodyPr\b[^>]*/>', lambda m: m.group(0)[:-2] + "><a:normAutofit/></a:bodyPr>", s)
    open(f, "w", encoding="utf-8").write(s)
# Embed the brand font so the deck renders correctly without it installed.
if FONT:
    ttfs = sorted(glob.glob("fonts/*.ttf"))
    def pick(subs):
        for t in ttfs:
            if any(x in os.path.basename(t) for x in subs):
                return t
        return None
    reg = pick(["Regular"]) or (ttfs[0] if ttfs else None)
    bold = pick(["SemiBold", "Semibold", "Bold"]) or reg
    faces = []
    if reg:
        faces.append(("regular", reg))
    if bold and bold != reg:
        faces.append(("bold", bold))
    if faces:
        os.makedirs(os.path.join(d, "ppt", "fonts"), exist_ok=True)
        ct = os.path.join(d, "[Content_Types].xml")
        s = open(ct, encoding="utf-8").read()
        if "fntdata" not in s:
            s = s.replace("</Types>", '<Default Extension="fntdata" ContentType="application/x-fontdata"/></Types>')
            open(ct, "w", encoding="utf-8").write(s)
        pr = os.path.join(d, "ppt", "_rels", "presentation.xml.rels")
        s = open(pr, encoding="utf-8").read()
        ids = [int(x) for x in re.findall(r'Id="rId(\d+)"', s)]
        nid = (max(ids) + 1) if ids else 1
        addrel = ""
        slotrids = []
        for i, (slot, ttf) in enumerate(faces, start=1):
            shutil.copy(ttf, os.path.join(d, "ppt", "fonts", "font%d.fntdata" % i))
            rid = "rId%d" % nid
            nid += 1
            addrel += '<Relationship Id="%s" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/font" Target="fonts/font%d.fntdata"/>' % (rid, i)
            slotrids.append((slot, rid))
        s = s.replace("</Relationships>", addrel + "</Relationships>")
        open(pr, "w", encoding="utf-8").write(s)
        pf = os.path.join(d, "ppt", "presentation.xml")
        s = open(pf, encoding="utf-8").read()
        s = s.replace("<p:presentation ", '<p:presentation embedTrueTypeFonts="1" ', 1)
        slots = "".join('<p:%s r:id="%s"/>' % (slot, rid) for slot, rid in slotrids)
        efl = '<p:embeddedFontLst><p:embeddedFont><p:font typeface="%s"/>%s</p:embeddedFont></p:embeddedFontLst>' % (FONT, slots)
        m = re.search(r'<p:notesSz[^>]*/>', s)
        if m:
            s = s[:m.end()] + efl + s[m.end():]
        else:
            s = s.replace("</p:presentation>", efl + "</p:presentation>")
        open(pf, "w", encoding="utf-8").write(s)
out = "/work/{out}"
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    for root, _, files in os.walk(d):
        for fn in files:
            p = os.path.join(root, fn)
            z.write(p, os.path.relpath(p, d))
PYEOF
rm -rf /work/build /work/{bundle}
"#,
        bundle = BUNDLE_NAME,
        src = source_file,
        font_py = font_py,
        out = PPTX_OUT,
    )
}

/// Last `max` bytes of `s` (char-boundary safe), prefixed with `…` when
/// clipped. For surfacing sandbox stderr tails in errors.
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut i = s.len() - max;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    format!("…{}", &s[i..])
}

/// Companion to [`TypstRenderTool`]: re-render a previously-produced
/// document after applying a small JSON Patch to its stored field
/// values. Registered per-template as `typst_<id>_edit` so the model
/// can change one headline without resending the whole input.
pub struct TypstEditTool {
    template: Arc<Template>,
    /// Sandbox client, threaded through so the re-render also refreshes
    /// the editable `.pptx` (see [`TypstRenderTool::sandbox`]).
    sandbox: Option<Arc<SandboxClient>>,
    /// Leaked `Box<str>` (`typst_<id>_edit`), same rationale as
    /// [`TypstRenderTool::id`].
    id: &'static str,
}

impl TypstEditTool {
    pub fn new(template: Arc<Template>, sandbox: Option<Arc<SandboxClient>>) -> Self {
        let id: &'static str = Box::leak(format!("typst_{}_edit", template.id).into_boxed_str());
        Self {
            template,
            sandbox,
            id,
        }
    }
}

impl Tool for TypstEditTool {
    fn id(&self) -> &str {
        self.id
    }

    /// Same as the render tool: an edit re-renders (incl. any sandbox pptx/docx
    /// export), so allow the sandbox ceiling rather than the 30 s default.
    fn max_duration(&self) -> Option<std::time::Duration> {
        self.sandbox
            .as_ref()
            .map(|s| s.loop_timeout() + std::time::Duration::from_secs(30))
    }

    fn schema(&self) -> ToolDef {
        let render_id = format!("typst_{}", self.template.id);
        ToolDef::function(
            self.id(),
            format!(
                "Make a change to a document previously rendered by \
                 `{render_id}` and re-render it — WITHOUT resending the whole \
                 input. Give `base` (the `data_id` the render returned) and \
                 EITHER `find`+`replace` OR `patch`. \
                 `find`/`replace`: swap an exact run of text for another \
                 wherever it appears — best for fixing wording (a sentence, a \
                 headline, a quote) when you don't know the field path. \
                 `patch`: an RFC 6902 JSON Patch for structural edits — \
                 e.g. change the third slide's title \
                 [{{\"op\":\"replace\",\"path\":\"/deck/slides/2/title\",\
                 \"value\":\"New headline\"}}], add a slide \
                 {{\"op\":\"add\",\"path\":\"/deck/slides/-\",\"value\":{{…}}}}, \
                 or remove one {{\"op\":\"remove\",\"path\":\"/deck/slides/1\"}}. \
                 If you don't know the exact current text or the right path, \
                 call `{render_id}_read` FIRST to see the stored content — do \
                 NOT re-render repeatedly to hunt for it. Returns a fresh PDF \
                 + preview and a new `data_id` you can edit again.",
            ),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["base"],
                "properties": {
                    "base": {
                        "type": "string",
                        "description": "The `data_id` returned by a previous \
                                        render/edit of this template, of the \
                                        form `<turn_id>/<file>.json`."
                    },
                    "find": {
                        "type": "string",
                        "description": "Exact text to search for across the \
                                        stored content. Every occurrence is \
                                        replaced with `replace`. Requires \
                                        `replace`. The edit FAILS if the text \
                                        isn't found (so you learn the text was \
                                        wrong instead of silently re-rendering \
                                        unchanged) — call the `_read` tool to \
                                        get the exact current text."
                    },
                    "replace": {
                        "type": "string",
                        "description": "The replacement text for `find`. Use an \
                                        empty string to delete the matched text."
                    },
                    "patch": {
                        "type": "array",
                        "description": "RFC 6902 JSON Patch: an array of \
                                        operations applied in order. Paths are \
                                        JSON Pointers into the stored field \
                                        values (the `deck` object is addressable \
                                        as `/deck/...`). Applied before \
                                        `find`/`replace` when both are given.",
                        "items": {
                            "type": "object",
                            "required": ["op", "path"],
                            "properties": {
                                "op": {
                                    "type": "string",
                                    "enum": ["add", "remove", "replace", "move", "copy", "test"]
                                },
                                "path": { "type": "string" },
                                "from": { "type": "string" },
                                "value": {}
                            }
                        }
                    },
                    "preview_page": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional. Which PDF page (1-based) to \
                                        render as the inline PNG preview so you \
                                        can verify the change. Defaults to 1 \
                                        (the cover); set it to the page you \
                                        edited for a multi-page document/deck."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        let template = self.template.clone();
        Box::pin(async move {
            let turn_id = ctx.assistant_turn_id.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "typst tools are only available inside a chat session \
                     (no assistant turn to attach the re-render to)"
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

            let obj = args.as_object().ok_or_else(|| {
                ToolError::InvalidArgs("expected an object {base, patch|find/replace}".into())
            })?;
            let base = obj.get("base").and_then(Value::as_str).ok_or_else(|| {
                ToolError::InvalidArgs(
                    "`base` (the data_id from the previous render) is required".into(),
                )
            })?;
            let patch = obj.get("patch").and_then(Value::as_array);
            let find = obj.get("find").and_then(Value::as_str);
            let replace = obj.get("replace").and_then(Value::as_str);
            let mut preview_page_map = Map::new();
            if let Some(p) = obj.get("preview_page") {
                preview_page_map.insert("preview_page".to_string(), p.clone());
            }
            let preview_page = take_preview_page(&mut preview_page_map)?;

            // Require exactly one editing mode's worth of input: a patch, a
            // find/replace, or both — but not neither (that would just
            // re-render the same bytes and spam another chip).
            if patch.is_none() && find.is_none() {
                return Err(ToolError::InvalidArgs(
                    "give either `find`+`replace` or a `patch` (or both) — \
                     nothing to change otherwise"
                        .into(),
                ));
            }
            if find.is_some() != replace.is_some() {
                return Err(ToolError::InvalidArgs(
                    "`find` and `replace` must be given together".into(),
                ));
            }

            // Fetch the prior render's data document (the edit base). It
            // lives at <turn>/<file>.json under whichever turn produced it
            // — typically an earlier turn in this same conversation.
            let (base_turn, base_file) = split_attachment_id(base)?;
            let fetched = chat_attachments::fetch(s3, base_turn, base_file)
                .await
                .map_err(|e| ToolError::Failed(format!("could not read base `{base}`: {e}")))?;
            let mut data: Value = serde_json::from_slice(&fetched.bytes).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "base `{base}` is not a JSON data document ({e}); pass the \
                     `data_id` from the render result, not the PDF/PNG id"
                ))
            })?;

            // Structural patch first (if any), then the text find/replace.
            if let Some(patch) = patch {
                super::json_patch::apply(&mut data, patch)
                    .map_err(|e| ToolError::InvalidArgs(format!("could not apply patch: {e}")))?;
            }
            if let (Some(find), Some(replace)) = (find, replace) {
                let n = replace_in_strings(&mut data, find, replace);
                if n == 0 {
                    return Err(ToolError::InvalidArgs(format!(
                        "`find` text was not present in the document, so nothing \
                         changed. Call `typst_{}_read` with this `base` to see \
                         the exact current text, then retry with text that \
                         matches verbatim.",
                        template.id
                    )));
                }
            }

            // Re-validate + re-stringify the edited data, then render and
            // attach to the CURRENT turn exactly like a fresh render.
            let inputs = inputs_from_data(&template, &data)?;
            render_and_attach(
                &ctx,
                turn_id,
                s3,
                &template,
                inputs,
                &data,
                self.sandbox.as_ref(),
                preview_page,
            )
            .await
        })
    }
}

/// Companion read tool `typst_<id>_read`: returns the stored field
/// values / deck structure of a previously-rendered document so the
/// model can locate the exact slide index, JSON-Pointer path, or current
/// wording to target with `_edit` — instead of re-rendering over and
/// over to find where something lives. Read-only: no compile, nothing
/// attached to the reply. Registered per-template alongside render/edit.
pub struct TypstReadTool {
    /// Leaked `Box<str>` (`typst_<id>_read`), same rationale as
    /// [`TypstRenderTool::id`].
    id: &'static str,
}

impl TypstReadTool {
    pub fn new(template: Arc<Template>) -> Self {
        let id: &'static str = Box::leak(format!("typst_{}_read", template.id).into_boxed_str());
        Self { id }
    }
}

impl Tool for TypstReadTool {
    fn id(&self) -> &str {
        self.id
    }

    fn schema(&self) -> ToolDef {
        // `id` is `typst_<tid>_read`; the render/edit siblings drop `_read`.
        let render_id = self.id.strip_suffix("_read").unwrap_or(self.id);
        ToolDef::function(
            self.id(),
            format!(
                "Inspect the stored content of a document previously rendered \
                 by `{render_id}` — its field values, and for a deck the full \
                 slide structure as JSON. Give `base`, the `data_id` that \
                 render/edit returned. Use this to find the exact slide index, \
                 JSON-Pointer path, or current wording you want to change \
                 BEFORE calling `{render_id}_edit`, so you never re-render \
                 repeatedly just to discover where something lives. Read-only: \
                 it attaches nothing to your reply."
            ),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["base"],
                "properties": {
                    "base": {
                        "type": "string",
                        "description": "The `data_id` from a previous render/edit \
                                        of this template (`<turn_id>/<file>.json`)."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed("chat attachments are not configured on this gateway".into())
            })?;
            let base = args
                .as_object()
                .and_then(|o| o.get("base"))
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs("`base` (the data_id) is required".into()))?;
            let (base_turn, base_file) = split_attachment_id(base)?;
            let fetched = chat_attachments::fetch(s3, base_turn, base_file)
                .await
                .map_err(|e| ToolError::Failed(format!("could not read base `{base}`: {e}")))?;
            let data: Value = serde_json::from_slice(&fetched.bytes).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "base `{base}` is not a JSON data document ({e}); pass the \
                     `data_id` from a render/edit result, not the PDF/PNG id"
                ))
            })?;
            Ok(json!({
                "data": data,
                "note": "These are the stored field values backing the document \
                         (a deck's slides are under the deck field). To change \
                         something, call this template's `_edit` tool with the \
                         SAME `base` and either `find`/`replace` (for wording) \
                         or a JSON Patch (for structure such as slide order) — \
                         do not re-render from scratch.",
            }))
        })
    }
}

/// Standalone `typst_<id>_pptx`: (re)export a previously-rendered deck
/// to an editable PowerPoint from its stored data — without re-rendering
/// the PDF. Registered only for templates that opt into `[pptx]` and
/// only when a sandbox is configured. Lets the model produce the `.pptx`
/// on demand (e.g. "give me the editable slides for that deck") rather
/// than only as a side effect of render/edit.
pub struct TypstPptxTool {
    template: Arc<Template>,
    sandbox: Arc<SandboxClient>,
    /// Leaked `Box<str>` (`typst_<id>_pptx`).
    id: &'static str,
}

impl TypstPptxTool {
    pub fn new(template: Arc<Template>, sandbox: Arc<SandboxClient>) -> Self {
        let id: &'static str = Box::leak(format!("typst_{}_pptx", template.id).into_boxed_str());
        Self {
            template,
            sandbox,
            id,
        }
    }
}

impl Tool for TypstPptxTool {
    fn id(&self) -> &str {
        self.id
    }

    /// This tool's whole job is the sandbox pptx export, so it always needs the
    /// sandbox ceiling rather than the 30 s default `TOOL_TIMEOUT`.
    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.sandbox.loop_timeout() + std::time::Duration::from_secs(30))
    }

    fn schema(&self) -> ToolDef {
        let render_id = format!("typst_{}", self.template.id);
        ToolDef::function(
            self.id(),
            format!(
                "Export a deck previously produced by `{render_id}` to an \
                 EDITABLE PowerPoint (.pptx) — real text, shapes and gradients, \
                 not images — ready to import into Google Slides. Give `base`, \
                 the `data_id` that render/edit returned. The `.pptx` is \
                 attached to your reply. (A render already attaches one \
                 automatically; use this to regenerate it or when you only \
                 have the data_id.)"
            ),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["base"],
                "properties": {
                    "base": {
                        "type": "string",
                        "description": "The `data_id` from a previous render/edit \
                                        of this template (`<turn_id>/<file>.json`)."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        let template = self.template.clone();
        Box::pin(async move {
            let turn_id = ctx.assistant_turn_id.as_ref().ok_or_else(|| {
                ToolError::Failed("typst tools are only available inside a chat session".into())
            })?;
            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed("chat attachments are not configured on this gateway".into())
            })?;
            let reservations = ctx.attachment_reservations.as_ref().ok_or_else(|| {
                ToolError::Failed("typst tools require a per-turn reservation set".into())
            })?;
            let cfg = template.pptx.as_ref().ok_or_else(|| {
                ToolError::Failed("this template has no pptx export configured".into())
            })?;

            let base = args
                .as_object()
                .and_then(|o| o.get("base"))
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs("`base` (the data_id) is required".into()))?;
            let (base_turn, base_file) = split_attachment_id(base)?;
            let fetched = chat_attachments::fetch(s3, base_turn, base_file)
                .await
                .map_err(|e| ToolError::Failed(format!("could not read base `{base}`: {e}")))?;
            let data: Value = serde_json::from_slice(&fetched.bytes).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "base `{base}` is not a JSON data document ({e}); pass the \
                     `data_id` from a render/edit result"
                ))
            })?;

            // Re-stage any `att:` image refs the stored deck carries (the
            // edit-base persists refs, not the ephemeral staged paths).
            let (staged, reps) = stage_att_refs(s3, &data).await?;
            let render_data = apply_reps_to_value(&data, &reps)?;
            let bytes =
                convert_to_pptx(&self.sandbox, &template, cfg, &render_data, &staged).await?;
            // Share the deck stem so the .pptx sits beside its siblings.
            let stem = base_file
                .strip_suffix(".json")
                .unwrap_or(&template.output_basename);
            let name = chat_attachments::reserve_filename(
                &ctx.db,
                turn_id,
                reservations,
                &format!("{stem}.pptx"),
            )
            .await
            .map_err(|e| ToolError::Failed(format!("reserve pptx name: {e}")))?;
            let out = chat_attachments::upload(s3, turn_id, &name, PPTX_MIME, bytes)
                .await
                .map_err(|e| ToolError::Failed(format!("upload pptx: {e}")))?;
            let marker = chat_attachments::marker_line(turn_id, &out);
            chat::append_content(&ctx.db, turn_id, &format!("\n\n{marker}\n\n"))
                .await
                .map_err(|e| ToolError::Failed(format!("persist marker: {e}")))?;
            Ok(json!({
                "pptx": { "filename": out.filename, "size": out.bytes,
                          "id": format!("{turn_id}/{}", out.filename) },
                "rendered": "The editable .pptx is now inline in your reply — do \
                             NOT repeat the marker text. Import it into Google \
                             Slides for a native editable deck.",
            }))
        })
    }
}

/// Split `<turn_id>/<filename>` for an attachment id. Mirrors the
/// validation in `fetch_attachment::split_id` (no nested / empty
/// segments) so a hallucinated id is rejected before any S3 call.
fn split_attachment_id(id: &str) -> Result<(&str, &str), ToolError> {
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

/// Extract the optional non-field `preview_page` control from a tool's
/// argument map, removing it so it isn't mistaken for a template field.
/// Defaults to 1 (the cover/first page); must be a positive integer.
fn take_preview_page(map: &mut Map<String, Value>) -> Result<u32, ToolError> {
    match map.remove("preview_page") {
        None | Some(Value::Null) => Ok(1),
        Some(Value::Number(n)) => n
            .as_u64()
            .filter(|p| *p >= 1)
            .and_then(|p| u32::try_from(p).ok())
            .ok_or_else(|| {
                ToolError::InvalidArgs(
                    "`preview_page` must be a positive integer (1-based PDF page)".into(),
                )
            }),
        Some(_) => Err(ToolError::InvalidArgs(
            "`preview_page` must be an integer".into(),
        )),
    }
}

/// Recursively replace every exact occurrence of `find` with `replace`
/// in the string *values* of a JSON document (object values and array
/// elements; object keys are left alone). Returns the number of
/// occurrences replaced. Lets the edit tool do "swap this sentence for
/// that one" without the model needing to know a JSON Pointer path.
fn replace_in_strings(v: &mut Value, find: &str, replace: &str) -> usize {
    match v {
        Value::String(s) if s.contains(find) => {
            let n = s.matches(find).count();
            *s = s.replace(find, replace);
            n
        }
        Value::String(_) => 0,
        Value::Array(a) => a
            .iter_mut()
            .map(|x| replace_in_strings(x, find, replace))
            .sum(),
        Value::Object(m) => m
            .values_mut()
            .map(|x| replace_in_strings(x, find, replace))
            .sum(),
        _ => 0,
    }
}

/// Does `stderr` carry the Typst "unescaped `@`" signature? An unescaped `@`
/// in a markup field is read as a cross-reference (`@label`), and when that
/// label doesn't exist Typst fails the whole compile with
/// ``label `<…>` does not exist in the document``. This is the single most
/// common render crash (every unescaped email / @-handle) and the one we
/// auto-fix via [`escape_unescaped_ats`] + a one-shot recompile. Matched on
/// the stable tail of that message rather than the label name.
fn is_unescaped_at_error(stderr: &str) -> bool {
    stderr.contains("does not exist in the document")
}

/// Escape every UNescaped `@` (→ `\@`) in one string, returning the rewritten
/// string and how many were escaped. An `@` already preceded by a backslash is
/// left alone, so re-running over already-escaped text is a no-op (idempotent).
fn escape_unescaped_ats_str(s: &str) -> (String, usize) {
    let mut out = String::with_capacity(s.len() + 4);
    // Whether the NEXT character is currently escaped by a preceding backslash.
    // Toggling on `\` (rather than a flat "prev was backslash") makes `\\@`
    // correctly count as an escaped-backslash followed by a bare `@`.
    let mut escaped = false;
    let mut n = 0;
    for c in s.chars() {
        if c == '@' && !escaped {
            out.push('\\');
            out.push('@');
            n += 1;
            escaped = false;
        } else {
            out.push(c);
            escaped = c == '\\' && !escaped;
        }
    }
    (out, n)
}

/// Recursively escape unescaped `@` in every string *value* of a JSON document
/// (object values and array elements; keys untouched), returning the rewritten
/// value and the total escaped. Operating on the parsed [`Value`] — not the raw
/// `--input` text — is what keeps the presentation's `deck` JSON valid: serde
/// re-serializes the `\@` we insert as the JSON escape `\\@`, so `json()` still
/// parses, while the string the template `eval`s as markup now carries a
/// literal `@`. The auto-fix paired with [`is_unescaped_at_error`].
fn escape_unescaped_ats(v: &Value) -> (Value, usize) {
    match v {
        Value::String(s) => {
            let (out, n) = escape_unescaped_ats_str(s);
            (Value::String(out), n)
        }
        Value::Array(a) => {
            let mut n = 0;
            let out = a
                .iter()
                .map(|x| {
                    let (v, k) = escape_unescaped_ats(x);
                    n += k;
                    v
                })
                .collect();
            (Value::Array(out), n)
        }
        Value::Object(m) => {
            let mut n = 0;
            let out = m
                .iter()
                .map(|(k, x)| {
                    let (v, c) = escape_unescaped_ats(x);
                    n += c;
                    (k.clone(), v)
                })
                .collect();
            (Value::Object(out), n)
        }
        other => (other.clone(), 0),
    }
}

/// Walk the manifest's declared fields, pull each one out of `args`,
/// type-check it, and stringify it for `typst --input k=v`. Unknown
/// fields in `args` are rejected (the schema declares
/// `additionalProperties: false`, but a buggy model can still
/// produce them — we error rather than silently dropping). Missing
/// required fields are also rejected with a clear message.
fn stringify_args(t: &Template, args: &Value) -> Result<Vec<(String, String)>, ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ToolError::InvalidArgs("expected a JSON object of field values".into()))?;
    let declared: std::collections::HashSet<&str> =
        t.fields.iter().map(|f| f.name.as_str()).collect();
    for key in obj.keys() {
        if !declared.contains(key.as_str()) {
            return Err(ToolError::InvalidArgs(format!(
                "unknown field `{key}` — declared fields: {}",
                t.fields
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let mut out = Vec::with_capacity(t.fields.len());
    for f in &t.fields {
        match obj.get(&f.name) {
            None if f.required => {
                return Err(ToolError::InvalidArgs(format!(
                    "missing required field `{}`",
                    f.name
                )));
            }
            None => continue,
            Some(v) => {
                let s = stringify_one(&f.name, f.ty, v)?;
                out.push((f.name.clone(), s));
            }
        }
    }
    Ok(out)
}

/// The model's field values in their *editable* form — the data
/// document stored alongside a render and used as the base for `_edit`
/// patches.
///
/// A field whose value is a string that *is itself* a JSON object or
/// array is embedded as parsed JSON rather than an escaped string, so
/// the presentation template's `deck` field (a JSON blob passed as a
/// string) becomes a real nested object: a user can read it and a
/// JSON Patch can address `/deck/slides/2/title`. Plain-string fields
/// (a letter body, a subject) are left exactly as given — we only
/// reinterpret values that clearly open with `{` or `[`, so ordinary
/// prose is never coerced into a number/bool/etc. [`inputs_from_data`]
/// is the inverse: it re-stringifies the embedded objects back to the
/// `--input` strings typst expects.
fn data_value(args: &Value) -> Value {
    match args {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), embed_json_strings(v));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Pretty-print a data document for storage. Falls back to the compact
/// form only if (impossibly) serialization fails, so there is always
/// *something* to attach.
fn serialize_data(data: &Value) -> Vec<u8> {
    serde_json::to_vec_pretty(data).unwrap_or_else(|_| data.to_string().into_bytes())
}

/// If `v` is a string that parses as a JSON object/array, return the
/// parsed value; otherwise return `v` untouched. Only `{`/`[`-leading
/// strings are probed so prose like `"42"` or `"true"` stays a string.
fn embed_json_strings(v: &Value) -> Value {
    if let Value::String(s) = v {
        let t = s.trim_start();
        if (t.starts_with('{') || t.starts_with('['))
            && let Ok(parsed) = serde_json::from_str::<Value>(s)
        {
            return parsed;
        }
    }
    v.clone()
}

/// Inverse of [`data_value`]: turn an (already-validated) data document
/// back into the `(name, --input value)` pairs typst compiles with. A
/// field whose patched value is a JSON object/array is re-serialized to
/// a compact string (the form `deck` had when the model first passed
/// it); strings pass through; numbers/bools stringify. Unknown keys and
/// missing required fields are rejected, mirroring [`stringify_args`].
fn inputs_from_data(t: &Template, data: &Value) -> Result<Vec<(String, String)>, ToolError> {
    let obj = data.as_object().ok_or_else(|| {
        ToolError::InvalidArgs("patched data is not a JSON object of field values".into())
    })?;
    let declared: std::collections::HashSet<&str> =
        t.fields.iter().map(|f| f.name.as_str()).collect();
    for key in obj.keys() {
        if !declared.contains(key.as_str()) {
            return Err(ToolError::InvalidArgs(format!(
                "patch produced unknown field `{key}` — declared fields: {}",
                t.fields
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let mut out = Vec::with_capacity(t.fields.len());
    for f in &t.fields {
        match obj.get(&f.name) {
            None if f.required => {
                return Err(ToolError::InvalidArgs(format!(
                    "patch left required field `{}` unset",
                    f.name
                )));
            }
            None => continue,
            Some(v) => out.push((f.name.clone(), value_to_input(v))),
        }
    }
    Ok(out)
}

/// Stringify one data value into a `--input` string. Objects/arrays
/// become compact JSON (the deck round-trips back to its string form);
/// scalars stringify the obvious way; null becomes empty.
fn value_to_input(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(_) | Value::Array(_) => v.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
    }
}

/// Signed-in user's identity, resolved once to satisfy any `default_from`
/// fields the model left blank.
struct Identity {
    name: Option<String>,
    email: Option<String>,
}

impl Identity {
    fn value(&self, src: DefaultSource) -> Option<&str> {
        let raw = match src {
            DefaultSource::UserName => self.name.as_deref(),
            DefaultSource::UserEmail => self.email.as_deref(),
        };
        // A user row with a blank/whitespace value is as good as absent.
        raw.map(str::trim).filter(|s| !s.is_empty())
    }
}

/// A model-supplied field counts as "not given" when it's missing entirely
/// or a blank/whitespace-only string — both mean the default should fill in.
fn arg_is_blank(args: &Map<String, Value>, name: &str) -> bool {
    match args.get(name) {
        None => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(_) => false,
    }
}

/// Whether the model supplied any identity-backed (`default_from`) field
/// itself. The identity fields travel together: if the model set even one
/// (e.g. writing as someone else), we leave the omitted partners null rather
/// than backfilling them — so we never pair one person's name with another's
/// email.
fn identity_claimed_by_model(t: &Template, args: &Map<String, Value>) -> bool {
    t.fields
        .iter()
        .any(|f| f.default_from.is_some() && !arg_is_blank(args, &f.name))
}

/// Does any `default_from` field still need filling? Lets `run` skip the DB
/// read when no field defaults from identity, the model already supplied one,
/// or the model claimed the identity block itself.
fn wants_identity(t: &Template, args: &Map<String, Value>) -> bool {
    !identity_claimed_by_model(t, args)
        && t.fields
            .iter()
            .any(|f| f.default_from.is_some() && arg_is_blank(args, &f.name))
}

/// Fill each `default_from` field the model omitted/left blank with the
/// signed-in user's value — unless the model claimed the identity block (then
/// the omitted partners stay unset). Pure over (template, args, identity) so
/// the behaviour is unit-testable without a DB. An identity value that's
/// itself absent leaves the field unset (the template renders it gracefully).
fn apply_identity_defaults(t: &Template, args: &mut Map<String, Value>, id: &Identity) {
    if identity_claimed_by_model(t, args) {
        return;
    }
    for f in &t.fields {
        let Some(src) = f.default_from else { continue };
        if let Some(v) = id.value(src) {
            args.insert(f.name.clone(), Value::String(v.to_string()));
        }
    }
}

/// The extensions a typst render writes together; the group must share
/// a stem so siblings stay in sync (not `foo-2.pdf` paired with
/// `foo-3.png` because one was free and the other wasn't). Passed to
/// [`chat_attachments::reserve_basename`] on every render so the
/// reservation is taken as a unit. `pdf` + `png` are the visible
/// attachments; `json` holds the field values as the `_edit` patch
/// base and is reserved (so its name can't collide) but not shown as a
/// chat chip. The static template `.typ` is deliberately NOT attached —
/// it can't be edited through the tool and can't recompile without its
/// fonts/assets, so it only adds clutter.
const TYPST_EXTS: &[&str] = &["pdf", "png", "json", "pptx", "docx", "odt"];

/// Whether `filename` is one of this template's visible typst chips
/// (`<base>.pdf` / `.png` / `.pptx`, including the `-2`, `-3`, … dedup
/// suffixes) — i.e. a deliverable an *earlier* same-turn render spliced
/// in. Used by the per-turn supersede in [`render_and_attach`] so a turn
/// shows only the latest render. The hidden `.json` edit base is
/// deliberately excluded (it carries no chip and must survive as a patch
/// target), as is any unrelated upload.
fn is_template_typst_chip(template: &Template, filename: &str) -> bool {
    let base = template.output_basename.as_str();
    let stem = match filename.rsplit_once('.') {
        Some((stem, "pdf" | "png" | "pptx" | "docx" | "odt")) => stem,
        _ => return false,
    };
    if stem == base {
        return true;
    }
    // `<base>-<n>` from the dedup suffix (n a positive integer).
    match stem.strip_prefix(base).and_then(|r| r.strip_prefix('-')) {
        Some(n) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

fn stringify_one(name: &str, ty: FieldType, v: &Value) -> Result<String, ToolError> {
    match (ty, v) {
        (FieldType::String, Value::String(s)) => Ok(s.clone()),
        (FieldType::Integer, Value::Number(n)) if n.is_i64() => Ok(n.to_string()),
        (FieldType::Boolean, Value::Bool(b)) => Ok(b.to_string()),
        (ty, got) => Err(ToolError::InvalidArgs(format!(
            "field `{name}` expects {expected}, got {got}",
            expected = ty.json_schema_name(),
            got = describe_value(got),
        ))),
    }
}

fn describe_value(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Number(n) if n.is_i64() => "integer",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Null => "null",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_core::server::typst::Field;
    use std::path::PathBuf;

    fn stub_template() -> Template {
        Template {
            id: "stub".into(),
            title: "Stub".into(),
            description: "stub".into(),
            output_basename: "stub".into(),
            fields: vec![
                Field {
                    name: "title".into(),
                    ty: FieldType::String,
                    required: true,
                    description: "doc title".into(),
                    default_from: None,
                },
                Field {
                    name: "draft".into(),
                    ty: FieldType::Boolean,
                    required: false,
                    description: "stamp as draft".into(),
                    default_from: None,
                },
            ],
            root: PathBuf::from("/dev/null"),
            source_file: "template.typ".into(),
            pptx: None,
            docx: None,
        }
    }

    #[test]
    fn stringify_passes_string_through() {
        let t = stub_template();
        let args = json!({"title": "Hello world"});
        let out = stringify_args(&t, &args).unwrap();
        assert_eq!(out, vec![("title".into(), "Hello world".into())]);
    }

    #[test]
    fn stringify_collects_optional_when_present() {
        let t = stub_template();
        let args = json!({"title": "x", "draft": true});
        let out = stringify_args(&t, &args).unwrap();
        assert_eq!(
            out,
            vec![
                ("title".into(), "x".into()),
                ("draft".into(), "true".into())
            ]
        );
    }

    #[test]
    fn stringify_rejects_unknown_field() {
        let t = stub_template();
        let args = json!({"title": "x", "subtitle": "oops"});
        let err = stringify_args(&t, &args).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(ref m) if m.contains("unknown field")));
    }

    #[test]
    fn stringify_rejects_missing_required() {
        let t = stub_template();
        let args = json!({"draft": false});
        let err = stringify_args(&t, &args).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(ref m) if m.contains("missing required")));
    }

    #[test]
    fn stringify_rejects_wrong_type() {
        let t = stub_template();
        let args = json!({"title": 42});
        let err = stringify_args(&t, &args).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(ref m) if m.contains("expects string")));
    }

    /// Round-trip a value document to its stored bytes for assertions.
    fn data_bytes(args: &Value) -> Value {
        serde_json::from_slice(&serialize_data(&data_value(args))).unwrap()
    }

    #[test]
    fn data_embeds_json_string_fields_as_nested_json() {
        // The presentation passes its whole deck as a JSON *string*; the
        // stored data must hold it as a real nested object, not an
        // escaped blob, so JSON Patch can address `/deck/slides/...`.
        let args = json!({
            "deck": "{\"deck_title\":\"Hi\",\"slides\":[{\"layout\":\"cover\"}]}",
            "theme": "dark"
        });
        let parsed = data_bytes(&args);
        assert_eq!(parsed["deck"]["deck_title"], "Hi");
        assert_eq!(parsed["deck"]["slides"][0]["layout"], "cover");
        assert_eq!(parsed["theme"], "dark");
    }

    #[test]
    fn data_leaves_plain_prose_untouched() {
        // A letter body that happens to look number-ish must stay a
        // string — we only reinterpret `{`/`[`-leading values.
        let args = json!({"subject": "Re: 42", "body": "42", "draft": true});
        let parsed = data_bytes(&args);
        assert_eq!(parsed["body"], "42"); // string, not the number 42
        assert_eq!(parsed["subject"], "Re: 42");
        assert_eq!(parsed["draft"], true);
    }

    #[test]
    fn data_keeps_malformed_json_string_as_string() {
        // Opens with `{` but isn't valid JSON → left as the original
        // string rather than dropped.
        let parsed = data_bytes(&json!({"deck": "{not valid json"}));
        assert_eq!(parsed["deck"], "{not valid json");
    }

    #[test]
    fn inputs_from_data_restringifies_embedded_objects() {
        // The edit round-trip: stored data has `deck` as a nested object;
        // inputs_from_data must hand typst the compact JSON *string* it
        // expects on `--input deck=…`, and pass plain strings through.
        let t = deck_template();
        let data = json!({
            "deck": {"deck_title": "Hi", "slides": [{"layout": "cover"}]},
            "theme": "dark"
        });
        let inputs = inputs_from_data(&t, &data).unwrap();
        let deck = inputs.iter().find(|(k, _)| k == "deck").unwrap();
        // Re-parses to the same object (compact form, exact whitespace
        // doesn't matter — typst parses it back).
        let reparsed: Value = serde_json::from_str(&deck.1).unwrap();
        assert_eq!(reparsed["deck_title"], "Hi");
        assert_eq!(reparsed["slides"][0]["layout"], "cover");
        let theme = inputs.iter().find(|(k, _)| k == "theme").unwrap();
        assert_eq!(theme.1, "dark"); // plain string, not re-quoted
    }

    #[test]
    fn inputs_from_data_rejects_unknown_and_missing() {
        let t = deck_template();
        // Unknown key (e.g. a patch added a stray field).
        let err = inputs_from_data(&t, &json!({"deck": {}, "bogus": 1})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(ref m) if m.contains("unknown field")));
        // Required `deck` removed by an over-eager patch.
        let err = inputs_from_data(&t, &json!({"theme": "dark"})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(ref m) if m.contains("required field")));
    }

    #[test]
    fn id_is_prefixed_and_stable() {
        let tool = TypstRenderTool::new(std::sync::Arc::new(stub_template()), None);
        assert_eq!(tool.id(), "typst_stub");
        // Calling id() twice must return the same pointer — leaked
        // strings are pinned for the process lifetime.
        let a = tool.id().as_ptr();
        let b = tool.id().as_ptr();
        assert_eq!(a, b);
    }

    #[test]
    fn edit_tool_id_suffixes_render_id() {
        let tool = TypstEditTool::new(std::sync::Arc::new(stub_template()), None);
        assert_eq!(tool.id(), "typst_stub_edit");
        // Only `base` is required now — `patch` and `find`/`replace` are
        // alternative editing modes the run() validates. The schema still
        // points at the render tool and offers all three controls.
        let def = tool.schema();
        let params = serde_json::to_value(&def.function.parameters).unwrap();
        let required = params["required"].as_array().unwrap();
        assert_eq!(required, &[json!("base")]);
        let props = &params["properties"];
        for key in ["base", "patch", "find", "replace", "preview_page"] {
            assert!(props.get(key).is_some(), "missing property `{key}`");
        }
        assert!(def.function.description.contains("typst_stub"));
    }

    #[test]
    fn read_tool_id_suffixes_render_id() {
        let tool = TypstReadTool::new(std::sync::Arc::new(stub_template()));
        assert_eq!(tool.id(), "typst_stub_read");
        let def = tool.schema();
        let params = serde_json::to_value(&def.function.parameters).unwrap();
        assert_eq!(params["required"].as_array().unwrap(), &[json!("base")]);
        // Description points back at the render/edit siblings.
        assert!(def.function.description.contains("typst_stub"));
        assert!(def.function.description.contains("typst_stub_edit"));
    }

    #[test]
    fn render_schema_offers_preview_page() {
        let tool = TypstRenderTool::new(std::sync::Arc::new(stub_template()), None);
        let params = serde_json::to_value(&tool.schema().function.parameters).unwrap();
        assert!(params["properties"]["preview_page"].is_object());
        // preview_page is a control, never a required field.
        let required = params["required"].as_array().unwrap();
        assert!(!required.contains(&json!("preview_page")));
    }

    #[test]
    fn replace_in_strings_counts_and_rewrites_nested_values() {
        let mut v = json!({
            "deck": {
                "slides": [
                    {"quote": "The AI is a co-pilot — not a replacement."},
                    {"title": "co-pilot duties"},
                ]
            },
            "theme": "dark",
        });
        let n = replace_in_strings(&mut v, "co-pilot", "tool");
        assert_eq!(n, 2);
        assert_eq!(
            v["deck"]["slides"][0]["quote"],
            "The AI is a tool — not a replacement."
        );
        assert_eq!(v["deck"]["slides"][1]["title"], "tool duties");
        assert_eq!(v["theme"], "dark");
        // No match → zero, unchanged.
        assert_eq!(replace_in_strings(&mut v, "nope", "x"), 0);
    }

    // --- auto-escape-on-`@` retry -----------------------------------------

    #[test]
    fn escape_unescaped_ats_str_handles_emails_and_idempotency() {
        // The dominant case: a bare email gets its `@` escaped, once.
        assert_eq!(
            escape_unescaped_ats_str("john@acme.com"),
            ("john\\@acme.com".to_string(), 1)
        );
        // Already escaped → left alone (idempotent, so re-running is a no-op).
        assert_eq!(
            escape_unescaped_ats_str("john\\@acme.com"),
            ("john\\@acme.com".to_string(), 0)
        );
        // Multiple bare `@` → each escaped.
        assert_eq!(
            escape_unescaped_ats_str("a@b@c"),
            ("a\\@b\\@c".to_string(), 2)
        );
        // An escaped backslash (`\\`) does NOT escape a following `@` — the two
        // backslashes cancel, so the `@` is bare and must be escaped.
        assert_eq!(
            escape_unescaped_ats_str("\\\\@x"),
            ("\\\\\\@x".to_string(), 1)
        );
        // No `@` → untouched.
        assert_eq!(
            escape_unescaped_ats_str("plain text"),
            ("plain text".to_string(), 0)
        );
    }

    #[test]
    fn escape_unescaped_ats_walks_nested_values_and_stays_valid_json() {
        // Mirrors the presentation case: the deck is a nested object with `@`
        // inside a markup field AND a raw title. Both string values get the
        // `@` escaped (the count is the total), keys and numbers are untouched.
        let v = json!({
            "deck": {
                "slides": [{"body": "ping me @ a@b.com", "title": "Sync @ 5"}],
                "count": 3
            }
        });
        let (out, n) = escape_unescaped_ats(&v);
        assert_eq!(n, 3, "two in body + one in title");
        assert_eq!(out["deck"]["slides"][0]["body"], "ping me \\@ a\\@b.com");
        assert_eq!(out["deck"]["slides"][0]["title"], "Sync \\@ 5");
        assert_eq!(out["deck"]["count"], 3); // number untouched
        // The whole point of escaping at the Value layer: re-serializing to the
        // `deck=<json>` --input string stays valid JSON (serde emits `\\@`), so
        // typst's `json()` still parses it and the eval'd markup sees `\@`.
        let s = serde_json::to_string(&out).unwrap();
        let reparsed: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            reparsed["deck"]["slides"][0]["body"],
            "ping me \\@ a\\@b.com"
        );
    }

    #[test]
    fn is_unescaped_at_error_matches_only_the_label_signature() {
        assert!(is_unescaped_at_error(
            "error: label `<acme.com>` does not exist in the document"
        ));
        // Unrelated compile failures must NOT trigger the escape-retry.
        assert!(!is_unescaped_at_error("error: unknown variable: foo"));
        assert!(!is_unescaped_at_error("error: expected expression"));
    }

    #[test]
    fn is_template_typst_chip_matches_only_this_groups_visuals() {
        let t = deck_template(); // output_basename = "deck"
        assert!(is_template_typst_chip(&t, "deck.pdf"));
        assert!(is_template_typst_chip(&t, "deck.png"));
        assert!(is_template_typst_chip(&t, "deck.pptx"));
        assert!(is_template_typst_chip(&t, "deck-2.pdf"));
        assert!(is_template_typst_chip(&t, "deck-17.png"));
        // The hidden edit base must survive a supersede.
        assert!(!is_template_typst_chip(&t, "deck.json"));
        assert!(!is_template_typst_chip(&t, "deck-2.json"));
        // Unrelated uploads and other templates' files stay.
        assert!(!is_template_typst_chip(&t, "report.pdf"));
        assert!(!is_template_typst_chip(&t, "deckster.pdf"));
        assert!(!is_template_typst_chip(&t, "deck-.pdf"));
        assert!(!is_template_typst_chip(&t, "deck-x.pdf"));
    }

    #[test]
    fn take_preview_page_defaults_and_validates() {
        let mut empty = Map::new();
        assert_eq!(take_preview_page(&mut empty).unwrap(), 1);
        let mut five = json!({"preview_page": 5}).as_object().unwrap().clone();
        assert_eq!(take_preview_page(&mut five).unwrap(), 5);
        // Removed so it isn't later mistaken for a template field.
        assert!(!five.contains_key("preview_page"));
        let mut zero = json!({"preview_page": 0}).as_object().unwrap().clone();
        assert!(take_preview_page(&mut zero).is_err());
        let mut bad = json!({"preview_page": "two"}).as_object().unwrap().clone();
        assert!(take_preview_page(&mut bad).is_err());
    }

    #[test]
    fn schema_advertises_fields_but_leaves_required_empty() {
        // Since a render can be driven inline OR from a canvas `document_id`,
        // the schema advertises every field in `properties` but keeps the
        // top-level `required` empty (either-or input); requiredness for the
        // inline path is enforced at runtime by `stringify_args`.
        let tool = TypstRenderTool::new(std::sync::Arc::new(stub_template()), None);
        let def = tool.schema();
        let params = serde_json::to_value(&def.function.parameters).unwrap();
        assert!(
            params["required"].as_array().unwrap().is_empty(),
            "required must be empty: {params}"
        );
        let props = &params["properties"];
        assert!(
            props.get("title").is_some(),
            "title field advertised: {params}"
        );
    }

    /// Template with a `deck` JSON-string field + an optional `theme`,
    /// mirroring the presentation manifest for round-trip tests.
    fn deck_template() -> Template {
        Template {
            id: "deck".into(),
            title: "Deck".into(),
            description: "deck".into(),
            output_basename: "deck".into(),
            fields: vec![
                Field {
                    name: "deck".into(),
                    ty: FieldType::String,
                    required: true,
                    description: "the deck json".into(),
                    default_from: None,
                },
                Field {
                    name: "theme".into(),
                    ty: FieldType::String,
                    required: false,
                    description: "theme".into(),
                    default_from: None,
                },
            ],
            root: PathBuf::from("/dev/null"),
            source_file: "template.typ".into(),
            pptx: None,
            docx: None,
        }
    }

    /// Template with two identity-backed fields, like the letter's
    /// sender_name / sender_email.
    fn identity_template() -> Template {
        Template {
            id: "id".into(),
            title: "Id".into(),
            description: "id".into(),
            output_basename: "id".into(),
            fields: vec![
                Field {
                    name: "sender_name".into(),
                    ty: FieldType::String,
                    required: false,
                    description: "from".into(),
                    default_from: Some(DefaultSource::UserName),
                },
                Field {
                    name: "sender_email".into(),
                    ty: FieldType::String,
                    required: false,
                    description: "email".into(),
                    default_from: Some(DefaultSource::UserEmail),
                },
            ],
            root: PathBuf::from("/dev/null"),
            source_file: "template.typ".into(),
            pptx: None,
            docx: None,
        }
    }

    fn me() -> Identity {
        Identity {
            name: Some("Jane Doe".into()),
            email: Some("jane.doe@example.com".into()),
        }
    }

    #[test]
    fn defaults_fill_omitted_identity_fields() {
        let t = identity_template();
        let mut args = Map::new();
        assert!(wants_identity(&t, &args));
        apply_identity_defaults(&t, &mut args, &me());
        assert_eq!(args["sender_name"], json!("Jane Doe"));
        assert_eq!(args["sender_email"], json!("jane.doe@example.com"));
    }

    #[test]
    fn explicit_name_suppresses_the_whole_identity_group() {
        let t = identity_template();
        // Writing on someone else's behalf: the model sets the name only.
        // The email must NOT be backfilled with the signed-in user's — name
        // and email come as a unit, so the omitted one stays null rather
        // than mismatching (one person's name, another's email).
        let mut args = json!({"sender_name": "John Roe"})
            .as_object()
            .unwrap()
            .clone();
        assert!(!wants_identity(&t, &args)); // group claimed → no DB read needed
        apply_identity_defaults(&t, &mut args, &me());
        assert_eq!(args["sender_name"], json!("John Roe"));
        assert!(!args.contains_key("sender_email"));
    }

    #[test]
    fn explicit_email_also_suppresses_name_default() {
        let t = identity_template();
        let mut args = json!({"sender_email": "john.roe@example.com"})
            .as_object()
            .unwrap()
            .clone();
        assert!(!wants_identity(&t, &args));
        apply_identity_defaults(&t, &mut args, &me());
        assert!(!args.contains_key("sender_name"));
        assert_eq!(args["sender_email"], json!("john.roe@example.com"));
    }

    #[test]
    fn blank_model_value_is_treated_as_omitted() {
        let t = identity_template();
        let mut args = json!({"sender_name": "   "}).as_object().unwrap().clone();
        assert!(wants_identity(&t, &args));
        apply_identity_defaults(&t, &mut args, &me());
        assert_eq!(args["sender_name"], json!("Jane Doe"));
    }

    #[test]
    fn missing_identity_leaves_field_unset() {
        let t = identity_template();
        let mut args = Map::new();
        let blank = Identity {
            name: None,
            email: Some("  ".into()),
        };
        apply_identity_defaults(&t, &mut args, &blank);
        assert!(!args.contains_key("sender_name"));
        assert!(!args.contains_key("sender_email"));
    }

    #[test]
    fn wants_identity_false_when_model_supplied_all() {
        let t = identity_template();
        let args = json!({"sender_name": "A", "sender_email": "a@b.c"})
            .as_object()
            .unwrap()
            .clone();
        assert!(!wants_identity(&t, &args));
    }

    #[test]
    fn wants_identity_false_for_template_without_default_from() {
        // The plain stub has no default_from fields → never triggers a DB read.
        let t = stub_template();
        let args = Map::new();
        assert!(!wants_identity(&t, &args));
    }

    // --- PPTX export -------------------------------------------------------

    #[test]
    fn pptx_script_carries_the_recipe_and_font() {
        let s = pptx_script("template.typ", Some("Urbanist"));
        assert!(s.contains("typ2pptx template.typ --root"), "{s}");
        assert!(s.contains("--detect-paragraphs"), "{s}");
        assert!(s.contains("TYPST_FONT_PATHS"), "{s}");
        assert!(s.contains(r#"FONT = "Urbanist""#), "{s}");
        assert!(s.contains(r#"typeface="Consolas""#), "{s}");
        assert!(s.contains("/work/presentation.pptx"), "{s}");
    }

    #[test]
    fn pptx_script_font_none_disables_swap() {
        let s = pptx_script("deck.typ", None);
        assert!(s.contains("FONT = None"), "{s}");
        assert!(s.contains("typ2pptx deck.typ --root"), "{s}");
    }

    #[test]
    fn build_bundle_zip_injects_deck_and_skips_on_disk_sample() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("template.typ"), b"= deck").unwrap();
        std::fs::create_dir(root.join("fonts")).unwrap();
        std::fs::write(root.join("fonts").join("Brand.ttf"), b"FONTBYTES").unwrap();
        // An on-disk sample deck.json that MUST be replaced by the real deck.
        std::fs::write(root.join("deck.json"), b"{\"stale\":true}").unwrap();

        let deck = br#"{"deck_title":"Fresh"}"#;
        let zip_bytes = build_bundle_zip(root, "deck.json", deck, &[]).unwrap();

        let mut zr = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).unwrap();
        let mut names: Vec<String> = (0..zr.len())
            .map(|i| zr.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["deck.json", "fonts/Brand.ttf", "template.typ"]);
        // The injected deck.json carries the fresh content, not the sample.
        use std::io::Read as _;
        let mut s = String::new();
        zr.by_name("deck.json")
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert!(s.contains("Fresh") && !s.contains("stale"), "{s}");
    }

    #[test]
    fn build_bundle_zip_carries_staged_uploads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("template.typ"), b"= deck").unwrap();
        let staged = vec![("uploads/u0_pic.png".to_string(), b"PNGBYTES".to_vec())];
        let zip_bytes = build_bundle_zip(root, "deck.json", b"{}", &staged).unwrap();
        let mut zr = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).unwrap();
        let names: Vec<String> = (0..zr.len())
            .map(|i| zr.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "uploads/u0_pic.png"), "{names:?}");
    }

    // --- `att:` image-ref staging -----------------------------------------

    #[test]
    fn collect_att_tokens_finds_nested_refs_and_dedups() {
        let data = json!({
            "deck": {
                "slides": [
                    {"layout": "media", "image": "att:T1/slide1_img1.png"},
                    {"layout": "cover", "bg_image": "assets/img/grainient.jpg"},
                    {"layout": "gallery", "items": [
                        {"image": "att:T1/slide3_img1.jpg"},
                        {"image": "att:T1/slide1_img1.png"}
                    ]}
                ]
            }
        });
        let mut out = Vec::new();
        collect_att_tokens(&data, &mut out);
        out.sort();
        // The on-disk path is NOT collected; the duplicate ref appears once.
        assert_eq!(
            out,
            vec![
                "att:T1/slide1_img1.png".to_string(),
                "att:T1/slide3_img1.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn sanitize_component_strips_separators_and_traversal() {
        assert_eq!(sanitize_component("slide1_img1.png"), "slide1_img1.png");
        // Any path separators / traversal collapse to a single safe component.
        assert_eq!(sanitize_component("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_component("a/b/c.PNG"), "c.PNG");
        assert_eq!(sanitize_component("weird name!@#.jpg"), "weird_name___.jpg");
        assert_eq!(sanitize_component("..."), "img");
    }

    #[test]
    fn apply_reps_rewrites_refs_in_strings_and_values() {
        let reps = vec![
            ("att:T1/a.png".to_string(), "uploads/u0_a.png".to_string()),
            ("att:T1/b.jpg".to_string(), "uploads/u1_b.jpg".to_string()),
        ];
        // Raw string (as it appears in a `--input deck=<json>` value).
        let s = r#"{"image":"att:T1/a.png","bg":"att:T1/b.jpg"}"#;
        let got = apply_replacements(s, &reps);
        assert_eq!(
            got,
            r#"{"image":"uploads/u0_a.png","bg":"uploads/u1_b.jpg"}"#
        );
        // Nested Value round-trip keeps structure, rewrites only the refs.
        let data = json!({"slides": [{"image": "att:T1/a.png"}, {"x": "keep"}]});
        let out = apply_reps_to_value(&data, &reps).unwrap();
        assert_eq!(out["slides"][0]["image"], "uploads/u0_a.png");
        assert_eq!(out["slides"][1]["x"], "keep");
        // No reps → unchanged clone.
        assert_eq!(apply_reps_to_value(&data, &[]).unwrap(), data);
    }

    #[test]
    fn build_staging_root_overlays_template_with_uploads() {
        let tpl = tempfile::tempdir().unwrap();
        let root = tpl.path();
        std::fs::write(root.join("template.typ"), b"= deck").unwrap();
        std::fs::create_dir(root.join("assets")).unwrap();
        std::fs::write(root.join("assets").join("logo.svg"), b"<svg/>").unwrap();

        let staged = vec![("uploads/u0_pic.png".to_string(), b"PIX".to_vec())];
        let staging = build_staging_root(root, &staged).unwrap();
        let sp = staging.path();
        // Template files copied through (including nested dirs)...
        assert_eq!(std::fs::read(sp.join("template.typ")).unwrap(), b"= deck");
        assert_eq!(
            std::fs::read(sp.join("assets").join("logo.svg")).unwrap(),
            b"<svg/>"
        );
        // ...and the staged upload written under uploads/.
        assert_eq!(
            std::fs::read(sp.join("uploads").join("u0_pic.png")).unwrap(),
            b"PIX"
        );
        // The original template dir is untouched (no uploads/ leaked in).
        assert!(!root.join("uploads").exists());
    }

    #[test]
    fn pptx_tool_id_suffixes_render_id_and_requires_base() {
        let cfg = std::sync::Arc::new(gateway_core::server::config::SandboxConfig {
            enabled: true,
            runner_url: "http://127.0.0.1:1".into(),
            timeout_secs: 30,
            max_artifact_bytes: 1024,
        });
        let sandbox = SandboxClient::new(cfg, "http://localhost".into());
        let tool = TypstPptxTool::new(std::sync::Arc::new(stub_template()), sandbox);
        assert_eq!(tool.id(), "typst_stub_pptx");
        let def = tool.schema();
        let params = serde_json::to_value(&def.function.parameters).unwrap();
        assert_eq!(params["required"], json!(["base"]));
    }

    // --- deck-as-canvas (#13) ---------------------------------------------

    #[test]
    fn render_schema_advertises_canvas_document_source() {
        // The render tool offers `document_id` (+ `version`) as an alternative
        // data source, and drops the hard `required` list so the input is
        // either-or (inline fields OR a canvas document) — runtime enforces.
        let tool = TypstRenderTool::new(std::sync::Arc::new(deck_template()), None);
        let def = tool.schema();
        let params = serde_json::to_value(&def.function.parameters).unwrap();
        let props = &params["properties"];
        assert!(
            props.get("document_id").is_some(),
            "document_id prop: {params}"
        );
        assert!(props.get("version").is_some(), "version prop: {params}");
        // The template field is still advertised, just not forced.
        assert!(
            props.get("deck").is_some(),
            "deck prop still present: {params}"
        );
        assert_eq!(
            params["required"].as_array().unwrap(),
            &Vec::<Value>::new(),
            "required must be empty (either-or input): {params}"
        );
    }

    #[test]
    fn docx_script_carries_the_html_pandoc_recipe_and_font() {
        let s = docx_script("template.typ", Some("Urbanist"));
        // The compile command is a python argv list: "typst","compile",…,"--format","html".
        assert!(s.contains("\"typst\""), "{s}");
        assert!(s.contains("\"--format\"") && s.contains("\"html\""), "{s}");
        assert!(s.contains("pandoc"), "{s}");
        assert!(s.contains("FONT='Urbanist'"), "{s}");
        assert!(s.contains("SRC='template.typ'"), "{s}");
        // embedding path present (obfuscated .odttf)
        assert!(s.contains("embedRegular") && s.contains("odttf"), "{s}");
        // ends leaving the artifact at /work
        assert!(s.contains(&format!("/work/{DOCX_OUT}")), "{s}");
        // also emits the bonus .odt
        assert!(s.contains("\"odt\"") && s.contains(ODT_OUT), "{s}");
    }

    #[test]
    fn docx_script_without_font_skips_embedding_branch() {
        // No font → FONT='' → the python `font` is None → no embed.
        let s = docx_script("template.typ", None);
        assert!(s.contains("FONT=''"), "{s}");
    }

    #[test]
    fn inputs_from_data_accepts_a_canvas_field_map() {
        // A canvas JSON document holds the field map (deck object under the
        // `deck` key) — exactly what a doc-sourced render parses and feeds to
        // inputs_from_data. Prove that shape validates and the deck round-trips
        // to a compact `--input deck=<json>` string.
        let t = deck_template();
        let data = json!({
            "deck": {"deck_title": "Q3", "slides": [{"layout": "cover", "title": "Hi"}]}
        });
        let inputs = inputs_from_data(&t, &data).expect("canvas field map should validate");
        let deck = inputs
            .iter()
            .find(|(k, _)| k == "deck")
            .expect("deck input");
        // Value re-serialized as compact JSON, ready for `typst --input`.
        assert!(
            deck.1.contains("\"slides\""),
            "deck json preserved: {}",
            deck.1
        );
        assert!(
            deck.1.starts_with('{'),
            "deck is a JSON object string: {}",
            deck.1
        );

        // A canvas doc missing the required `deck` field is rejected (the
        // runtime guard behind the empty schema `required`).
        let bad = json!({"theme": "dark"});
        assert!(
            inputs_from_data(&t, &bad).is_err(),
            "missing required deck must error"
        );
    }
}
