// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use super::*;

#[derive(Deserialize)]
pub(crate) struct ExcalidrawArgs {
    /// Inline `.excalidraw` scene JSON authored by the model. Wins over
    /// `attachment_id` when both are given.
    #[serde(default)]
    scene: Option<String>,
    /// `<turn>/<file>` id of an uploaded `.excalidraw`/`.json` scene to
    /// convert instead of authoring one inline.
    #[serde(default)]
    attachment_id: Option<String>,
    #[serde(default = "default_excalidraw_format")]
    format: ImageFormat,
    #[serde(default)]
    filename: Option<String>,
}

pub(crate) fn default_excalidraw_format() -> ImageFormat {
    ImageFormat::Svg
}

/// Output formats excalirender (and `render_typst`) can emit. The
/// extension is inferred by the tool from `-o <name>.<ext>`.
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ImageFormat {
    Svg,
    Png,
    Pdf,
}

impl ImageFormat {
    pub(crate) fn ext(self) -> &'static str {
        match self {
            ImageFormat::Svg => "svg",
            ImageFormat::Png => "png",
            ImageFormat::Pdf => "pdf",
        }
    }
}

/// An uploaded file we'll treat as an Excalidraw scene: the `.excalidraw`
/// extension, or a `.json`/json-mime file (Excalidraw scenes are JSON).
pub(crate) fn is_excalidraw(a: &AttachmentRef) -> bool {
    let n = a.filename.to_ascii_lowercase();
    n.ends_with(".excalidraw") || n.ends_with(".json") || a.mime.contains("json")
}

/// Excalidraw `line`/`arrow` elements carry their geometry in `points`, which
/// must be an array of `[x, y]` pairs (`[[0,0],[0,90]]`). Models routinely emit
/// a *flat* number list (`[0,0,0,90]`) instead; excalirender then dies on the
/// whole scene with the opaque `Error: number is not iterable`. Reshape any
/// flat, even-length, all-numeric `points` array into pairs. A no-op on
/// already-nested points and on anything that isn't a flat number array, so
/// correct scenes (including real Excalidraw exports) pass through untouched.
pub(crate) fn normalize_excalidraw_points(scene: &mut Value) {
    let Some(elements) = scene.get_mut("elements").and_then(Value::as_array_mut) else {
        return;
    };
    for el in elements {
        let Some(points) = el.get_mut("points").and_then(Value::as_array_mut) else {
            continue;
        };
        if points.len() < 2 || points.len() % 2 != 0 || !points.iter().all(Value::is_number) {
            continue;
        }
        let paired: Vec<Value> = points
            .chunks_exact(2)
            .map(|pair| Value::Array(pair.to_vec()))
            .collect();
        *points = paired;
    }
}

pub struct RenderExcalidraw(pub Arc<SandboxClient>);

impl Tool for RenderExcalidraw {
    fn id(&self) -> &str {
        "render_excalidraw"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Render an Excalidraw diagram to an image and return it to the \
             user. Pass `scene` with the `.excalidraw` JSON you authored \
             (boxes, arrows, text, the hand-drawn sketch style) to GENERATE a \
             diagram, or `attachment_id` to CONVERT an `.excalidraw`/`.json` \
             scene the user uploaded. Output `format` is `svg` (default — \
             vector, best for slides and for embedding into a Typst document \
             via `render_typst`), `png`, or `pdf`. The rendered file is \
             attached to your reply. Rendering uses the real Excalidraw \
             fonts/look and needs no network.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "scene": {
                        "type": "string",
                        "description": "The Excalidraw scene as `.excalidraw` JSON \
                                        (the `{\"type\":\"excalidraw\",\"elements\":[…]}` \
                                        document). Provide this to generate a diagram. \
                                        For `arrow`/`line` elements, `points` must be an \
                                        array of `[x,y]` pairs (e.g. `[[0,0],[0,90]]`), \
                                        NOT a flat number list. Takes precedence over \
                                        `attachment_id`."
                    },
                    "attachment_id": {
                        "type": "string",
                        "description": "`<turn>/<file>` id of an uploaded \
                                        `.excalidraw`/`.json` scene to convert. \
                                        Used only when `scene` is omitted."
                    },
                    "format": {
                        "type": "string", "enum": ["svg", "png", "pdf"],
                        "description": "Output format. Default svg."
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
            let args: ExcalidrawArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{scene?, attachment_id?, format?, filename?}}: {e}"
                ))
            })?;

            // Resolve the scene bytes from whichever source was given. Inline
            // `scene` wins; otherwise convert an uploaded file. A model can
            // produce malformed JSON, so validate before shipping it to the
            // renderer — a clear InvalidArgs nudges it to fix the scene rather
            // than puzzling over excalirender's parser error.
            let (scene_bytes, source): (Vec<u8>, Value) = match args
                .scene
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(scene) => {
                    let mut v = serde_json::from_str::<Value>(scene).map_err(|e| {
                        ToolError::InvalidArgs(format!(
                            "`scene` is not valid JSON ({e}); pass a complete .excalidraw document"
                        ))
                    })?;
                    normalize_excalidraw_points(&mut v);
                    // Re-serialize the (possibly fixed) scene. Falls back to the
                    // original bytes only if serialization somehow fails — which
                    // can't happen for a value we just parsed.
                    let bytes =
                        serde_json::to_vec(&v).unwrap_or_else(|_| scene.as_bytes().to_vec());
                    (bytes, json!({"from": "scene"}))
                }
                None => {
                    let (att, bytes) = resolve_one_attachment(
                        &ctx,
                        args.attachment_id.as_deref(),
                        "Excalidraw (.excalidraw/.json) scene",
                        is_excalidraw,
                    )
                    .await?;
                    // Uploaded scenes come from real Excalidraw and are normally
                    // well-formed, but normalize defensively when they parse as
                    // JSON; ship the original bytes verbatim if they don't (the
                    // renderer reports the error in that case).
                    let bytes = serde_json::from_slice::<Value>(&bytes)
                        .ok()
                        .map(|mut v| {
                            normalize_excalidraw_points(&mut v);
                            v
                        })
                        .and_then(|v| serde_json::to_vec(&v).ok())
                        .unwrap_or(bytes);
                    (bytes, json!({"from": "attachment", "id": att.id}))
                }
            };

            let ext = args.format.ext();
            let stem = filename_stem(args.filename.as_deref(), "diagram");
            let out = format!("{stem}.{ext}");
            // The scene rides in as a fixed-name input file (never interpolated
            // into the command), so its content can't break out into the shell;
            // only the sanitized output name is templated.
            let code = format!("set -e\nexcalirender diagram.excalidraw -o {out:?}\n");
            let req = RunRequest {
                language: Language::Bash,
                code,
                files: vec![InputFile {
                    name: "diagram.excalidraw".into(),
                    content_b64: b64::encode(&scene_bytes),
                }],
                timeout_secs: None,
                network: false,
                container_id: None,
                keep_alive: false,
            };
            let mut out_val = self.0.execute(&ctx, req).await?;
            if let Some(obj) = out_val.as_object_mut() {
                obj.insert("source".into(), source);
            }
            Ok(out_val)
        })
    }
}

// ---------------------------------------------------------------------------
// render_typst — compile model-authored Typst (charts, diagrams, decks)
//
// The free-form companion to the fixed `typst_<id>` template tools: the model
// writes the Typst source itself. Two things make this the home for "turn
// data into a chart" and "put this diagram into a presentation":
//   - `@preview/gribouille` (a ggplot2-style Grammar-of-Graphics package) is
//     pre-cached in the image, so the source can `#import` it offline; and
//   - `attachments` are staged into `/work` first, so the source can
//     `image("diagram.svg")` a figure produced earlier (e.g. by
//     `render_excalidraw`) and bake it into the document.

#[derive(Deserialize)]
pub(crate) struct TypstArgs {
    /// The Typst document source. May `#import "@preview/gribouille:0.3.0": *`
    /// for charts and `image("<staged-name>")` any staged attachment.
    /// Alternative to `document_id`; exactly one must be given.
    #[serde(default)]
    source: Option<String>,
    /// Render from a `format: "typst"` canvas document instead of inline
    /// source — the iterate-in-canvas, render-on-demand loop (also dodges
    /// inline payload limits for large documents).
    #[serde(default)]
    document_id: Option<String>,
    /// With `document_id`: render a specific version (default: latest).
    #[serde(default)]
    version: Option<i64>,
    /// Figures/data to drop into `/work` before compiling, so `source` can
    /// reference them by name. Use the `name` override to control the
    /// filename the source must match (e.g. `image("diagram.svg")`).
    #[serde(default)]
    attachments: Vec<AttachmentArg>,
    #[serde(default = "default_typst_format")]
    format: ImageFormat,
    #[serde(default)]
    filename: Option<String>,
}

pub(crate) fn default_typst_format() -> ImageFormat {
    ImageFormat::Pdf
}

pub struct RenderTypst(pub Arc<SandboxClient>);

impl Tool for RenderTypst {
    fn id(&self) -> &str {
        "render_typst"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Compile a Typst document you write and return it to the user — \
             for charts, diagrams, and slide decks where you control the \
             layout. Two extras beyond plain Typst: (1) the \
             `@preview/gribouille` package is available offline for \
             ggplot2-style Grammar-of-Graphics charts. Its API is \
             grammar-of-graphics, NOT keyword args — use exactly this shape: \
             `#import \"@preview/gribouille:0.3.0\": *` then \
             `#plot(data: <table-or-builtin>, mapping: aes(x: \"col\", y: \"col\"), \
             layers: (geom-point(),))` (layers is a TUPLE; other geoms: \
             geom-line, geom-bar, geom-col; `penguins` is a built-in dataset; \
             read CSV with `csv(\"file.csv\")`). Do NOT write `plot(x: …, geom: …)`. \
             And (2) files listed in `attachments` are placed in the \
             working directory first, so your source can `image(\"diagram.svg\")` \
             a figure produced earlier — e.g. an `.svg` from `render_excalidraw` — \
             to embed it into a presentation. `format` is `pdf` (default, for \
             multi-page documents/decks), `png`, or `svg` (single page; use \
             `pdf` if the document has multiple pages). The result is attached \
             to your reply. For the company-branded letter/deck templates, use \
             the dedicated `typst_<template>` tools instead. For anything the \
             user will iterate on, draft in the document canvas FIRST and \
             render from there: prose in a `markdown` document, or the Typst \
             source itself in a `format: \"typst\"` document — then pass its \
             `document_id` here instead of `source` (edits + re-render keep \
             working on the same document, and large sources dodge inline \
             payload limits). Syntax notes: colors are `rgb(\"#1D4ED8\")` or \
             named (`blue`) — a bare `#1D4ED8` is NOT valid Typst. Emoji \
             render via the installed Noto Color Emoji fallback — just write \
             them in the text. No network is used.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": [],
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "The Typst document source to compile. Pass either \
                                        this or `document_id`, not both."
                    },
                    "document_id": {
                        "type": "string",
                        "description": "Render a `format: \"typst\"` canvas document \
                                        (from `create_document`) instead of inline source."
                    },
                    "version": {
                        "type": "integer",
                        "description": "With `document_id`: render this specific version \
                                        (default: latest)."
                    },
                    "attachments": {
                        "type": "array",
                        "description": "Figures/data to stage into the working directory \
                                        before compiling, so the source can reference them \
                                        by name (e.g. an .svg/.png to `image(\"…\")`, or a \
                                        .csv a gribouille chart reads).",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["id"],
                            "properties": {
                                "id": {"type": "string", "description": "Attachment id \
                                       `<turn>/<file>`, or just a filename from earlier in \
                                       this conversation (newest match wins)."},
                                "name": {"type": "string", "description": "Filename to give \
                                         the file in the working directory — match what the \
                                         source references, e.g. `diagram.svg`."}
                            }
                        }
                    },
                    "format": {
                        "type": "string", "enum": ["pdf", "png", "svg"],
                        "description": "Output format. Default pdf."
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
            let args: TypstArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{source? | document_id?, version?, attachments?, format?, \
                     filename?}}: {e}"
                ))
            })?;
            // Exactly one source of truth: inline `source` XOR a typst
            // canvas document.
            let (source, canvas) = match (&args.source, &args.document_id) {
                (Some(_), Some(_)) => {
                    return Err(ToolError::InvalidArgs(
                        "pass either `source` or `document_id`, not both".into(),
                    ));
                }
                (None, None) => {
                    return Err(ToolError::InvalidArgs(
                        "pass `source` (inline Typst) or `document_id` (a `format: \"typst\"` \
                         canvas document)"
                            .into(),
                    ));
                }
                (Some(s), None) => {
                    if s.trim().is_empty() {
                        return Err(ToolError::InvalidArgs("source must be non-empty".into()));
                    }
                    (s.clone(), None)
                }
                (None, Some(doc_id)) => {
                    use gateway_core::server::db::documents::{self, DocumentFormat};
                    let session_id = ctx.session_id.as_deref().ok_or_else(|| {
                        ToolError::Failed(
                            "canvas documents are only available inside a chat session".into(),
                        )
                    })?;
                    let (doc, ver) =
                        documents::get_version(&ctx.db, session_id, doc_id, args.version)
                            .await
                            .map_err(|e| {
                                ToolError::Failed(format!("reading canvas document: {e}"))
                            })?
                            .ok_or_else(|| {
                                ToolError::InvalidArgs(format!(
                                    "no canvas document `{doc_id}` (v{:?}) in this conversation",
                                    args.version
                                ))
                            })?;
                    // Deletion hides a document but leaves it resolvable, so
                    // rendering one has to be refused explicitly — otherwise a
                    // stale id from earlier in the turn silently produces a PDF
                    // from a document the user removed.
                    if doc.is_deleted() {
                        return Err(ToolError::InvalidArgs(format!(
                            "canvas document `{doc_id}` is deleted — call \
                             `undelete_document` first if you want to render it"
                        )));
                    }
                    if doc.format != DocumentFormat::Typst {
                        return Err(ToolError::InvalidArgs(format!(
                            "canvas document `{doc_id}` is `{}` — render_typst needs a \
                             `format: \"typst\"` document",
                            doc.format.as_str()
                        )));
                    }
                    (ver.content.clone(), Some((doc_id.clone(), ver.version)))
                }
            };

            // Stage any referenced figures/data into /work first, then add the
            // source as in.typ (so an explicit attachment named `in.typ` can't
            // clobber the program — the source is appended last and wins).
            let Staged {
                files: staged_files,
                staged,
                available,
                mut notes,
                documents: attachment_documents,
            } = stage_attachments(&ctx, &args.attachments).await?;
            let mut files = staged_files;
            // A canvas document listed among the figures is materialised too —
            // a deck that `#include`s a canvas-drafted section is a real case,
            // and silently dropping it would fail the compile with a missing
            // file the model can't see.
            let _ =
                super::stage_documents(&ctx, &attachment_documents, &mut files, &mut notes).await;
            files.push(InputFile {
                name: "in.typ".into(),
                content_b64: b64::encode(source.as_bytes()),
            });

            let ext = args.format.ext();
            let stem = filename_stem(args.filename.as_deref(), "document");
            let out = format!("{stem}.{ext}");
            // Only the sanitized output name is templated; the Typst source
            // rides in as a file, never interpolated into the command.
            let code = format!("set -e\ntypst compile in.typ {out:?}\n");
            let req = RunRequest {
                language: Language::Bash,
                code,
                files,
                timeout_secs: None,
                network: false,
                container_id: None,
                keep_alive: false,
            };
            let mut out_val = self.0.execute(&ctx, req).await?;
            augment_with_staging(&mut out_val, staged, available, notes);
            if let (Some((doc_id, ver)), Some(obj)) = (canvas, out_val.as_object_mut()) {
                obj.insert("canvas_document_id".into(), json!(doc_id));
                obj.insert("canvas_version".into(), json!(ver));
                obj.insert(
                    "canvas_note".into(),
                    json!(
                        "Rendered from the canvas document — to change it, edit the \
                         document and re-render with the SAME document_id."
                    ),
                );
            }
            Ok(out_val)
        })
    }
}

// ---------------------------------------------------------------------------
// Retrieval tool: read_sandbox_output — grep/head/tail/range over a stored
// large output (the searchable-object / pointers-as-context pattern).
