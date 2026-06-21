// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-template `typst_<id>` tool wrapper.
//!
//! Two tools per discovered template:
//!
//! - [`TypstRenderTool`] (`typst_<id>`) — the manifest's declared
//!   fields become its JSON schema; on invocation we hand the validated
//!   key/value pairs to [`crate::server::typst::compile`], then splice
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

use super::sandbox::{SandboxClient, b64};
use super::{Tool, ToolContext, ToolError, ToolFuture, ToolResult};
use crate::server::chat_attachments;
use crate::server::typst::{self, DefaultSource, FieldType, PptxExport, Template};

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

    fn schema(&self) -> ToolDef {
        let t = &self.template;
        let mut props = Map::new();
        let mut required: Vec<Value> = Vec::new();
        for f in &t.fields {
            props.insert(
                f.name.clone(),
                json!({
                    "type": f.ty.json_schema_name(),
                    "description": f.description,
                }),
            );
            if f.required {
                required.push(Value::String(f.name.clone()));
            }
        }
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
                 (the final document — the deliverable) and a page-1 PNG \
                 preview you can visually inspect (clicking it opens the \
                 PDF). The tool result also returns a `data_id` referencing \
                 the exact field values you supplied. To make a SMALL change \
                 afterwards — fix one headline, tweak one bullet — do NOT \
                 resend the whole input: call `{id}_edit` with that `data_id` \
                 and a tiny JSON Patch describing only what changes.",
                descr = t.description,
                base = t.output_basename,
                id = self.id(),
            ),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": properties,
                "required": required,
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
            if wants_identity(&template, &arg_map) {
                match crate::server::db::users::find_by_id(&ctx.db, &ctx.user_id).await {
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
    s3: &crate::server::config::S3Config,
    template: &Template,
    inputs: Vec<(String, String)>,
    data: &Value,
    sandbox: Option<&Arc<SandboxClient>>,
) -> ToolResult {
    let rendered = typst::compile(template, &inputs).await.map_err(|e| {
        // Compile errors become InvalidArgs so the model is nudged to
        // fix its input — typst's stderr usually names the offending
        // line / variable.
        use typst::CompileError;
        match e {
            CompileError::Failed(msg) => {
                ToolError::InvalidArgs(format!("typst compile failed:\n{msg}"))
            }
            other => ToolError::Failed(other.to_string()),
        }
    })?;

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

    let data_bytes = serialize_data(data);

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
        match convert_to_pptx(sandbox, template, cfg, data).await {
            Ok(bytes) => {
                match chat_attachments::upload(s3, turn_id, &pptx_name, PPTX_MIME, bytes).await {
                    Ok(out) => pptx_out = Some(out),
                    Err(e) => pptx_error = Some(format!("upload pptx: {e}")),
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, template = %template.id, "pptx export failed");
                pptx_error = Some(e.to_string());
            }
        }
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
    chunk.push_str("\n\n");
    chat::append_content(&ctx.db, turn_id, &chunk)
        .await
        .map_err(|e| ToolError::Failed(format!("persist markers: {e}")))?;

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
    Ok(result)
}

/// MIME for a `.pptx` (OOXML presentation).
const PPTX_MIME: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";

/// Single input file name carrying the zipped template + deck.
const BUNDLE_NAME: &str = "bundle.zip";
/// The lone `.pptx` the sandbox script leaves in `/work`.
const PPTX_OUT: &str = "presentation.pptx";

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
) -> Result<Vec<u8>, ToolError> {
    let deck = data.get(&cfg.data_field).ok_or_else(|| {
        ToolError::Failed(format!(
            "pptx export: data has no `{}` field to write as {}",
            cfg.data_field, cfg.data_file
        ))
    })?;
    let deck_bytes = serde_json::to_vec_pretty(deck)
        .map_err(|e| ToolError::Failed(format!("pptx export: serialize deck: {e}")))?;

    let bundle = build_bundle_zip(&template.root, &cfg.data_file, &deck_bytes)?;
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
fn build_bundle_zip(root: &Path, data_file: &str, deck_bytes: &[u8]) -> Result<Vec<u8>, ToolError> {
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
        zw.finish()
            .map_err(|e| ToolError::Failed(format!("pptx bundle: finish: {e}")))?;
    }
    Ok(buf)
}

/// The bash recipe run in the sandbox. All messy work happens in a
/// subdir; only the final `.pptx` is copied to `/work` so it is the sole
/// returned artifact. The optional `font` stamp corrects typ2pptx
/// writing the deck's font as `Consolas`.
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
import zipfile, glob, os, tempfile
FONT = {font_py}
d = tempfile.mkdtemp()
with zipfile.ZipFile("deck.pptx") as z:
    z.extractall(d)
if FONT:
    for f in glob.glob(os.path.join(d, "ppt", "slides", "*.xml")):
        s = open(f, encoding="utf-8").read()
        s = s.replace('typeface="Consolas"', 'typeface="%s"' % FONT)
        open(f, "w", encoding="utf-8").write(s)
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

    fn schema(&self) -> ToolDef {
        let render_id = format!("typst_{}", self.template.id);
        ToolDef::function(
            self.id(),
            format!(
                "Make a SMALL change to a document previously rendered by \
                 `{render_id}` and re-render it — WITHOUT resending the whole \
                 input. Give `base` (the `data_id` the render returned) and \
                 `patch` (an RFC 6902 JSON Patch array applied to the stored \
                 field values). Example — change the third slide's title: \
                 [{{\"op\":\"replace\",\"path\":\"/deck/slides/2/title\",\
                 \"value\":\"New headline\"}}]. Add an item with \
                 {{\"op\":\"add\",\"path\":\"/deck/slides/-\",\"value\":{{…}}}}, \
                 remove with {{\"op\":\"remove\",\"path\":\"/deck/slides/1\"}}. \
                 Returns a fresh PDF + preview and a new `data_id` you can \
                 patch again. Prefer this over re-calling `{render_id}` \
                 whenever most of the content is unchanged.",
            ),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["base", "patch"],
                "properties": {
                    "base": {
                        "type": "string",
                        "description": "The `data_id` returned by a previous \
                                        render/edit of this template, of the \
                                        form `<turn_id>/<file>.json`."
                    },
                    "patch": {
                        "type": "array",
                        "description": "RFC 6902 JSON Patch: an array of \
                                        operations applied in order. Paths are \
                                        JSON Pointers into the stored field \
                                        values (the `deck` object is addressable \
                                        as `/deck/...`).",
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

            let obj = args
                .as_object()
                .ok_or_else(|| ToolError::InvalidArgs("expected an object {base, patch}".into()))?;
            let base = obj.get("base").and_then(Value::as_str).ok_or_else(|| {
                ToolError::InvalidArgs(
                    "`base` (the data_id from the previous render) is required".into(),
                )
            })?;
            let patch = obj.get("patch").and_then(Value::as_array).ok_or_else(|| {
                ToolError::InvalidArgs("`patch` (an RFC 6902 JSON Patch array) is required".into())
            })?;

            // Fetch the prior render's data document (the patch base). It
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

            super::json_patch::apply(&mut data, patch)
                .map_err(|e| ToolError::InvalidArgs(format!("could not apply patch: {e}")))?;

            // Re-validate + re-stringify the patched data, then render and
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
            )
            .await
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

            let bytes = convert_to_pptx(&self.sandbox, &template, cfg, &data).await?;
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
const TYPST_EXTS: &[&str] = &["pdf", "png", "json", "pptx"];

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
    use crate::server::typst::Field;
    use std::path::PathBuf;

    fn stub_template() -> Template {
        Template {
            id: "stub".into(),
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
        // Schema requires base + patch and points at the render tool.
        let def = tool.schema();
        let params = serde_json::to_value(&def.function.parameters).unwrap();
        let required = params["required"].as_array().unwrap();
        assert!(required.contains(&json!("base")) && required.contains(&json!("patch")));
        assert!(def.function.description.contains("typst_stub"));
    }

    #[test]
    fn schema_lists_required_fields_only() {
        let tool = TypstRenderTool::new(std::sync::Arc::new(stub_template()), None);
        let def = tool.schema();
        let params = serde_json::to_value(&def.function.parameters).unwrap();
        let required = params["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "title");
    }

    /// Template with a `deck` JSON-string field + an optional `theme`,
    /// mirroring the presentation manifest for round-trip tests.
    fn deck_template() -> Template {
        Template {
            id: "deck".into(),
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
        }
    }

    /// Template with two identity-backed fields, like the letter's
    /// sender_name / sender_email.
    fn identity_template() -> Template {
        Template {
            id: "id".into(),
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
        let zip_bytes = build_bundle_zip(root, "deck.json", deck).unwrap();

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
    fn pptx_tool_id_suffixes_render_id_and_requires_base() {
        let cfg = std::sync::Arc::new(crate::server::config::SandboxConfig {
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
}
