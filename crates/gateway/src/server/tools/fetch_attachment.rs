// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Resolves opaque attachment ids (handed to the model in past-turn
//! replay stubs) back to the live S3 object and returns the bytes.
//!
//! Why: chat attachments only travel in their original user turn —
//! the gateway strips later replays down to `[attached file="..."
//! mime="..." size=N id="<turn_id>/<filename>"]` stubs so the
//! conversation context doesn't bloat. If the model decides it
//! actually needs to look at that file again, it calls this tool
//! with the stub's `id` and the gateway pulls the object server-side
//! (no presigned URL ever crosses the wire to the LLM provider, and
//! TTL expiry is irrelevant because the gateway re-fetches each
//! time).
//!
//! Three return shapes depending on the attachment's mime:
//!
//! - **Text-ish** (CSV, JSON, markdown, code, …): decoded UTF-8 in
//!   `content`. The model reads it like any other tool output.
//! - **Image**: the gateway presigns a fresh GET URL and returns a
//!   `tool_content_parts(...)` envelope carrying a text summary plus
//!   an `image_url` part — the driver splices that into the upstream
//!   `role:"tool"` message as array content, which lets a vision
//!   model actually re-see the image. No bytes cross the wire to
//!   the LLM provider in inline form; just the (time-limited)
//!   presigned URL the upstream fetches itself.
//! - **Other binary** (PDF, zip, …): metadata only, with a note
//!   telling the model the bytes can't be reattached via a tool
//!   result. The model should ask the user to re-upload.

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture, tool_content_parts};
use crate::server::chat_attachments::{self, BinaryDisposition, PayloadLimits};

/// Hard cap on text returned to the model — shared with `fetch_url`
/// so both tools have the same contract. 4 MB is generous enough
/// that essentially no real attachment is truncated in practice
/// (modern context windows handle ~1M tokens of text), while still
/// bounding the gateway's per-call memory footprint. The caller
/// can request less via `max_bytes`.
const HARD_MAX_BYTES: usize = 4 * 1024 * 1024;
const HARD_MAX_BYTES_DEFAULT: usize = HARD_MAX_BYTES;
/// Image ceiling — generous enough for phone photos (typically
/// 5–15 MB) and screenshots. Above this we surface a
/// `kind: "image-too-large"` payload so the model knows why it
/// didn't get the bytes inline.
const MAX_IMAGE_BYTES: usize = 25 * 1024 * 1024;

pub struct FetchAttachment;

#[derive(Deserialize)]
struct FetchArgs {
    id: String,
    #[serde(default)]
    max_bytes: Option<usize>,
}

impl Tool for FetchAttachment {
    fn id(&self) -> &str {
        "fetch_attachment"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Fetch the contents of a chat attachment by its opaque id. \
             User messages with attachments show them as `[attached file=… \
             mime=… size=… id=\"<turn_id>/<filename>\"]` stubs — call this \
             tool with the stub's id when you actually need the bytes. \
             Text-ish files (code, JSON, CSV, markdown, plain text, …) are \
             returned as UTF-8 in `content`. Images are re-attached as a \
             visible `image_url` part you can look at. Other binary files \
             (PDF, zip, …) return metadata only; ask the user to re-upload \
             if you need them. Skip calling this if the user's question \
             doesn't depend on the attachment's contents.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Opaque attachment id of the form \
                                        `<turn_id>/<filename>` exactly as it \
                                        appeared in the replay stub."
                    },
                    "max_bytes": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": HARD_MAX_BYTES,
                        "description": "Optional cap on bytes returned for \
                                        text content. Defaults to the full \
                                        attachment up to 4 MB (the hard cap)."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: FetchArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{id, max_bytes?}}: {e}")))?;

            let (turn_id, filename) = split_id(&args.id)?;
            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "chat attachments are not configured on this gateway \
                     (operator must set [chat.s3])"
                        .into(),
                )
            })?;

            let fetched = chat_attachments::fetch(s3, turn_id, filename)
                .await
                .map_err(|e| ToolError::Failed(format!("s3 GET failed: {e}")))?;

            let cap = args
                .max_bytes
                .unwrap_or(HARD_MAX_BYTES_DEFAULT)
                .min(HARD_MAX_BYTES);
            let mime = fetched.mime.clone();
            let limits = PayloadLimits {
                max_text_bytes: cap,
                max_image_bytes: MAX_IMAGE_BYTES,
            };

            match chat_attachments::classify_payload(&mime, filename, fetched.bytes, limits) {
                BinaryDisposition::Text {
                    content,
                    bytes_returned,
                    truncated,
                    original_len,
                } => Ok(json!({
                    "id": args.id,
                    "filename": filename,
                    "mime": mime,
                    "size": original_len,
                    "kind": "text",
                    "content": content,
                    "bytes_returned": bytes_returned,
                    "bytes_original": original_len,
                    "truncated": truncated,
                })),
                BinaryDisposition::Image {
                    data_uri,
                    original_len,
                } => {
                    // `tool_content_parts(...)` envelope: the driver
                    // splices this into the upstream `role:"tool"`
                    // message as an array-of-parts so vision models
                    // actually see the image. OpenAI Chat Completions
                    // accepts `data:` URIs in `image_url`.
                    let summary = format!(
                        "Re-attached image `{filename}` ({mime}, {original_len} bytes, id={id}).",
                        id = args.id,
                    );
                    Ok(tool_content_parts(vec![
                        json!({"type": "text", "text": summary}),
                        json!({"type": "image_url", "image_url": {"url": data_uri}}),
                    ]))
                }
                BinaryDisposition::Binary { original_len } => {
                    // Two cases land here: actual binary (PDF/zip/…)
                    // and over-cap images. Differentiate via mime so
                    // the model gets a precise reason rather than a
                    // generic "binary" stub for what is in fact an
                    // image.
                    let (kind, note) = if mime.starts_with("image/") {
                        (
                            "image-too-large",
                            format!(
                                "Image is {original_len} bytes; ceiling is \
                                 {MAX_IMAGE_BYTES} bytes for inline return. \
                                 Ask the user for a downscaled version if \
                                 you need to look at it."
                            ),
                        )
                    } else {
                        (
                            "binary",
                            "Non-image binary attachment — bytes can't be \
                             reattached via a tool result. Ask the user to \
                             re-upload if you need to inspect this file."
                                .to_string(),
                        )
                    };
                    Ok(json!({
                        "id": args.id,
                        "filename": filename,
                        "mime": mime,
                        "size": original_len,
                        "kind": kind,
                        "note": note,
                    }))
                }
            }
        })
    }
}

/// Split `<turn_id>/<filename>` into its parts. Rejects ids with
/// multiple slashes, leading slashes, or empty components — keeps
/// the surface tight against a model that hallucinates a different
/// shape than the replay stub.
fn split_id(id: &str) -> Result<(&str, &str), ToolError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_id_accepts_simple_form() {
        assert_eq!(split_id("t-1/x.csv").unwrap(), ("t-1", "x.csv"));
    }

    #[test]
    fn split_id_rejects_nested_filename() {
        assert!(matches!(
            split_id("t-1/sub/x.csv").unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn split_id_rejects_missing_slash() {
        assert!(matches!(
            split_id("bareword").unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn split_id_rejects_empty_segments() {
        assert!(matches!(
            split_id("/x.csv").unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
        assert!(matches!(
            split_id("t-1/").unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn schema_names_match_id() {
        assert_eq!(FetchAttachment.id(), FetchAttachment.schema().function.name);
    }

    #[tokio::test]
    async fn errors_cleanly_when_s3_not_configured() {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let ctx = ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: pool,
            s3: None,
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
        };
        let err = FetchAttachment
            .run(ctx, json!({"id": "t-1/x.csv"}))
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("not configured"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_malformed_id_before_touching_s3() {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let ctx = ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: pool,
            // Deliberately Some so we'd reach the s3 call if validation slipped —
            // the test asserts we don't get that far.
            s3: Some(std::sync::Arc::new(crate::server::config::S3Config {
                endpoint: "http://127.0.0.1:1".into(),
                region: "us-east-1".into(),
                bucket: "b".into(),
                access_key_env: "FETCH_ATTACHMENT_TEST_NOT_SET".into(),
                secret_key_env: "FETCH_ATTACHMENT_TEST_NOT_SET".into(),
                key_prefix: "chat-attachments".into(),
            })),
            assistant_turn_id: None,
            session_id: None,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
        };
        let err = FetchAttachment
            .run(ctx, json!({"id": "nope"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }
}
