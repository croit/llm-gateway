// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `edit_image` — image-to-image editing.
//!
//! Given an existing attachment (a user upload or a previously generated
//! image, addressed by its `<turn_id>/<filename>` id) plus a text
//! instruction, the gateway fetches those bytes server-side, routes them to a
//! `kind = "image"` backend that advertises edit support (via
//! [`gateway_core::server::image_gen::ImageGenerator::edit`]), and splices the
//! result back into the reply as an inline attachment — same rendering path
//! as `generate_image` / `upload_attachment`.
//!
//! Only registered when an edit-capable backend is configured
//! (`supports_edit = true`); editing against a non-GDPR-compliant backend is
//! refused in the core, since it would ship existing user content off-site.

use serde::Deserialize;
use serde_json::{Value, json};
use session_core::db as chat;
use shared::api::ToolDef;

use gateway_core::server::chat_attachments;
use gateway_core::server::db::usage::UsageSource;
use gateway_core::server::image_gen::UsageMeta;
use gateway_core::server::tools::{Tool, ToolContext, ToolError, ToolFuture};

pub struct EditImage;

#[derive(Deserialize)]
struct EditArgs {
    /// Attachment id or visible filename of the image to edit. Bare filenames
    /// are resolved against the current chat session.
    image_id: String,
    prompt: String,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

impl Tool for EditImage {
    fn id(&self) -> &str {
        "edit_image"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Edit an existing image with a text instruction (image-to-image): \
              change the background, add or remove an element, restyle it. Pass \
              the `image_id` of an image already in the conversation. Use the \
              attachment id when available, or the visible filename. The edited \
              image is rendered inline in your reply; do NOT repeat any marker \
              or describe it in prose.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["image_id", "prompt"],
                "properties": {
                    "image_id": {
                        "type": "string",
                        "description": "Attachment id or visible filename of the \
                                        source image already in this conversation."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "What to change about the image."
                    },
                    "size": {
                        "type": "string",
                        "description": "Optional output dimensions like `1024x1024`."
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional specific image model to use."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: EditArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{image_id, prompt, size?, model?}}: {e}"))
            })?;
            if args.prompt.trim().is_empty() {
                return Err(ToolError::InvalidArgs("`prompt` must not be empty".into()));
            }
            let session_id = ctx.session_id.as_ref().ok_or_else(|| {
                ToolError::Failed("edit_image is only available inside a chat session".into())
            })?;
            let resolved = if let Some((src_turn, src_file)) = args.image_id.split_once('/') {
                (src_turn.to_string(), src_file.to_string())
            } else {
                let attachments = chat_attachments::list_session_attachments(&ctx.db, session_id)
                    .await
                    .map_err(|e| ToolError::Failed(format!("listing chat attachments: {e}")))?;
                let Some(attachment) =
                    chat_attachments::resolve_attachment(&attachments, &args.image_id)
                else {
                    return Err(ToolError::InvalidArgs(format!(
                        "`image_id` does not identify an image in this conversation (got `{}`)",
                        args.image_id
                    )));
                };
                (attachment.turn_id.clone(), attachment.filename.clone())
            };
            let (src_turn, src_file) = (&resolved.0, &resolved.1);

            let image_gen = ctx.image_gen.as_ref().ok_or_else(|| {
                ToolError::Failed("image generation is not configured on this gateway".into())
            })?;
            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "chat attachments are not configured on this gateway ([chat.s3])".into(),
                )
            })?;
            let turn_id = ctx.assistant_turn_id.as_ref().ok_or_else(|| {
                ToolError::Failed("edit_image is only available inside a chat session".into())
            })?;
            let reservations = ctx.attachment_reservations.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "edit_image requires a per-turn attachment-reservation set".into(),
                )
            })?;

            // Fetch the source image bytes server-side (same private-bucket
            // path fetch_attachment uses).
            let source = chat_attachments::fetch(s3, src_turn, src_file)
                .await
                .map_err(|e| ToolError::Failed(format!("reading source image: {e}")))?;
            if !source.mime.starts_with("image/") {
                return Err(ToolError::InvalidArgs(format!(
                    "attachment `{}` is not an image ({})",
                    args.image_id, source.mime
                )));
            }

            let meta = UsageMeta {
                user_id: ctx.user_id.clone(),
                source: UsageSource::Chat,
            };
            let edited = image_gen
                .edit(
                    args.model.as_deref(),
                    source.bytes,
                    &source.mime,
                    &args.prompt,
                    args.size.as_deref(),
                    &meta,
                )
                .await
                .map_err(|e| ToolError::Failed(e.to_string()))?;

            let base = format!(
                "edited-image{}",
                chat_attachments::ext_for_mime(&edited.mime).unwrap_or(".png")
            );
            let filename =
                chat_attachments::reserve_filename(&ctx.db, turn_id, reservations, &base)
                    .await
                    .map_err(|e| ToolError::Failed(format!("reserve filename: {e}")))?;
            let size = edited.bytes.len() as u64;
            let outcome =
                chat_attachments::upload(s3, turn_id, &filename, &edited.mime, edited.bytes)
                    .await
                    .map_err(|e| ToolError::Failed(format!("s3 upload failed: {e}")))?;

            let marker = chat_attachments::marker_line(turn_id, &outcome);
            chat::append_content(&ctx.db, turn_id, &format!("\n\n{marker}\n\n"))
                .await
                .map_err(|e| ToolError::Failed(format!("persist marker: {e}")))?;

            Ok(json!({
                "filename": outcome.filename,
                "mime": outcome.mime,
                "size": size,
                "id": format!("{turn_id}/{}", outcome.filename),
                "rendered": "Inline in your message bubble — do NOT repeat the \
                             marker text or describe the image in your prose.",
            }))
        })
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(130))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names_match_id() {
        assert_eq!(EditImage.id(), EditImage.schema().function.name);
    }

    async fn ctx_no_image_gen() -> ToolContext {
        ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: gateway_core::server::db::open(std::path::Path::new(":memory:"))
                .await
                .unwrap(),
            s3: None,
            assistant_turn_id: Some("t-1".into()),
            session_id: Some("s-1".into()),
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
    async fn rejects_bad_image_id() {
        let ctx = ctx_no_image_gen().await;
        let err = EditImage
            .run(
                ctx,
                json!({ "image_id": "no-slash", "prompt": "make it blue" }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }

    #[tokio::test]
    async fn errors_when_image_gen_not_configured() {
        let ctx = ctx_no_image_gen().await;
        let err = EditImage
            .run(
                ctx,
                json!({ "image_id": "turn-1/pic.png", "prompt": "make it blue" }),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("image generation is not configured")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
