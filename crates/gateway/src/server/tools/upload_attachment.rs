// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Lets the assistant push a file back into the conversation —
//! images, PDFs, generated CSVs, anything that's too large or
//! awkward to inline in prose. The tool uploads to the same S3
//! bucket user-composer attachments go to, then splices a
//! `[gw-attachment …]` marker into the assistant turn's `content`
//! at the model's current write position, so the chat-bubble
//! renderer (which already knows how to split markers out of
//! `content` and draw chips/images) picks it up for free.
//!
//! Why server-side splicing rather than returning the marker for
//! the model to echo back: the model would have to remember to
//! emit the exact marker text after the tool returns, and any
//! escape/quoting drift would break the renderer. Doing the splice
//! ourselves keeps the contract tight — the tool's success is the
//! marker landing in the bubble.

use serde::Deserialize;
use serde_json::{Value, json};
use session_core::db as chat;
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};
use crate::server::chat_attachments;

/// Hard cap on the decoded byte size of one upload. Mirrors the
/// 30 s upload timeout in `chat_attachments` — keeps a runaway
/// model from streaming megabytes of base64 in a single tool call.
const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;

pub struct UploadAttachment;

#[derive(Deserialize)]
struct UploadArgs {
    filename: String,
    mime: String,
    /// Raw UTF-8 content. Use for text/code/CSV/markdown — anything
    /// that round-trips cleanly as a JSON string.
    #[serde(default)]
    content: Option<String>,
    /// Standard (RFC 4648) base64-encoded content. Use for binary
    /// files (images, PDFs, archives). Mutually exclusive with
    /// `content`.
    #[serde(default)]
    content_base64: Option<String>,
}

impl Tool for UploadAttachment {
    fn id(&self) -> &str {
        "upload_attachment"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Attach a file to the current assistant message. The file is \
             stored in the gateway's chat-attachment bucket and rendered \
             inline in your reply (images as thumbnails, other files as \
             download chips), exactly like a user-uploaded attachment. \
             Use `content` for UTF-8 text (code, CSV, markdown, JSON, etc.) \
             and `content_base64` for binary files (PNG, JPEG, PDF, …). \
             Exactly one of the two must be supplied. Filenames must not \
             contain `/`.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["filename", "mime"],
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "User-visible filename. No slashes."
                    },
                    "mime": {
                        "type": "string",
                        "description": "Mime type, e.g. `image/png`, \
                                        `application/pdf`, `text/csv`."
                    },
                    "content": {
                        "type": "string",
                        "description": "Raw UTF-8 file contents. Use for text."
                    },
                    "content_base64": {
                        "type": "string",
                        "description": "RFC 4648 base64-encoded file contents. \
                                        Use for binary files."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: UploadArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{filename, mime, content | content_base64}}: {e}"
                ))
            })?;
            let bytes = decode_payload(&args)?;
            if bytes.len() > MAX_UPLOAD_BYTES {
                return Err(ToolError::InvalidArgs(format!(
                    "attachment is {} bytes; max {} bytes per upload",
                    bytes.len(),
                    MAX_UPLOAD_BYTES
                )));
            }

            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "chat attachments are not configured on this gateway \
                     (operator must set [chat.s3])"
                        .into(),
                )
            })?;
            let turn_id = ctx.assistant_turn_id.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "upload_attachment is only available inside a chat \
                     session — there's no assistant turn to attach to on \
                     this code path"
                        .into(),
                )
            })?;

            // Same-turn dedup, race-safe across concurrent tool calls:
            // the second `upload_attachment("report.csv")` in one round
            // becomes `report-2.csv` so the first marker still points
            // at the first bytes. The reservation mutex serializes the
            // pick across the `join_all` of parallel tool calls.
            let reservations = ctx.attachment_reservations.as_ref().ok_or_else(|| {
                ToolError::Failed(
                    "upload_attachment requires a per-turn attachment-reservation set, \
                     which is only initialised on the chat-page path"
                        .into(),
                )
            })?;
            let filename =
                chat_attachments::reserve_filename(&ctx.db, turn_id, reservations, &args.filename)
                    .await
                    .map_err(|e| ToolError::Failed(format!("reserve filename: {e}")))?;

            let outcome = chat_attachments::upload(s3, turn_id, &filename, &args.mime, bytes)
                .await
                .map_err(|e| ToolError::Failed(format!("s3 upload failed: {e}")))?;

            // Marker line goes onto the assistant turn's `content`
            // at whatever position the model is currently writing —
            // the renderer's `split_markers` walk then pulls it out
            // and renders an image/chip in-place. Leading `\n` so
            // we don't accidentally glue the marker onto a prose
            // line the model just streamed.
            let marker = chat_attachments::marker_line(turn_id, &outcome);
            let chunk = format!("\n\n{marker}\n\n");
            chat::append_content(&ctx.db, turn_id, &chunk)
                .await
                .map_err(|e| ToolError::Failed(format!("persist marker: {e}")))?;

            Ok(json!({
                "filename": outcome.filename,
                "mime": outcome.mime,
                "size": outcome.bytes,
                "id": format!("{turn_id}/{}", outcome.filename),
                "rendered": "Inline in your message bubble — \
                             do NOT repeat the marker text in your prose.",
            }))
        })
    }
}

fn decode_payload(args: &UploadArgs) -> Result<Vec<u8>, ToolError> {
    match (&args.content, &args.content_base64) {
        (Some(_), Some(_)) => Err(ToolError::InvalidArgs(
            "pass either `content` or `content_base64`, not both".into(),
        )),
        (None, None) => Err(ToolError::InvalidArgs(
            "missing file contents (need `content` or `content_base64`)".into(),
        )),
        (Some(s), None) => Ok(s.as_bytes().to_vec()),
        (None, Some(b64)) => decode_base64(b64),
    }
}

/// Strict RFC 4648 base64 decoder (no URL-safe alphabet, no whitespace
/// tolerance other than ASCII whitespace stripping — the LLM JSON
/// string may have line breaks). Padding (`=`) is optional. Hand-rolled
/// to avoid pulling in `base64` as a direct workspace dep just for one
/// caller.
fn decode_base64(s: &str) -> Result<Vec<u8>, ToolError> {
    let clean: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let bytes = clean.as_bytes();
    // Strip optional padding from the end; we infer length from the
    // un-padded count.
    let trimmed = bytes.iter().rposition(|b| *b != b'=').map_or(0, |i| i + 1);
    let body = &bytes[..trimmed];

    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut acc_bits: u8 = 0;
    for (idx, &c) in body.iter().enumerate() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => {
                return Err(ToolError::InvalidArgs(format!(
                    "invalid base64 character at position {idx}"
                )));
            }
        };
        acc = (acc << 6) | (v as u32);
        acc_bits += 6;
        if acc_bits >= 8 {
            acc_bits -= 8;
            out.push((acc >> acc_bits) as u8);
            acc &= (1 << acc_bits) - 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_base64_round_trip_simple() {
        // "hello world" → base64
        let bytes = decode_base64("aGVsbG8gd29ybGQ=").unwrap();
        assert_eq!(bytes, b"hello world");
    }

    #[test]
    fn decode_base64_accepts_unpadded() {
        let bytes = decode_base64("aGVsbG8gd29ybGQ").unwrap();
        assert_eq!(bytes, b"hello world");
    }

    #[test]
    fn decode_base64_tolerates_embedded_whitespace() {
        // The LLM might newline-wrap a long string.
        let bytes = decode_base64("aGVs\nbG8g\nd29ybGQ=").unwrap();
        assert_eq!(bytes, b"hello world");
    }

    #[test]
    fn decode_base64_rejects_invalid_char() {
        let err = decode_base64("aGVs!bG8=").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }

    #[test]
    fn decode_payload_rejects_both_fields() {
        let args = UploadArgs {
            filename: "x.txt".into(),
            mime: "text/plain".into(),
            content: Some("a".into()),
            content_base64: Some("YQ==".into()),
        };
        assert!(matches!(
            decode_payload(&args).unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn decode_payload_rejects_neither_field() {
        let args = UploadArgs {
            filename: "x.txt".into(),
            mime: "text/plain".into(),
            content: None,
            content_base64: None,
        };
        assert!(matches!(
            decode_payload(&args).unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn schema_names_match_id() {
        assert_eq!(
            UploadAttachment.id(),
            UploadAttachment.schema().function.name
        );
    }

    #[tokio::test]
    async fn errors_when_no_assistant_turn() {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let ctx = ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: pool,
            s3: Some(std::sync::Arc::new(crate::server::config::S3Config {
                endpoint: "http://127.0.0.1:1".into(),
                region: "us-east-1".into(),
                bucket: "b".into(),
                access_key_env: "UPLOAD_ATTACH_TEST_NOT_SET".into(),
                secret_key_env: "UPLOAD_ATTACH_TEST_NOT_SET".into(),
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
        let err = UploadAttachment
            .run(
                ctx,
                json!({
                    "filename": "x.txt",
                    "mime": "text/plain",
                    "content": "hi",
                }),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("no assistant turn"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn errors_when_s3_not_configured() {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let ctx = ToolContext {
            user_id: "u".into(),
            roles: vec![],
            db: pool,
            s3: None,
            assistant_turn_id: Some("t-1".into()),
            session_id: Some("s-1".into()),
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
        };
        let err = UploadAttachment
            .run(
                ctx,
                json!({
                    "filename": "x.txt",
                    "mime": "text/plain",
                    "content": "hi",
                }),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("not configured"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
