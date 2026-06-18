// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Per-template `typst_<id>` tool wrapper.
//!
//! One [`TypstRenderTool`] instance per discovered template. The
//! manifest's declared fields become the tool's JSON schema; on
//! invocation we hand the validated key/value pairs to
//! [`crate::server::typst::compile`], then upload three attachments
//! ({.pdf, .png, .typ}) under the assistant turn id and splice
//! their markers into the assistant's `content`. The model can then
//! iterate by editing field values and calling again — the previous
//! attachments stay in the turn's content for context.
//!
//! These are registered dynamically at startup (see `main.rs`);
//! the tool id is `typst_<template.id>` and is leaked into a
//! `'static str` so the `Tool` trait's signature is happy. Tools
//! registered once at startup never get dropped, so the leak is
//! bounded.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use session_core::db as chat;
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};
use crate::server::chat_attachments;
use crate::server::typst::{self, DefaultSource, FieldType, Template};

pub struct TypstRenderTool {
    template: Arc<Template>,
    /// Leaked `Box<str>` so the trait's `&'static str` return is
    /// satisfied for runtime-constructed tools. The Tool lives for
    /// the whole process; the leak is single-allocation-per-template
    /// at startup.
    id: &'static str,
}

impl TypstRenderTool {
    pub fn new(template: Template) -> Self {
        let id: &'static str = Box::leak(format!("typst_{}", template.id).into_boxed_str());
        Self {
            template: Arc::new(template),
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
                "{descr} Returns three attachments spliced into your reply: \
                 `{base}.pdf` (final document), `{base}.png` (page-1 \
                 preview you can visually inspect), and `{base}.typ` (the \
                 raw typst source so you can iterate on edits).",
                descr = t.description,
                base = t.output_basename,
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

            let inputs = stringify_args(&template, &args)?;
            let rendered = typst::compile(&template, &inputs).await.map_err(|e| {
                // Compile errors become InvalidArgs so the model is
                // nudged to fix its input — typst's stderr usually
                // names the offending line / variable.
                use typst::CompileError;
                match e {
                    CompileError::Failed(msg) => {
                        ToolError::InvalidArgs(format!("typst compile failed:\n{msg}"))
                    }
                    other => ToolError::Failed(other.to_string()),
                }
            })?;

            // Same-turn dedup, race-safe across concurrent tool calls:
            // a second typst call (or a sibling `upload_attachment`
            // claiming e.g. `letter.png` mid-render) would otherwise
            // overwrite this render's objects in S3 and leave the
            // earlier markers pointing at the new bytes. The
            // reservation mutex serializes the pick across the
            // `join_all` of parallel tool calls, and the trio is
            // reserved as a unit so the three files stay in sync
            // (chart.pdf+png+typ → chart-2.pdf+png+typ).
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
            let typ_name = format!("{base}.typ");

            let pdf_out =
                chat_attachments::upload(s3, turn_id, &pdf_name, "application/pdf", rendered.pdf)
                    .await
                    .map_err(|e| ToolError::Failed(format!("upload pdf: {e}")))?;
            let png_out =
                chat_attachments::upload(s3, turn_id, &png_name, "image/png", rendered.png)
                    .await
                    .map_err(|e| ToolError::Failed(format!("upload png: {e}")))?;
            let typ_out =
                chat_attachments::upload(s3, turn_id, &typ_name, "text/x-typst", rendered.source)
                    .await
                    .map_err(|e| ToolError::Failed(format!("upload typ: {e}")))?;

            // Splice all three markers in one chunk so the renderer
            // emits a tight cluster (image preview + chip for the
            // pdf + chip for the source) at the model's write
            // position.
            let chunk = format!(
                "\n\n{}\n{}\n{}\n\n",
                chat_attachments::marker_line(turn_id, &pdf_out),
                chat_attachments::marker_line(turn_id, &png_out),
                chat_attachments::marker_line(turn_id, &typ_out),
            );
            chat::append_content(&ctx.db, turn_id, &chunk)
                .await
                .map_err(|e| ToolError::Failed(format!("persist markers: {e}")))?;

            Ok(json!({
                "template": template.id,
                "pdf": { "filename": pdf_out.filename, "size": pdf_out.bytes,
                         "id": format!("{turn_id}/{}", pdf_out.filename) },
                "png": { "filename": png_out.filename, "size": png_out.bytes,
                         "id": format!("{turn_id}/{}", png_out.filename) },
                "source": { "filename": typ_out.filename, "size": typ_out.bytes,
                            "id": format!("{turn_id}/{}", typ_out.filename) },
                "rendered": "All three attachments are now inline in your \
                             message bubble — do NOT repeat the marker text \
                             in your prose. Look at the PNG to verify the \
                             layout; call again with adjusted fields to iterate.",
            }))
        })
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

/// Three extensions a typst render writes together; the trio must
/// share a stem so the model sees `foo.pdf` / `foo.png` / `foo.typ`
/// (not `foo-2.pdf` paired with `foo-3.png` because one was free and
/// the other wasn't). Passed to [`chat_attachments::reserve_basename`]
/// on every render so the reservation is taken as a unit.
const TYPST_EXTS: &[&str] = &["pdf", "png", "typ"];

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

    #[test]
    fn id_is_prefixed_and_stable() {
        let tool = TypstRenderTool::new(stub_template());
        assert_eq!(tool.id(), "typst_stub");
        // Calling id() twice must return the same pointer — leaked
        // strings are pinned for the process lifetime.
        let a = tool.id().as_ptr();
        let b = tool.id().as_ptr();
        assert_eq!(a, b);
    }

    #[test]
    fn schema_lists_required_fields_only() {
        let tool = TypstRenderTool::new(stub_template());
        let def = tool.schema();
        let params = serde_json::to_value(&def.function.parameters).unwrap();
        let required = params["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "title");
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
}
