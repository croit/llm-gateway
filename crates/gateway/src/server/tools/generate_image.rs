// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `generate_image` — text-to-image generation.
//!
//! The model describes an image; the gateway routes the request to a
//! `kind = "image"` upstream pool (via [`crate::server::image_gen`]), gets the
//! bytes in hand (decoding `b64_json`, or fetching an upstream URL
//! server-side), uploads them to the same S3 bucket chat attachments use, and
//! splices a `[gw-attachment …]` marker into the assistant turn's `content` —
//! so the chat renderer draws the image inline exactly like `upload_attachment`
//! does. The tool's success is that marker landing in the bubble; it returns
//! only concise metadata to the model (not the image bytes), matching
//! `upload_attachment` — the picture is for the human, and re-embedding it
//! would burn context for no benefit.

use serde::Deserialize;
use serde_json::{Value, json};
use session_core::db as chat;
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};
use crate::server::chat_attachments;
use crate::server::db::usage::UsageSource;
use crate::server::image_gen::UsageMeta;

pub struct GenerateImage;

#[derive(Deserialize)]
struct GenerateArgs {
    prompt: String,
    /// Requested dimensions, e.g. `"1024x1024"`. Backend-dependent; omitted →
    /// the backend's default.
    #[serde(default)]
    size: Option<String>,
    /// Specific image model / alias to use. Omitted → the image pool's model.
    #[serde(default)]
    model: Option<String>,
}

impl Tool for GenerateImage {
    fn id(&self) -> &str {
        "generate_image"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Generate an image from a text prompt (diagrams, mockups, \
             illustrations, marketing visuals). The image is created by the \
             configured image backend, stored in the gateway, and rendered \
             inline in your reply as a thumbnail — exactly like an attachment. \
             Write a vivid, specific prompt (subject, style, composition, \
             colours). Do NOT describe the result back to the user as text or \
             repeat any marker; it appears in your message automatically.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["prompt"],
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "What to draw. Be specific about subject, \
                                        style, composition, and colours."
                    },
                    "size": {
                        "type": "string",
                        "description": "Optional dimensions like `1024x1024`. \
                                        Backend-dependent; omit for the default."
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional specific image model to use. \
                                        Omit to use the configured default."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: GenerateArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{prompt, size?, model?}}: {e}"))
            })?;
            if args.prompt.trim().is_empty() {
                return Err(ToolError::InvalidArgs("`prompt` must not be empty".into()));
            }

            let image_gen = ctx.image_gen.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "image generation is not configured on this gateway \
                     (operator must add a `kind = \"image\"` upstream pool)"
                        .into(),
                )
            })?;

            // Attachment side effects live only on the chat path — the same
            // preconditions `upload_attachment` enforces. Check them BEFORE the
            // (slow, billable) generation call so we don't spend an upstream
            // request we can't render.
            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "chat attachments are not configured on this gateway \
                     (operator must set [chat.s3]) — nowhere to store the image"
                        .into(),
                )
            })?;
            let turn_id = ctx.assistant_turn_id.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "generate_image is only available inside a chat session — \
                     there's no assistant turn to attach the image to"
                        .into(),
                )
            })?;
            let reservations = ctx.attachment_reservations.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "generate_image requires a per-turn attachment-reservation \
                     set, which is only initialised on the chat-page path"
                        .into(),
                )
            })?;

            let meta = UsageMeta {
                user_id: ctx.user_id.clone(),
                // The tool only completes on the chat path (it needs an
                // assistant turn), so attribute the metered call to Chat.
                source: UsageSource::Chat,
            };
            let image = image_gen
                .generate(
                    args.model.as_deref(),
                    &args.prompt,
                    args.size.as_deref(),
                    &meta,
                )
                .await
                .map_err(|e| ToolError::Failed(e.to_string()))?;

            let base = format!("generated-image{}", ext_for_mime(&image.mime));
            let filename =
                chat_attachments::reserve_filename(&ctx.db, turn_id, reservations, &base)
                    .await
                    .map_err(|e| ToolError::Failed(format!("reserve filename: {e}")))?;

            let size = image.bytes.len() as u64;
            let outcome =
                chat_attachments::upload(s3, turn_id, &filename, &image.mime, image.bytes)
                    .await
                    .map_err(|e| ToolError::Failed(format!("s3 upload failed: {e}")))?;

            let marker = chat_attachments::marker_line(turn_id, &outcome);
            let chunk = format!("\n\n{marker}\n\n");
            chat::append_content(&ctx.db, turn_id, &chunk)
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
        // Diffusion backends can take a while; give the tool more headroom
        // than the runner's default, matching the core's generate timeout.
        Some(std::time::Duration::from_secs(130))
    }
}

/// File extension (with leading dot) for a generated-image mime. Defaults to
/// `.png` for anything unrecognised — the marker only needs a plausible name.
fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => ".jpg",
        "image/webp" => ".webp",
        "image/gif" => ".gif",
        _ => ".png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names_match_id() {
        assert_eq!(GenerateImage.id(), GenerateImage.schema().function.name);
    }

    #[test]
    fn ext_for_mime_maps_common_types() {
        assert_eq!(ext_for_mime("image/png"), ".png");
        assert_eq!(ext_for_mime("image/jpeg"), ".jpg");
        assert_eq!(ext_for_mime("image/webp"), ".webp");
        assert_eq!(ext_for_mime("application/octet-stream"), ".png");
    }

    async fn ctx_without_image_gen() -> ToolContext {
        ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: crate::server::db::open(std::path::Path::new(":memory:"))
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
        }
    }

    #[tokio::test]
    async fn errors_when_image_gen_not_configured() {
        let ctx = ctx_without_image_gen().await;
        let err = GenerateImage
            .run(ctx, json!({ "prompt": "a red circle" }))
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => {
                assert!(msg.contains("image generation is not configured"), "{msg}")
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_empty_prompt() {
        let ctx = ctx_without_image_gen().await;
        let err = GenerateImage
            .run(ctx, json!({ "prompt": "   " }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }
}
