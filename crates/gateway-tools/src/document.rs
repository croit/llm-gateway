// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! The document canvas tools — `create_document`, `edit_document`,
//! `read_document`, `list_documents`, `edit_document_section`,
//! `list_document_versions`, `restore_document_version`.
//!
//! Let the model build up a long document (a guide, a spec, a config) over
//! many turns and change one passage at a time, instead of regenerating
//! the whole thing each round. The document lives in the `documents` store
//! (see [`gateway_core::server::db::documents`]), not in the chat transcript, so
//! the model never has to hold the whole thing in context: it reads slices
//! and edits by anchor.
//!
//! `edit_document` dispatches on the document's format:
//!   * `markdown` / `text` / `html` — anchored find/replace
//!     ([`super::text_edit`]): a `find` snippet that must match exactly
//!     once, swapped for `replace`. An empty `find` appends.
//!   * `json` / `toml` — an RFC 6902 JSON Patch ([`super::json_patch`]),
//!     the same machinery the Typst edit tool uses; TOML is parsed to
//!     JSON, patched, and reserialised.
//!
//! Every edit appends a version, so the canvas keeps a scrubbable history.
//! On the chat path each create/edit pushes a live patch to the canvas
//! panel (see [`live_update`]); on other paths it's a silent store write.

use serde_json::Value;
use shared::api::ToolDef;

use crate::json_patch;
use crate::text_edit::{self, Edit};
use gateway_core::server::db::documents::{self, DocumentFormat, EditKind};
use gateway_core::server::tools::{Tool, ToolContext, ToolError, ToolFuture};

/// Largest document we accept (in bytes of content). Generous for a
/// long-form guide; guards against a single runaway tool call.
const MAX_DOC_BYTES: usize = 512 * 1024;

/// Default slice size `read_document` returns when the caller doesn't ask
/// for a section or grep — enough to inspect a chunk without dumping a
/// whole guide into context.
const DEFAULT_READ_BYTES: usize = 16 * 1024;

// ---------------------------------------------------------------------------
// Shared helpers

/// The session id + user id a canvas tool needs, or a clear error. The
/// canvas is a property of a chat conversation, so these tools refuse to
/// run off the chat path (proxy / bearer callers have no session to scope
/// documents to).
fn require_session(ctx: &ToolContext) -> Result<&str, ToolError> {
    ctx.session_id.as_deref().ok_or_else(|| {
        ToolError::Failed(
            "document tools are only available inside a chat session \
             (there's no conversation to attach the document to)"
                .into(),
        )
    })
}

/// Push the freshly-changed canvas to the live chat page, if anyone's
/// watching. Best-effort: off the chat path (no `chat_feedback`) or with
/// no live subscriber it's a no-op, and the panel renders on the next full
/// page load instead. Targets the always-present `#document-canvas-slot`
/// so the first `create_document` of a conversation has a morph target
/// even though the page loaded with no panel.
async fn live_update(ctx: &ToolContext, session_id: &str, active_id: &str) {
    let Some(fb) = ctx.chat_feedback.as_ref() else {
        return;
    };
    if fb.broadcast.receiver_count() == 0 {
        return;
    }
    // No live HTTP request here (this fires from tool execution, not a page
    // load) so there's no `lang` cookie to read — falls back to English,
    // same as `internal_error_html`/`forbidden_html` do for request-less
    // render paths. The panel picks up the viewer's real language on the
    // next full page load or nav-patch, both of which do derive it.
    let html = match gateway_core::server::document_canvas::render_canvas_html(
        &ctx.db,
        session_id,
        Some(active_id),
        None,
        session_core::i18n::Lang::En,
    )
    .await
    {
        Ok(Some(html)) => html,
        // Nothing to show or a transient read error — skip the live patch;
        // the next page load reconciles from the DB.
        _ => return,
    };
    // One Inject frame carrying three datastar events: (1) swap the canvas
    // panel, (2) reveal the header toggle (`hasCanvas`) on any device, and
    // (3) open the docked panel — but only on a wide viewport, so a mobile
    // edit never auto-covers the chat. The window event is desktop-gated in
    // JS; the shell's `data-on:gwcanvasopen__window` flips `canvasOpen`.
    let mut frame =
        session_core::chrome::sse_patch(Some("#document-canvas-slot"), Some("inner"), &html)
            .to_vec();
    frame.extend_from_slice(&session_core::chrome::sse_signals(
        r#"{"hasCanvas": true, "hasDocument": true}"#,
    ));
    frame.extend_from_slice(&session_core::chrome::sse_script(
        "if(window.innerWidth>=768){window.dispatchEvent(new CustomEvent('gwcanvasopen'))}",
    ));
    let _ = fb.broadcast.send(session_core::workers::TurnUpdate::Inject(
        std::sync::Arc::new(frame.into()),
    ));
}

/// Parse + reserialise a structured document through an RFC 6902 patch.
/// JSON and TOML both round-trip via `serde_json::Value`.
fn apply_structured_patch(
    format: DocumentFormat,
    content: &str,
    patch: &[Value],
) -> Result<String, ToolError> {
    let mut doc: Value = match format {
        DocumentFormat::Json => serde_json::from_str(content)
            .map_err(|e| ToolError::Failed(format!("stored document is not valid JSON: {e}")))?,
        DocumentFormat::Toml => toml::from_str(content)
            .map_err(|e| ToolError::Failed(format!("stored document is not valid TOML: {e}")))?,
        _ => unreachable!("apply_structured_patch only called for structured formats"),
    };
    json_patch::apply(&mut doc, patch)
        .map_err(|e| ToolError::InvalidArgs(format!("could not apply patch: {e}")))?;
    match format {
        DocumentFormat::Json => serde_json::to_string_pretty(&doc)
            .map_err(|e| ToolError::Failed(format!("reserialising JSON: {e}"))),
        DocumentFormat::Toml => toml::to_string_pretty(&doc).map_err(|e| {
            ToolError::Failed(format!(
                "the patched document can't be written back as TOML \
                 (TOML needs a table at the top level): {e}"
            ))
        }),
        _ => unreachable!(),
    }
}

/// Validate document content size up front so the error is about the input
/// rather than a DB write.
fn check_size(content: &str) -> Result<(), ToolError> {
    if content.len() > MAX_DOC_BYTES {
        return Err(ToolError::InvalidArgs(format!(
            "document content is {} bytes; the limit is {MAX_DOC_BYTES}. \
             Split it or trim it.",
            content.len()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// create_document

pub struct CreateDocument;

impl Tool for CreateDocument {
    fn id(&self) -> &str {
        "create_document"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Start a new long-form document that you can then grow and edit \
             one passage at a time across turns — WITHOUT rewriting the whole \
             thing each round. This is the DEFAULT home for anything \
             substantial you write for the user: a guide, a spec, an article, \
             a letter, a config file. It appears in a live canvas panel \
             beside the chat, every change becomes a new version (roll back \
             any time with `restore_document_version`), and several documents \
             can coexist in one conversation. Returns a `document_id`; pass \
             it to `edit_document` to change a passage, `read_document` to \
             re-read it, `list_documents` to find it later, and \
             `export_document` to hand the user a finished PDF/Word/PowerPoint. \
             Prefer this over pasting a growing document into chat each turn \
             or one-shot rendering (`generate_document`/`render_typst`) — \
             those can't be revised passage by passage.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["title", "format", "content"],
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Short human title for the document, e.g. \
                                        'Ceph RADOS Gateway Guide'."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["markdown", "text", "html", "json", "toml", "typst", "yaml"],
                        "description": "Content format. Use `markdown` for prose/guides \
                                        (the default choice); `typst` for layout-intensive \
                                        documents you will render (draft the source here, \
                                        compile via `render_typst`/`export_document`). \
                                        `json`/`toml` enable structured editing via JSON \
                                        Patch; `yaml` is edited as text so comments and \
                                        key order survive."
                    },
                    "content": {
                        "type": "string",
                        "description": "The initial full content. It's fine to start \
                                        small (e.g. a title + outline) and grow it with \
                                        `edit_document`."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let session_id = require_session(&ctx)?.to_string();
            let obj = args
                .as_object()
                .ok_or_else(|| ToolError::InvalidArgs("expected an object".into()))?;
            let title = obj
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ToolError::InvalidArgs("`title` is required".into()))?;
            let format_s = obj
                .get("format")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs("`format` is required".into()))?;
            let format = DocumentFormat::parse(format_s).ok_or_else(|| {
                ToolError::InvalidArgs(format!(
                    "unknown format `{format_s}`; use markdown, text, html, json, toml, typst, or yaml"
                ))
            })?;
            let content = obj
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs("`content` is required".into()))?;
            check_size(content)?;
            // For structured formats, reject content that isn't parseable up
            // front — otherwise the first edit would fail confusingly.
            if format.edit_kind() == EditKind::Structured {
                apply_structured_patch(format, content, &[])?;
            }

            let id = documents::new_id();
            documents::create(
                &ctx.db,
                &id,
                &session_id,
                &ctx.user_id,
                title,
                format,
                content,
                ctx.assistant_turn_id.as_deref(),
            )
            .await
            .map_err(|e| ToolError::Failed(format!("creating document: {e}")))?;

            live_update(&ctx, &session_id, &id).await;

            Ok(serde_json::json!({
                "document_id": id,
                "title": title,
                "format": format.as_str(),
                "version": 1,
                "chars": content.chars().count(),
                "status": "The document is now shown in the canvas panel. To change a \
                           passage later, call `edit_document` with this `document_id` — \
                           do NOT recreate or repaste the whole document."
            }))
        })
    }
}

// ---------------------------------------------------------------------------
// edit_document

pub struct EditDocument;

impl EditDocument {
    /// Build the `Vec<Edit>` for a text-format document from the `edits`
    /// array.
    fn parse_text_edits(edits: &[Value]) -> Result<Vec<Edit>, ToolError> {
        edits
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let o = e
                    .as_object()
                    .ok_or_else(|| ToolError::InvalidArgs(format!("edit {i} must be an object")))?;
                let find = o
                    .get("find")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let replace = o
                    .get("replace")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ToolError::InvalidArgs(format!(
                            "edit {i} needs a string `replace` (the text to insert)"
                        ))
                    })?
                    .to_string();
                Ok(Edit { find, replace })
            })
            .collect()
    }
}

impl Tool for EditDocument {
    fn id(&self) -> &str {
        "edit_document"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Change a document created by `create_document` WITHOUT resending \
             the whole thing — only the passages you touch. How `edits` works \
             depends on the document's format:\n\
             • markdown/text/html: `edits` is a list of {find, replace}. `find` \
             must appear EXACTLY ONCE in the document (copy it verbatim, \
             including whitespace); it's swapped for `replace`. Omit/empty \
             `find` to append `replace` to the end. Re-read with \
             `read_document` first if unsure of the exact text.\n\
             • json/toml: `edits` is an RFC 6902 JSON Patch — a list of \
             {op, path, value} where op is add/remove/replace/move/copy/test \
             and path is a JSON Pointer, e.g. \
             {\"op\":\"replace\",\"path\":\"/server/port\",\"value\":8080}.\n\
             Edits apply in order. Returns the new version number.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["document_id", "edits"],
                "properties": {
                    "document_id": {
                        "type": "string",
                        "description": "The id returned by `create_document`."
                    },
                    "edits": {
                        "type": "array",
                        "description": "For markdown/text/html: [{find, replace}]. \
                                        For json/toml: an RFC 6902 patch [{op, path, value?, from?}].",
                        "items": {
                            "type": "object",
                            "properties": {
                                "find": { "type": "string", "description": "Text format: unique anchor to replace (empty = append)." },
                                "replace": { "type": "string", "description": "Text format: replacement text." },
                                "op": { "type": "string", "enum": ["add", "remove", "replace", "move", "copy", "test"], "description": "Structured format: patch op." },
                                "path": { "type": "string", "description": "Structured format: JSON Pointer." },
                                "value": { "description": "Structured format: value for add/replace/test." },
                                "from": { "type": "string", "description": "Structured format: source pointer for move/copy." }
                            }
                        }
                    },
                    "summary": {
                        "type": "string",
                        "description": "Optional one-line note describing this revision (for the version history)."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let session_id = require_session(&ctx)?.to_string();
            let obj = args
                .as_object()
                .ok_or_else(|| ToolError::InvalidArgs("expected an object".into()))?;
            let document_id = obj
                .get("document_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs("`document_id` is required".into()))?;
            let edits = obj
                .get("edits")
                .and_then(Value::as_array)
                .ok_or_else(|| ToolError::InvalidArgs("`edits` (an array) is required".into()))?;
            if edits.is_empty() {
                return Err(ToolError::InvalidArgs("`edits` must not be empty".into()));
            }
            let summary = obj.get("summary").and_then(Value::as_str);

            let (doc, ver) = documents::get_version(&ctx.db, &session_id, document_id, None)
                .await
                .map_err(|e| ToolError::Failed(format!("reading document: {e}")))?
                .ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "no document `{document_id}` in this conversation — \
                         call `list_documents` to see the ids"
                    ))
                })?;

            let new_content = match doc.format.edit_kind() {
                EditKind::Text => {
                    let parsed = Self::parse_text_edits(edits)?;
                    text_edit::apply_edits(&ver.content, &parsed)
                        .map_err(|e| ToolError::InvalidArgs(e.to_string()))?
                }
                EditKind::Structured => apply_structured_patch(doc.format, &ver.content, edits)?,
            };
            check_size(&new_content)?;

            let updated = documents::append_version(
                &ctx.db,
                &session_id,
                document_id,
                &new_content,
                summary,
                ctx.assistant_turn_id.as_deref(),
            )
            .await
            .map_err(|e| ToolError::Failed(format!("saving edit: {e}")))?
            .ok_or_else(|| ToolError::Failed("document vanished mid-edit".into()))?;

            live_update(&ctx, &session_id, document_id).await;

            Ok(serde_json::json!({
                "document_id": document_id,
                "version": updated.current_ver,
                "edits_applied": edits.len(),
                "chars": new_content.chars().count(),
                "status": "Document updated and the canvas refreshed. The new content is \
                           stored — do NOT repeat it in your reply."
            }))
        })
    }
}

// ---------------------------------------------------------------------------
// read_document

pub struct ReadDocument;

impl Tool for ReadDocument {
    fn id(&self) -> &str {
        "read_document"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Read back a document's content (or a slice of it) so you can find \
             the exact text to anchor an `edit_document` on. Use `section` to \
             pull one markdown section by heading, `grep` to find matching \
             lines, or neither to read from the top. Pass `version` to read an \
             older revision. Reading a slice keeps long documents out of your \
             context.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["document_id"],
                "properties": {
                    "document_id": { "type": "string", "description": "The id from `create_document`." },
                    "version": { "type": "integer", "description": "Revision to read (default: latest)." },
                    "section": { "type": "string", "description": "Markdown/Typst only: return the section whose heading contains this text, up to the next heading of the same or higher level." },
                    "grep": { "type": "string", "description": "Return only lines containing this text (case-insensitive), with line numbers." },
                    "max_bytes": { "type": "integer", "description": "Cap the returned content size (default 16384). Ignored when `section`/`grep` are used." }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let session_id = require_session(&ctx)?.to_string();
            let obj = args
                .as_object()
                .ok_or_else(|| ToolError::InvalidArgs("expected an object".into()))?;
            let document_id = obj
                .get("document_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs("`document_id` is required".into()))?;
            let version = obj.get("version").and_then(Value::as_i64);

            let (doc, ver) = documents::get_version(&ctx.db, &session_id, document_id, version)
                .await
                .map_err(|e| ToolError::Failed(format!("reading document: {e}")))?
                .ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "no document `{document_id}` in this conversation"
                    ))
                })?;

            let total_chars = ver.content.chars().count();
            let mut result = serde_json::json!({
                "document_id": document_id,
                "title": doc.title,
                "format": doc.format.as_str(),
                "version": ver.version,
                "latest_version": doc.current_ver,
                "total_chars": total_chars,
            });

            if let Some(grep) = obj
                .get("grep")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let needle = grep.to_lowercase();
                let lines: Vec<String> = ver
                    .content
                    .lines()
                    .enumerate()
                    .filter(|(_, l)| l.to_lowercase().contains(&needle))
                    .map(|(i, l)| format!("{}: {l}", i + 1))
                    .collect();
                result["match_count"] = serde_json::json!(lines.len());
                result["content"] = serde_json::json!(lines.join("\n"));
                return Ok(result);
            }

            if let Some(section) = obj
                .get("section")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let Some(heading) = heading_level_fn(doc.format) else {
                    return Err(ToolError::InvalidArgs(format!(
                        "`section` works on markdown and typst documents; `{document_id}` is {}                          — use `grep` or read from the top instead",
                        doc.format.as_str()
                    )));
                };
                match extract_section(&ver.content, section, heading) {
                    Some(s) => {
                        result["section"] = serde_json::json!(section);
                        result["content"] = serde_json::json!(s);
                    }
                    None => {
                        result["content"] = serde_json::json!("");
                        result["note"] =
                            serde_json::json!(format!("no heading containing {section:?} found"));
                    }
                }
                return Ok(result);
            }

            let max_bytes = obj
                .get("max_bytes")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_READ_BYTES);
            let (slice, truncated) = clip(&ver.content, max_bytes);
            result["content"] = serde_json::json!(slice);
            if truncated {
                result["truncated"] = serde_json::json!(true);
                result["note"] = serde_json::json!(
                    "Content truncated. Use `grep` or `section`, or raise `max_bytes`, \
                     to read more."
                );
            }
            Ok(result)
        })
    }
}

// ---------------------------------------------------------------------------
// list_documents

pub struct ListDocuments;

impl Tool for ListDocuments {
    fn id(&self) -> &str {
        "list_documents"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "List the documents you've created in this conversation (id, title, \
             format, current version). Use it to find a `document_id` to edit or \
             read when you've lost track of it.",
            serde_json::json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, _args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let session_id = require_session(&ctx)?.to_string();
            let docs = documents::list_for_session(&ctx.db, &session_id)
                .await
                .map_err(|e| ToolError::Failed(format!("listing documents: {e}")))?;
            let items: Vec<Value> = docs
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "document_id": d.id,
                        "title": d.title,
                        "format": d.format.as_str(),
                        "version": d.current_ver,
                        "updated_at": d.updated_at.to_string(),
                    })
                })
                .collect();
            Ok(serde_json::json!({ "documents": items }))
        })
    }
}

// ---------------------------------------------------------------------------
// list_document_versions

pub struct ListDocumentVersions;

impl Tool for ListDocumentVersions {
    fn id(&self) -> &str {
        "list_document_versions"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "List a document's version history — every revision's number, edit \
             summary, timestamp, and size (newest first). Use it to find the \
             version to inspect (`read_document` with `version`) or to roll \
             back to (`restore_document_version`).",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["document_id"],
                "properties": {
                    "document_id": { "type": "string", "description": "The id from `create_document`." }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let session_id = require_session(&ctx)?.to_string();
            let document_id = args
                .get("document_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs("`document_id` is required".into()))?;
            let versions = documents::list_versions(&ctx.db, &session_id, document_id)
                .await
                .map_err(|e| ToolError::Failed(format!("listing versions: {e}")))?;
            if versions.is_empty() {
                return Err(ToolError::InvalidArgs(format!(
                    "no document `{document_id}` in this conversation — \
                     call `list_documents` to see the ids"
                )));
            }
            let items: Vec<Value> = versions
                .iter()
                .map(|v| {
                    serde_json::json!({
                        "version": v.version,
                        "summary": v.summary,
                        "created_at": v.created_at.to_string(),
                        "chars": v.chars,
                    })
                })
                .collect();
            Ok(serde_json::json!({
                "document_id": document_id,
                "current_version": items[0]["version"],
                "versions": items,
            }))
        })
    }
}

// ---------------------------------------------------------------------------
// restore_document_version

pub struct RestoreDocumentVersion;

impl Tool for RestoreDocumentVersion {
    fn id(&self) -> &str {
        "restore_document_version"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Roll a document back to an earlier version. Non-destructive: the \
             old content is re-applied as a NEW version, so the history stays \
             intact and even the rollback can be rolled back. Find version \
             numbers with `list_document_versions`.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["document_id", "version"],
                "properties": {
                    "document_id": { "type": "string", "description": "The id from `create_document`." },
                    "version": { "type": "integer", "description": "The version number to restore." }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let session_id = require_session(&ctx)?.to_string();
            let obj = args
                .as_object()
                .ok_or_else(|| ToolError::InvalidArgs("expected an object".into()))?;
            let document_id = obj
                .get("document_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs("`document_id` is required".into()))?;
            let version = obj.get("version").and_then(Value::as_i64).ok_or_else(|| {
                ToolError::InvalidArgs("`version` (an integer) is required".into())
            })?;

            let (doc, ver) =
                documents::get_version(&ctx.db, &session_id, document_id, Some(version))
                    .await
                    .map_err(|e| ToolError::Failed(format!("reading document: {e}")))?
                    .ok_or_else(|| {
                        ToolError::InvalidArgs(format!(
                            "no version {version} of document `{document_id}` in this \
                             conversation — call `list_document_versions` to see the history"
                        ))
                    })?;
            if version == doc.current_ver {
                return Err(ToolError::InvalidArgs(format!(
                    "version {version} is already the current version — nothing to restore"
                )));
            }

            let note = format!("Restored version {version}");
            let updated = documents::append_version(
                &ctx.db,
                &session_id,
                document_id,
                &ver.content,
                Some(&note),
                ctx.assistant_turn_id.as_deref(),
            )
            .await
            .map_err(|e| ToolError::Failed(format!("saving restore: {e}")))?
            .ok_or_else(|| ToolError::Failed("document vanished mid-restore".into()))?;

            live_update(&ctx, &session_id, document_id).await;

            Ok(serde_json::json!({
                "document_id": document_id,
                "restored_version": version,
                "version": updated.current_ver,
                "status": format!(
                    "Version {version} is now the current content (saved as version {}); \
                     the canvas refreshed.",
                    updated.current_ver
                ),
            }))
        })
    }
}

// ---------------------------------------------------------------------------
// edit_document_section (markdown)

pub struct EditDocumentSection;

impl Tool for EditDocumentSection {
    fn id(&self) -> &str {
        "edit_document_section"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Replace a whole section of a MARKDOWN or TYPST document by its \
             heading, or add it if no such heading exists — a robust \
             alternative to `edit_document` find/replace for larger \
             structural changes. Give the `document_id`, the `heading` to \
             target (matched by substring, case-insensitive), and the full \
             replacement `content` INCLUDING its heading line (markdown \
             \"## Installation\\n\\n…\", typst \"== Installation\\n\\n…\"). \
             The section spans from its heading to the next heading of the \
             same or higher level.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["document_id", "heading", "content"],
                "properties": {
                    "document_id": {"type": "string", "description": "The id from `create_document`."},
                    "heading": {"type": "string", "description": "Heading text to target, e.g. 'Installation' (substring match)."},
                    "content": {"type": "string", "description": "The full replacement section, including its own heading line. If no matching heading exists it's appended as a new section."}
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let session_id = require_session(&ctx)?.to_string();
            let obj = args
                .as_object()
                .ok_or_else(|| ToolError::InvalidArgs("expected an object".into()))?;
            let document_id = obj
                .get("document_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs("`document_id` is required".into()))?;
            let heading = obj
                .get("heading")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ToolError::InvalidArgs("`heading` is required".into()))?;
            let content = obj
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs("`content` is required".into()))?;

            let (doc, ver) = documents::get_version(&ctx.db, &session_id, document_id, None)
                .await
                .map_err(|e| ToolError::Failed(format!("reading document: {e}")))?
                .ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "no document `{document_id}` in this conversation"
                    ))
                })?;
            let Some(level) = heading_level_fn(doc.format) else {
                return Err(ToolError::InvalidArgs(format!(
                    "section edits work on markdown and typst documents; `{document_id}` is {}",
                    doc.format.as_str()
                )));
            };
            let existed = extract_section(&ver.content, heading, level).is_some();
            let new_content = replace_or_append_section(&ver.content, heading, content, level);
            check_size(&new_content)?;

            let note = if existed {
                format!("Replaced section matching {heading:?}")
            } else {
                format!("Added section {heading:?}")
            };
            let updated = documents::append_version(
                &ctx.db,
                &session_id,
                document_id,
                &new_content,
                Some(&note),
                ctx.assistant_turn_id.as_deref(),
            )
            .await
            .map_err(|e| ToolError::Failed(format!("saving edit: {e}")))?
            .ok_or_else(|| ToolError::Failed("document vanished mid-edit".into()))?;

            live_update(&ctx, &session_id, document_id).await;

            Ok(serde_json::json!({
                "document_id": document_id,
                "version": updated.current_ver,
                "action": if existed { "replaced" } else { "added" },
                "chars": new_content.chars().count(),
            }))
        })
    }
}

// ---------------------------------------------------------------------------
// content slicing helpers

/// First `max_bytes` of `s` on a char boundary; returns whether it was
/// clipped.
fn clip(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// The ATX heading level of a markdown line (`## X` → 2), or `None` when
/// the line isn't a heading. A run of `#` must be followed by a space.
fn markdown_heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes > 0 && line.chars().nth(hashes) == Some(' ') {
        Some(hashes)
    } else {
        None
    }
}

/// The heading level of a typst line (`== X` → 2): a run of `=` followed
/// by a space. Mirrors [`markdown_heading_level`] for typst's syntax.
fn typst_heading_level(line: &str) -> Option<usize> {
    let eqs = line.chars().take_while(|c| *c == '=').count();
    if eqs > 0 && line.chars().nth(eqs) == Some(' ') {
        Some(eqs)
    } else {
        None
    }
}

/// The heading detector for a format's section anchors, or `None` for
/// formats without heading structure (text/html/json/toml/yaml). Single
/// source of truth for `read_document`'s `section` slicing and
/// `edit_document_section`, so both support exactly the same formats.
fn heading_level_fn(format: DocumentFormat) -> Option<fn(&str) -> Option<usize>> {
    match format {
        DocumentFormat::Markdown => Some(markdown_heading_level),
        DocumentFormat::Typst => Some(typst_heading_level),
        _ => None,
    }
}

/// Locate a markdown section by a heading substring (case-insensitive):
/// returns `(start, end)` line indices spanning the heading through to the
/// line before the next heading of the same or higher level. `None` if no
/// heading matches.
fn section_bounds(
    lines: &[&str],
    needle: &str,
    heading: fn(&str) -> Option<usize>,
) -> Option<(usize, usize)> {
    let needle = needle.to_lowercase();
    let start = lines
        .iter()
        .position(|l| heading(l).is_some() && l.to_lowercase().contains(&needle))?;
    let level = heading(lines[start]).unwrap();
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(start + 1) {
        if let Some(lvl) = heading(l)
            && lvl <= level
        {
            end = i;
            break;
        }
    }
    Some((start, end))
}

/// Extract a markdown section's text (heading included). Returns `None` if
/// no heading contains `needle`.
fn extract_section(
    content: &str,
    needle: &str,
    heading: fn(&str) -> Option<usize>,
) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let (start, end) = section_bounds(&lines, needle, heading)?;
    Some(lines[start..end].join("\n"))
}

/// Replace the section whose heading contains `heading` with
/// `new_section` (which should include its own heading line), or append
/// `new_section` as a new section when no heading matches.
fn replace_or_append_section(
    content: &str,
    heading: &str,
    new_section: &str,
    level: fn(&str) -> Option<usize>,
) -> String {
    let new_section = new_section.trim_matches('\n');
    let lines: Vec<&str> = content.lines().collect();
    match section_bounds(&lines, heading, level) {
        Some((start, end)) => {
            let mut parts: Vec<String> = Vec::new();
            if start > 0 {
                parts.push(lines[..start].join("\n").trim_end().to_string());
            }
            parts.push(new_section.to_string());
            if end < lines.len() {
                parts.push(lines[end..].join("\n").trim_start_matches('\n').to_string());
            }
            parts.retain(|p| !p.is_empty());
            parts.join("\n\n")
        }
        None if content.trim().is_empty() => new_section.to_string(),
        None => format!("{}\n\n{new_section}", content.trim_end()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_patch_roundtrips_json() {
        let out = apply_structured_patch(
            DocumentFormat::Json,
            r#"{"a": 1, "b": 2}"#,
            &[serde_json::json!({"op": "replace", "path": "/a", "value": 9})],
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"], 9);
        assert_eq!(v["b"], 2);
    }

    #[test]
    fn structured_patch_roundtrips_toml() {
        let out = apply_structured_patch(
            DocumentFormat::Toml,
            "port = 80\nhost = \"localhost\"\n",
            &[serde_json::json!({"op": "replace", "path": "/port", "value": 8080})],
        )
        .unwrap();
        assert!(out.contains("port = 8080"), "{out}");
        assert!(out.contains("host = \"localhost\""), "{out}");
    }

    #[test]
    fn invalid_json_content_is_rejected() {
        let e = apply_structured_patch(DocumentFormat::Json, "not json", &[]).unwrap_err();
        assert!(matches!(e, ToolError::Failed(_)));
    }

    #[test]
    fn extract_section_returns_heading_to_next_same_level() {
        let md = "# Title\n\nintro\n\n## Install\n\nsteps\n\n## Usage\n\nmore\n";
        let s = extract_section(md, "Install", markdown_heading_level).unwrap();
        assert!(s.starts_with("## Install"), "{s}");
        assert!(s.contains("steps"), "{s}");
        assert!(!s.contains("## Usage"), "{s}");
    }

    #[test]
    fn extract_section_stops_at_higher_level_heading() {
        let md = "## A\n\naaa\n# B\n\nbbb\n";
        let s = extract_section(md, "A", markdown_heading_level).unwrap();
        assert!(s.contains("aaa"), "{s}");
        assert!(!s.contains("# B"), "{s}");
    }

    #[test]
    fn extract_section_missing_is_none() {
        assert!(extract_section("# Only\n", "nope", markdown_heading_level).is_none());
    }

    #[test]
    fn replace_section_swaps_only_that_section() {
        let md = "# T\n\nintro\n\n## Install\n\nold steps\n\n## Usage\n\nuse it\n";
        let out = replace_or_append_section(
            md,
            "Install",
            "## Install\n\nnew steps",
            markdown_heading_level,
        );
        assert!(out.contains("new steps"), "{out}");
        assert!(!out.contains("old steps"), "{out}");
        assert!(out.contains("## Usage"), "{out}");
        assert!(out.contains("intro"), "{out}");
    }

    #[test]
    fn replace_section_appends_when_heading_absent() {
        let md = "# T\n\nintro\n";
        let out =
            replace_or_append_section(md, "Refs", "## Refs\n\nsee here", markdown_heading_level);
        assert!(out.starts_with("# T"), "{out}");
        assert!(out.trim_end().ends_with("see here"), "{out}");
        assert!(out.contains("intro"), "{out}");
    }

    #[test]
    fn append_section_into_empty_doc() {
        let out = replace_or_append_section("", "X", "## X\n\nbody", markdown_heading_level);
        assert_eq!(out, "## X\n\nbody");
    }

    #[test]
    fn typst_headings_anchor_sections() {
        assert_eq!(typst_heading_level("= Title"), Some(1));
        assert_eq!(typst_heading_level("== Sub"), Some(2));
        assert_eq!(typst_heading_level("=no space"), None);
        assert_eq!(typst_heading_level("text = x"), None);

        let typ = "= Title\n\nintro\n\n== Install\n\nold steps\n\n== Usage\n\nuse it\n";
        let s = extract_section(typ, "Install", typst_heading_level).unwrap();
        assert!(s.starts_with("== Install"), "{s}");
        assert!(!s.contains("== Usage"), "{s}");
        let out = replace_or_append_section(
            typ,
            "Install",
            "== Install\n\nnew steps",
            typst_heading_level,
        );
        assert!(out.contains("new steps"), "{out}");
        assert!(!out.contains("old steps"), "{out}");
        assert!(out.contains("== Usage"), "{out}");
    }

    #[tokio::test]
    async fn tool_runtime_typst_document_with_section_edit() {
        let pool = seeded_pool().await;
        let c = ctx(pool, "s1");
        let created = CreateDocument
            .run(
                c.clone(),
                serde_json::json!({
                    "title": "Deck", "format": "typst",
                    "content": "= Deck\n\n== Intro\n\nhello\n"
                }),
            )
            .await
            .unwrap();
        assert_eq!(created["format"], "typst");
        let id = created["document_id"].as_str().unwrap().to_string();

        // Section edit anchors on the `==` typst heading.
        let edited = EditDocumentSection
            .run(
                c.clone(),
                serde_json::json!({
                    "document_id": id, "heading": "Intro",
                    "content": "== Intro\n\nrevised\n"
                }),
            )
            .await
            .unwrap();
        assert_eq!(edited["action"], "replaced");

        // `section` reads work for typst too.
        let read = ReadDocument
            .run(
                c.clone(),
                serde_json::json!({"document_id": id, "section": "Intro"}),
            )
            .await
            .unwrap();
        assert!(read["content"].as_str().unwrap().contains("revised"));

        // YAML is text-edited (find/replace), never JSON-patched.
        let y = CreateDocument
            .run(
                c.clone(),
                serde_json::json!({
                    "title": "Cfg", "format": "yaml",
                    "content": "# keep this comment\nport: 80\n"
                }),
            )
            .await
            .unwrap();
        let yid = y["document_id"].as_str().unwrap().to_string();
        EditDocument
            .run(
                c.clone(),
                serde_json::json!({
                    "document_id": yid,
                    "edits": [{"find": "port: 80", "replace": "port: 8080"}]
                }),
            )
            .await
            .unwrap();
        let read = ReadDocument
            .run(c, serde_json::json!({"document_id": yid}))
            .await
            .unwrap();
        let content = read["content"].as_str().unwrap();
        assert!(content.contains("port: 8080"), "{content}");
        assert!(content.contains("# keep this comment"), "{content}");
    }

    #[test]
    fn clip_is_char_boundary_safe() {
        let (out, trunc) = clip("héllo wörld", 3);
        assert!(trunc);
        assert!("héllo wörld".starts_with(&out));
    }

    // --- canvas render wiring (pins the panel ↔ route contract) ---------

    async fn seeded_pool() -> gateway_core::server::db::Pool {
        use std::path::Path;
        let pool = gateway_core::server::db::open(Path::new(":memory:"))
            .await
            .unwrap();
        sqlx::query(
            r#"INSERT INTO users (id, email, created_at, updated_at)
               VALUES ('u1', 'u1@example.com', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO chat_sessions (id, user_id, created_at, updated_at)
               VALUES ('s1', 'u1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn canvas_html_renders_markdown_and_is_none_when_empty() {
        let pool = seeded_pool().await;
        // No documents yet → no panel.
        assert!(
            gateway_core::server::document_canvas::render_canvas_html(
                &pool,
                "s1",
                None,
                None,
                session_core::i18n::Lang::En
            )
            .await
            .unwrap()
            .is_none()
        );

        let id = documents::new_id();
        documents::create(
            &pool,
            &id,
            "s1",
            "u1",
            "RGW Guide",
            DocumentFormat::Markdown,
            "# Intro\n\nhello world\n",
            None,
        )
        .await
        .unwrap();

        let html = gateway_core::server::document_canvas::render_canvas_html(
            &pool,
            "s1",
            None,
            None,
            session_core::i18n::Lang::En,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(html.contains("RGW Guide"), "title shown: {html}");
        assert!(html.contains("document-canvas"), "panel class present");
        assert!(html.contains("<h1"), "markdown rendered to HTML: {html}");
    }

    fn ctx(pool: gateway_core::server::db::Pool, session_id: &str) -> ToolContext {
        ToolContext {
            user_id: "u1".into(),
            roles: vec!["user".into()],
            db: pool,
            s3: None,
            assistant_turn_id: None,
            session_id: Some(session_id.into()),
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
            image_gen: None,
            sandbox_lease: None,
            crypto: None,
        }
    }

    #[tokio::test]
    async fn tool_runtime_create_edit_read_roundtrip() {
        let pool = seeded_pool().await;
        let c = ctx(pool, "s1");

        let created = CreateDocument
            .run(
                c.clone(),
                serde_json::json!({
                    "title": "Guide", "format": "markdown",
                    "content": "# Intro\n\nold body\n"
                }),
            )
            .await
            .unwrap();
        let id = created["document_id"].as_str().unwrap().to_string();
        assert_eq!(created["version"], 1);

        // Text find/replace bumps to v2.
        let edited = EditDocument
            .run(
                c.clone(),
                serde_json::json!({
                    "document_id": id,
                    "edits": [{"find": "old body", "replace": "new body"}]
                }),
            )
            .await
            .unwrap();
        assert_eq!(edited["version"], 2);

        // Read it back by grep.
        let read = ReadDocument
            .run(
                c.clone(),
                serde_json::json!({"document_id": id, "grep": "new"}),
            )
            .await
            .unwrap();
        assert!(read["content"].as_str().unwrap().contains("new body"));

        // A non-unique/missing anchor is a clean InvalidArgs, not a panic.
        let err = EditDocument
            .run(
                c,
                serde_json::json!({
                    "document_id": id,
                    "edits": [{"find": "nowhere", "replace": "x"}]
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }

    #[tokio::test]
    async fn tool_runtime_version_history_and_rollback() {
        let pool = seeded_pool().await;
        let c = ctx(pool, "s1");
        let created = CreateDocument
            .run(
                c.clone(),
                serde_json::json!({
                    "title": "Guide", "format": "markdown", "content": "v1 body"
                }),
            )
            .await
            .unwrap();
        let id = created["document_id"].as_str().unwrap().to_string();
        EditDocument
            .run(
                c.clone(),
                serde_json::json!({
                    "document_id": id,
                    "edits": [{"find": "v1 body", "replace": "v2 body"}],
                    "summary": "rewrote body"
                }),
            )
            .await
            .unwrap();

        // History lists both versions, newest first, with summaries.
        let hist = ListDocumentVersions
            .run(c.clone(), serde_json::json!({"document_id": id}))
            .await
            .unwrap();
        assert_eq!(hist["current_version"], 2);
        let versions = hist["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0]["version"], 2);
        assert_eq!(versions[0]["summary"], "rewrote body");
        assert_eq!(versions[1]["summary"], "Created");

        // Rollback to v1 appends a NEW version 3 with v1's content.
        let restored = RestoreDocumentVersion
            .run(
                c.clone(),
                serde_json::json!({"document_id": id, "version": 1}),
            )
            .await
            .unwrap();
        assert_eq!(restored["version"], 3);
        let read = ReadDocument
            .run(c.clone(), serde_json::json!({"document_id": id}))
            .await
            .unwrap();
        assert_eq!(read["content"], "v1 body");
        assert_eq!(read["version"], 3);

        // Restoring the current version is a clean error, as is an
        // unknown version.
        let err = RestoreDocumentVersion
            .run(
                c.clone(),
                serde_json::json!({"document_id": id, "version": 3}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
        let err = RestoreDocumentVersion
            .run(c, serde_json::json!({"document_id": id, "version": 99}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }

    #[tokio::test]
    async fn tool_runtime_json_patch_edit() {
        let pool = seeded_pool().await;
        let c = ctx(pool, "s1");
        let created = CreateDocument
            .run(
                c.clone(),
                serde_json::json!({
                    "title": "Cfg", "format": "json", "content": "{\"port\": 80}"
                }),
            )
            .await
            .unwrap();
        let id = created["document_id"].as_str().unwrap().to_string();

        let edited = EditDocument
            .run(
                c.clone(),
                serde_json::json!({
                    "document_id": id,
                    "edits": [{"op": "replace", "path": "/port", "value": 8080}]
                }),
            )
            .await
            .unwrap();
        assert_eq!(edited["version"], 2);

        let read = ReadDocument
            .run(c, serde_json::json!({"document_id": id}))
            .await
            .unwrap();
        assert!(read["content"].as_str().unwrap().contains("8080"));
    }

    #[tokio::test]
    async fn version_switcher_url_matches_the_route_pattern() {
        let pool = seeded_pool().await;
        let id = documents::new_id();
        documents::create(
            &pool,
            &id,
            "s1",
            "u1",
            "Doc",
            DocumentFormat::Markdown,
            "v1 body\n",
            None,
        )
        .await
        .unwrap();
        // A second version turns on the version switcher.
        documents::append_version(&pool, "s1", &id, "v2 body\n", Some("edit"), None)
            .await
            .unwrap();

        let html = gateway_core::server::document_canvas::render_canvas_html(
            &pool,
            "s1",
            None,
            None,
            session_core::i18n::Lang::En,
        )
        .await
        .unwrap()
        .unwrap();
        // The @get target must be the path the router serves
        // (`/chat/{id}/document/{doc_id}`) so the panel's switcher reaches a
        // real route. This is the UI-directive ↔ endpoint contract.
        assert!(
            html.contains(&format!("/chat/s1/document/{id}?version=")),
            "version switcher points at the document route: {html}"
        );
    }
}
