// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Chat-attachment storage. Each multipart file part from the
//! composer lands at `<key_prefix>/<turn_id>/<filename>` in the
//! configured S3 (or S3-compatible) bucket. The bucket can be
//! fully private with no presign capability on the credentials —
//! every byte that reaches a browser or upstream LLM is fetched
//! server-side via the gateway:
//!
//! - **Browser thumbnails** go through `GET /chat/attachment/
//!   <turn>/<file>`, a cookie-authenticated proxy that streams the
//!   S3 object through.
//! - **`fetch_attachment` tool** reads bytes the same way and (for
//!   images) returns them inline as a `data:` URI in the OpenAI
//!   image_url content part.
//!
//! Configuration lives at `config.chat.s3` (see
//! `server::config::S3Config`). Missing config → uploads return a
//! clear error and the chat handler tells the user attachments
//! aren't enabled, rather than silently dropping the bytes.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use s3::Bucket;
use s3::creds::Credentials;
use s3::error::S3Error;
use s3::region::Region;
use session_core::db as chat_db;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::server::config::S3Config;

/// 30s upload timeout per file. Keeps a single misbehaving file
/// from holding the chat composer's spinner open forever.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum AttachmentError {
    #[error("chat attachments are not configured (set [chat.s3] in gateway.toml)")]
    NotConfigured,
    #[error("missing credential env var `{0}`")]
    MissingCredential(String),
    #[error("filename `{0}` rejected (must not be empty or contain `/`)")]
    BadFilename(String),
    #[error("s3 client: {0}")]
    Client(#[from] S3Error),
    #[error("s3 credentials: {0}")]
    Credentials(#[from] s3::creds::error::CredentialsError),
}

/// One attachment ready to ship: filename + mime + size. The
/// gateway-relative proxy URL gets baked into the marker line by
/// [`marker_line`] at write time (it needs the `turn_id` segment,
/// which lives at the call site). `fetch_attachment` reads bytes
/// server-side via [`fetch`] when the model asks.
pub struct UploadOutcome {
    pub filename: String,
    pub mime: String,
    pub bytes: u64,
}

/// Upload one file part. `turn_id` is the assistant-turn id the
/// composer associated the attachment with; using it as the key
/// segment scopes the object to the conversation row that owns it
/// (so a future cleanup pass can `s3:DeleteObject` everything
/// matching the prefix once a turn is hard-deleted).
pub async fn upload(
    cfg: &S3Config,
    turn_id: &str,
    filename: &str,
    mime: &str,
    bytes: Vec<u8>,
) -> Result<UploadOutcome, AttachmentError> {
    if filename.is_empty() || filename.contains('/') {
        return Err(AttachmentError::BadFilename(filename.to_string()));
    }
    let bucket = open_bucket(cfg)?;
    let key = object_key(&cfg.key_prefix, turn_id, filename);
    let total = bytes.len() as u64;

    let res = tokio::time::timeout(
        UPLOAD_TIMEOUT,
        bucket.put_object_with_content_type(&key, &bytes, mime),
    )
    .await;
    match res {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => return Err(AttachmentError::Client(err)),
        Err(_) => {
            return Err(AttachmentError::Client(S3Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "upload timed out after 30s",
            ))));
        }
    }

    Ok(UploadOutcome {
        filename: filename.to_string(),
        mime: mime.to_string(),
        bytes: total,
    })
}

/// Server-side copy one attachment object from `from_turn`'s key to
/// `to_turn`'s key (same filename). Used by the fork path: the bytes
/// never transit the gateway — S3 duplicates the object internally —
/// so forking a conversation with large attachments stays cheap.
pub async fn copy_object(
    cfg: &S3Config,
    from_turn: &str,
    to_turn: &str,
    filename: &str,
) -> Result<(), AttachmentError> {
    if filename.is_empty() || filename.contains('/') {
        return Err(AttachmentError::BadFilename(filename.to_string()));
    }
    let bucket = open_bucket(cfg)?;
    let from = object_key(&cfg.key_prefix, from_turn, filename);
    let to = object_key(&cfg.key_prefix, to_turn, filename);
    let status = bucket.copy_object_internal(&from, &to).await?;
    if !(200..300).contains(&status) {
        return Err(AttachmentError::Client(S3Error::Io(std::io::Error::other(
            format!("s3 COPY returned status {status}"),
        ))));
    }
    Ok(())
}

fn open_bucket(cfg: &S3Config) -> Result<Bucket, AttachmentError> {
    let access = cfg
        .access_key()
        .ok_or_else(|| AttachmentError::MissingCredential(cfg.access_key_env.clone()))?;
    let secret = cfg
        .secret_key()
        .ok_or_else(|| AttachmentError::MissingCredential(cfg.secret_key_env.clone()))?;
    let creds = Credentials::new(Some(&access), Some(&secret), None, None, None)?;
    let region = Region::Custom {
        region: cfg.region.clone(),
        endpoint: cfg.endpoint.clone(),
    };
    let bucket = Bucket::new(&cfg.bucket, region, creds)?.with_path_style();
    Ok(*bucket)
}

/// Object key inside the bucket: `<key_prefix>/<turn_id>/<filename>`.
/// Exposed so the chat-attachment-view route (which presigns a
/// fresh URL on every render so the chat bubble's `<img src>` stays
/// alive past the 1 h upload-time presign) can derive the same key.
pub fn object_key(prefix: &str, turn_id: &str, filename: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        format!("{turn_id}/{filename}")
    } else {
        format!("{prefix}/{turn_id}/{filename}")
    }
}

/// Bytes + content-type for an attachment, fetched server-side via
/// the S3 client (the bucket is never reached directly from a
/// browser or from the upstream LLM). Used by both the chat-bubble
/// proxy route and the `fetch_attachment` tool.
pub struct FetchedAttachment {
    pub bytes: Vec<u8>,
    /// `Content-Type` the bucket served the object with. Falls back
    /// to `"application/octet-stream"` when missing.
    pub mime: String,
}

/// GET the object at `<key_prefix>/<turn_id>/<filename>` and return
/// the raw bytes + content-type. Errors with [`AttachmentError::Client`]
/// for any non-2xx response so the caller can surface a clean tool
/// error to the model.
pub async fn fetch(
    cfg: &S3Config,
    turn_id: &str,
    filename: &str,
) -> Result<FetchedAttachment, AttachmentError> {
    if filename.is_empty() || filename.contains('/') {
        return Err(AttachmentError::BadFilename(filename.to_string()));
    }
    let bucket = open_bucket(cfg)?;
    let key = object_key(&cfg.key_prefix, turn_id, filename);
    let resp = bucket
        .get_object(&key)
        .await
        .map_err(AttachmentError::Client)?;
    let status = resp.status_code();
    if !(200..300).contains(&status) {
        return Err(AttachmentError::Client(S3Error::Io(std::io::Error::other(
            format!("s3 GET returned status {status}"),
        ))));
    }
    let mime = resp
        .headers()
        .into_iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v)
        .unwrap_or_else(|| "application/octet-stream".to_string());
    Ok(FetchedAttachment {
        bytes: resp.to_vec(),
        mime,
    })
}

/// One attachment discovered in a session's turns, addressed by the
/// same opaque `<turn_id>/<filename>` id the model sees in replay
/// stubs. Produced by [`list_session_attachments`] / [`round_attachments`]
/// so the sandbox tools can stage uploaded files into `/work` and
/// advertise what else is reachable by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRef {
    /// `<turn_id>/<filename>` — the id the model passes to pull this file.
    pub id: String,
    pub turn_id: String,
    pub filename: String,
    pub mime: String,
    pub size: u64,
}

/// The opaque `<turn_id>/<filename>` id for a marker. Files are keyed
/// in S3 under the turn whose row carries the marker, so the id is the
/// owning turn id plus the marker's (raw, un-encoded) filename — the
/// same shape `fetch_attachment` splits back apart.
fn marker_id(turn_id: &str, att: &ParsedAttachment) -> String {
    format!("{turn_id}/{}", att.filename)
}

/// Parse the markers in one column into [`AttachmentRef`]s under
/// `turn_id`, appending to `out` and skipping ids already present (a
/// preview marker and the file it links to can both appear).
fn collect_markers(out: &mut Vec<AttachmentRef>, turn_id: &str, text: &str) {
    for att in parse_markers(text) {
        let id = marker_id(turn_id, &att);
        if out.iter().any(|r: &AttachmentRef| r.id == id) {
            continue;
        }
        out.push(AttachmentRef {
            id,
            turn_id: turn_id.to_string(),
            filename: att.filename,
            mime: att.mime,
            size: att.size,
        });
    }
}

/// All attachments referenced anywhere in `turns`, addressed by opaque
/// id. Walks both marker-bearing columns: user uploads land in a user
/// turn's `user_content`, tool-produced files (typst, sandbox, …) land
/// in an assistant turn's `content`.
fn collect_all(turns: &[chat_db::TurnWithTools]) -> Vec<AttachmentRef> {
    let mut out = Vec::new();
    for t in turns {
        for text in [t.turn.user_content.as_deref(), t.turn.content.as_deref()]
            .into_iter()
            .flatten()
        {
            collect_markers(&mut out, &t.turn.id, text);
        }
    }
    out
}

/// The current round's uploads: the most recent `user`-role turn's
/// `user_content` markers (empty if it carried none).
fn collect_round(turns: &[chat_db::TurnWithTools]) -> Vec<AttachmentRef> {
    let Some(t) = turns
        .iter()
        .rev()
        .find(|t| t.turn.role == chat_db::TurnRole::User)
    else {
        return Vec::new();
    };
    let Some(text) = t.turn.user_content.as_deref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_markers(&mut out, &t.turn.id, text);
    out
}

async fn session_turns(
    db: &chat_db::Pool,
    session_id: &str,
) -> Result<Vec<chat_db::TurnWithTools>, AttachmentError> {
    chat_db::list_turns(db, session_id)
        .await
        .map_err(|e| AttachmentError::Client(S3Error::Io(std::io::Error::other(e.to_string()))))
}

/// Every attachment referenced anywhere in a session's turns. The
/// returned ids are session-scoped by construction — only this
/// session's turns are read — so callers can treat presence in this
/// list as proof a given id belongs to the session.
pub async fn list_session_attachments(
    db: &chat_db::Pool,
    session_id: &str,
) -> Result<Vec<AttachmentRef>, AttachmentError> {
    Ok(collect_all(&session_turns(db, session_id).await?))
}

/// The current round's uploaded files: the attachments on the most
/// recent `user`-role turn. This is what a just-sent message carried —
/// the files the user most likely wants the model to work on — and it's
/// the same target whether the turn was a fresh send, a retry, or an
/// edit (all leave the relevant user turn as the latest one).
pub async fn round_attachments(
    db: &chat_db::Pool,
    session_id: &str,
) -> Result<Vec<AttachmentRef>, AttachmentError> {
    Ok(collect_round(&session_turns(db, session_id).await?))
}

/// Both views in one query: every session attachment plus the current
/// round's subset. The hot `run_in_sandbox` staging path needs both, so
/// this avoids walking the session's turns twice.
pub async fn session_and_round_attachments(
    db: &chat_db::Pool,
    session_id: &str,
) -> Result<(Vec<AttachmentRef>, Vec<AttachmentRef>), AttachmentError> {
    let turns = session_turns(db, session_id).await?;
    Ok((collect_all(&turns), collect_round(&turns)))
}

/// Whether `id` names an attachment present in the enumerated session
/// set. The session enumeration only reads this session's turns, so a
/// hit proves the id belongs to the caller's conversation — the gate
/// the sandbox tools apply before fetching a model-supplied id.
pub fn attachment_in_session(session: &[AttachmentRef], id: &str) -> bool {
    session.iter().any(|a| a.id == id)
}

/// Three-way classification of bytes-with-a-mime, shared between
/// `fetch_attachment` and `fetch_url`. The model sees one of three
/// shapes regardless of which tool produced the bytes:
///
/// * **Text** — UTF-8 decoded `content`, with truncation accounting.
///   Applies to `text/*` mimes + known structured-data file types
///   (csv/json/yaml/code/etc.). The model reads it inline.
/// * **Image** — `data:<mime>;base64,<…>` URI. The caller wraps it
///   in `tool_content_parts(...)` so the upstream LLM gets a real
///   `image_url` content part it can look at.
/// * **Binary** — metadata only. Other binary content (PDF, zip,
///   audio, …) has no good way to ride a tool result, so we tell
///   the model what we found and stop there. Over-cap images
///   degrade to this variant too — the caller can detect that case
///   via the `mime` it passed in.
pub enum BinaryDisposition {
    Text {
        content: String,
        bytes_returned: usize,
        truncated: bool,
        original_len: usize,
    },
    Image {
        data_uri: String,
        original_len: usize,
    },
    Binary {
        original_len: usize,
    },
}

/// Per-classification limits. Text + image have very different
/// scaling (text is read directly by the model, image is base64'd
/// and shipped as a vision input) so they take separate caps.
pub struct PayloadLimits {
    /// Max UTF-8 bytes to return as `content` for text-ish payloads.
    /// Larger payloads are truncated and the result carries
    /// `truncated: true` so the model can decide whether to ask for
    /// the rest with a tighter `max_bytes`.
    pub max_text_bytes: usize,
    /// Max raw bytes we'll base64-encode into an image data URI.
    /// Above this an image degrades to [`BinaryDisposition::Binary`]
    /// (the caller surfaces a "too large to inline" note); the
    /// upstream LLM provider's own image-size limits + the
    /// gateway's per-request memory ceiling sit somewhere here.
    pub max_image_bytes: usize,
}

/// Classify fetched bytes into one of [`BinaryDisposition`]'s
/// variants. `filename` is consulted only for `is_inline_text`'s
/// extension-fallback heuristic — pass `""` when there's no
/// filename (URL fetches usually don't have one, and the mime is
/// the primary signal anyway).
pub fn classify_payload(
    mime: &str,
    filename: &str,
    bytes: Vec<u8>,
    limits: PayloadLimits,
) -> BinaryDisposition {
    let original_len = bytes.len();
    if is_inline_text(mime, filename) {
        let truncated = original_len > limits.max_text_bytes;
        let slice = if truncated {
            &bytes[..limits.max_text_bytes]
        } else {
            &bytes[..]
        };
        let content = String::from_utf8_lossy(slice).into_owned();
        return BinaryDisposition::Text {
            content,
            bytes_returned: slice.len(),
            truncated,
            original_len,
        };
    }
    if mime.starts_with("image/") && original_len <= limits.max_image_bytes {
        return BinaryDisposition::Image {
            data_uri: to_data_uri(mime, &bytes),
            original_len,
        };
    }
    BinaryDisposition::Binary { original_len }
}

/// Encode bytes as a `data:<mime>;base64,<…>` URI. Used by
/// `fetch_attachment`'s image branch so the upstream LLM gets the
/// image inline as a typed `image_url` content part — no presigned
/// URL needed, no network access from the upstream to the gateway.
/// OpenAI Chat Completions accepts `data:` URIs in `image_url`.
pub fn to_data_uri(mime: &str, bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 32 + mime.len());
    out.push_str("data:");
    out.push_str(mime);
    out.push_str(";base64,");
    append_base64(&mut out, bytes);
    out
}

/// RFC 4648 base64 encoder (standard alphabet, padded). Hand-rolled
/// to keep us off a direct `base64` dep — pairs with the matching
/// decoder over in `tools::upload_attachment` so we're symmetric on
/// the encode/decode pair without pulling in 200 KB of crate.
fn append_base64(out: &mut String, bytes: &[u8]) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() >= 2 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() >= 3 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
}

// Marker parsing + format helpers are shared with the chat renderer
// in `session-core::attachments`. Re-exported here so existing
// gateway call sites keep working.
pub use session_core::attachments::{
    ParsedAttachment, dedupe_filename, is_inline_text, is_pdf, parse_markers,
    strip_markers_for_replay,
};

/// Reserve a unique filename for an upload that's about to land in
/// the current turn. Holds the per-turn `reservations` mutex while
/// it reads the turn's already-committed markers, folds in the
/// in-flight reservation set, picks a free name, and inserts it —
/// so concurrent tool calls (typst + upload_attachment, or two
/// typst calls in parallel) each get a distinct name instead of
/// trampling each other in S3.
///
/// On the chat path `reservations` is always `Some`. On proxy
/// paths it would be `None`, but the only callers that upload
/// (`typst_*`, `upload_attachment`) refuse to run there because
/// `assistant_turn_id` is also `None`, so this branch never fires
/// in practice.
pub async fn reserve_filename(
    db: &chat_db::Pool,
    turn_id: &str,
    reservations: &Mutex<HashSet<String>>,
    desired: &str,
) -> Result<String, AttachmentError> {
    let mut taken = reservations.lock().await;
    let existing = chat_db::get_content(db, turn_id)
        .await
        .map_err(|e| AttachmentError::Client(S3Error::Io(std::io::Error::other(e.to_string()))))?
        .unwrap_or_default();
    let mut used = session_core::attachments::existing_filenames(&existing);
    used.extend(taken.iter().cloned());
    let chosen = session_core::attachments::dedupe_filename_against(&used, desired);
    taken.insert(chosen.clone());
    Ok(chosen)
}

/// Trio sibling of [`reserve_filename`]: pick a single stem such
/// that `{stem}.{ext}` is free for every `ext` (typst writes
/// `.pdf` + `.png` + `.typ` together and the model expects them to
/// share a stem). Inserts every reserved name into the set so a
/// parallel `upload_attachment` can't claim e.g. `chart.png` while
/// the typst render is still in flight.
pub async fn reserve_basename(
    db: &chat_db::Pool,
    turn_id: &str,
    reservations: &Mutex<HashSet<String>>,
    base: &str,
    exts: &[&str],
) -> Result<String, AttachmentError> {
    let mut taken = reservations.lock().await;
    let existing = chat_db::get_content(db, turn_id)
        .await
        .map_err(|e| AttachmentError::Client(S3Error::Io(std::io::Error::other(e.to_string()))))?
        .unwrap_or_default();
    let mut used = session_core::attachments::existing_filenames(&existing);
    used.extend(taken.iter().cloned());
    let chosen = session_core::attachments::dedupe_basename_against(&used, base, exts);
    for ext in exts {
        taken.insert(format!("{chosen}.{ext}"));
    }
    Ok(chosen)
}

/// Fresh per-turn reservation set for [`crate::server::tools::
/// ToolContext::attachment_reservations`]. One allocation per
/// assistant turn, dropped when the turn finishes — the set is
/// scoped to a single turn's tool-call rounds, not the conversation.
pub fn new_reservation_set() -> Arc<Mutex<HashSet<String>>> {
    Arc::new(Mutex::new(HashSet::new()))
}

/// Build the canonical marker line for a freshly-uploaded file
/// under `turn_id`. The marker's `url` field carries the
/// gateway-relative proxy route (`/chat/attachment/<turn>/<file>`)
/// so chat-bubble renderers — page-render and SSE-tick alike —
/// can drop it straight into `<img src>` / chip hrefs without any
/// rewriting layer. The LLM-side replay path rewrites the marker
/// to an opaque-id stub regardless of what's in `url`.
///
/// Filenames / mimes with embedded `"` get the inner quote stripped
/// — keeps the parser simple at the cost of a (rare) name collision.
pub fn marker_line(turn_id: &str, att: &UploadOutcome) -> String {
    session_core::attachments::marker_line(
        &att.filename,
        &att.mime,
        &proxy_url(turn_id, &att.filename),
        att.bytes,
    )
}

/// Marker for a *preview* attachment whose click-through opens a
/// different file. `link` is the full marker `link="…"` target (a
/// gateway proxy URL) — e.g. a typst render's PNG preview links to its
/// PDF so clicking the inline image opens the document, not a bigger
/// copy of the preview image.
pub fn marker_line_linked(turn_id: &str, att: &UploadOutcome, link: &str) -> String {
    session_core::attachments::marker_line_linked(
        &att.filename,
        &att.mime,
        &proxy_url(turn_id, &att.filename),
        att.bytes,
        Some(link),
    )
}

/// The gateway-relative URL the chat bubble's `<img>` / chip hrefs
/// point at for one uploaded attachment. The handler at this path
/// gates on the session cookie + verifies the turn belongs to the
/// caller before pulling the bytes from S3.
pub fn proxy_url(turn_id: &str, filename: &str) -> String {
    format!(
        "/chat/attachment/{turn_id}/{}",
        urlencode_path_segment(filename),
    )
}

/// Percent-encode a path segment so filenames with spaces / `?` /
/// `#` / `%` don't punch out of the URL path. Conservative encoding
/// per RFC 3986 unreserved set; the upload path already rejects `/`
/// so the segment is one path component.
fn urlencode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_assembles_prefix_turn_filename() {
        assert_eq!(
            object_key("chat-attachments", "t-1", "data.csv"),
            "chat-attachments/t-1/data.csv"
        );
    }

    #[test]
    fn object_key_tolerates_stray_slashes_on_prefix() {
        assert_eq!(object_key("/chat/", "t-1", "data.csv"), "chat/t-1/data.csv");
    }

    #[test]
    fn object_key_handles_empty_prefix() {
        assert_eq!(object_key("", "t-1", "data.csv"), "t-1/data.csv");
    }

    // Marker parsing / strip / inline-text tests live in
    // `session_core::attachments` now; the helpers here are thin
    // re-export shims.

    #[test]
    fn is_inline_text_covers_common_data_formats() {
        assert!(is_inline_text("text/csv", "x.csv"));
        assert!(is_inline_text("application/json", "x.json"));
        assert!(is_inline_text("application/octet-stream", "schema.sql"));
        assert!(!is_inline_text("image/png", "x.png"));
        assert!(!is_inline_text("application/pdf", "x.pdf"));
    }

    #[test]
    fn base64_encodes_canonical_examples() {
        // Pulled from RFC 4648 §10 test vectors.
        let cases: &[(&[u8], &str)] = &[
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (input, expected) in cases {
            let mut out = String::new();
            append_base64(&mut out, input);
            assert_eq!(&out, expected, "input={input:?}");
        }
    }

    #[test]
    fn data_uri_prefixes_mime_and_encodes() {
        let uri = to_data_uri("image/png", &[0xFF, 0xD8, 0xFF]);
        // 3 bytes → 4 base64 chars; FF D8 FF = /9j/ in base64.
        assert_eq!(uri, "data:image/png;base64,/9j/");
    }

    #[test]
    fn proxy_url_format_matches_route() {
        assert_eq!(
            proxy_url("t-abc", "chart.png"),
            "/chat/attachment/t-abc/chart.png"
        );
    }

    #[test]
    fn proxy_url_percent_encodes_filename() {
        // Spaces, `?`, `#`, `%`, `&` must not punch holes in the URL.
        assert_eq!(
            proxy_url("t-1", "my file?.png"),
            "/chat/attachment/t-1/my%20file%3F.png"
        );
        assert_eq!(proxy_url("t-1", "a#b"), "/chat/attachment/t-1/a%23b");
        assert_eq!(
            proxy_url("t-1", "100%.csv"),
            "/chat/attachment/t-1/100%25.csv"
        );
    }

    #[test]
    fn marker_line_bakes_proxy_url() {
        let outcome = UploadOutcome {
            filename: "chart.png".into(),
            mime: "image/png".into(),
            bytes: 42,
        };
        let m = marker_line("t-abc", &outcome);
        assert!(
            m.contains(r#"url="/chat/attachment/t-abc/chart.png""#),
            "{m}"
        );
    }

    async fn seed_turn(pool: &chat_db::Pool, turn_id: &str, content: &str) {
        sqlx::query(
            r#"INSERT INTO users (id, email, created_at, updated_at)
               VALUES ('u1', 'u1@example.com', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')
               ON CONFLICT(id) DO NOTHING"#,
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO chat_sessions (id, user_id, created_at, updated_at)
               VALUES ('s1', 'u1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')
               ON CONFLICT(id) DO NOTHING"#,
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO chat_turns
                 (id, session_id, seq, role, content, status, created_at)
               VALUES (?, 's1', 0, 'assistant', ?, 'in_progress',
                       '2026-01-01T00:00:00Z')"#,
        )
        .bind(turn_id)
        .bind(content)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Seed one turn with explicit role/seq and column contents.
    /// `user_content` carries user-upload markers; `content` carries
    /// tool-produced markers — the same split the live code uses.
    async fn seed_turn_full(
        pool: &chat_db::Pool,
        turn_id: &str,
        seq: i64,
        role: &str,
        user_content: Option<&str>,
        content: Option<&str>,
    ) {
        sqlx::query(
            r#"INSERT INTO users (id, email, created_at, updated_at)
               VALUES ('u1', 'u1@example.com', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')
               ON CONFLICT(id) DO NOTHING"#,
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO chat_sessions (id, user_id, created_at, updated_at)
               VALUES ('s1', 'u1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')
               ON CONFLICT(id) DO NOTHING"#,
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO chat_turns
                 (id, session_id, seq, role, user_content, content, status, created_at)
               VALUES (?, 's1', ?, ?, ?, ?, 'completed', '2026-01-01T00:00:00Z')"#,
        )
        .bind(turn_id)
        .bind(seq)
        .bind(role)
        .bind(user_content)
        .bind(content)
        .execute(pool)
        .await
        .unwrap();
    }

    fn marker_for(turn_id: &str, filename: &str, mime: &str, size: u64) -> String {
        marker_line(
            turn_id,
            &UploadOutcome {
                filename: filename.into(),
                mime: mime.into(),
                bytes: size,
            },
        )
    }

    #[tokio::test]
    async fn list_session_attachments_reads_both_columns_and_dedupes() {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        // User turn: an upload in `user_content`.
        let user_marker = marker_for("t-user", "deck.pptx", "application/vnd.ms-powerpoint", 1234);
        seed_turn_full(&pool, "t-user", 0, "user", Some(&user_marker), None).await;
        // Assistant turn: a produced file in `content`, listed twice
        // (a dupe id must collapse to one entry).
        let prod = marker_for("t-asst", "chart.png", "image/png", 99);
        let content = format!("{prod}\n\n{prod}");
        seed_turn_full(&pool, "t-asst", 1, "assistant", None, Some(&content)).await;

        let atts = list_session_attachments(&pool, "s1").await.unwrap();
        let ids: Vec<&str> = atts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["t-user/deck.pptx", "t-asst/chart.png"],
            "{atts:?}"
        );
        let deck = atts.iter().find(|a| a.filename == "deck.pptx").unwrap();
        assert_eq!(deck.turn_id, "t-user");
        assert_eq!(deck.size, 1234);
        assert!(attachment_in_session(&atts, "t-user/deck.pptx"));
        assert!(!attachment_in_session(&atts, "t-other/secret.pptx"));
    }

    #[tokio::test]
    async fn round_attachments_picks_latest_user_turn() {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        // An older user turn with a file, then a newer user turn with a
        // different file. The round = the newest user turn's uploads.
        let old = marker_for("t-u1", "old.pptx", "application/vnd.ms-powerpoint", 1);
        seed_turn_full(&pool, "t-u1", 0, "user", Some(&old), None).await;
        seed_turn_full(&pool, "t-a1", 1, "assistant", None, None).await;
        let new = marker_for("t-u2", "new.pptx", "application/vnd.ms-powerpoint", 2);
        seed_turn_full(&pool, "t-u2", 2, "user", Some(&new), None).await;

        let round = round_attachments(&pool, "s1").await.unwrap();
        let ids: Vec<&str> = round.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["t-u2/new.pptx"], "{round:?}");
    }

    #[tokio::test]
    async fn round_attachments_empty_when_latest_user_turn_has_no_files() {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        seed_turn_full(&pool, "t-u1", 0, "user", Some("just text, no files"), None).await;
        let round = round_attachments(&pool, "s1").await.unwrap();
        assert!(round.is_empty(), "{round:?}");
    }

    #[tokio::test]
    async fn reserve_filename_picks_distinct_names_for_concurrent_calls() {
        // The regression this guards against: two `upload_attachment`
        // (or two `typst_*`) calls in one round both ask for the same
        // filename, `execute_tool_calls` runs them in parallel via
        // `join_all`, and without the per-turn reservation set both
        // read the same pre-upload `content`, pick the same name, and
        // the second `put_object` overwrites the first.
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        seed_turn(&pool, "t-1", "").await;
        let reservations = new_reservation_set();
        let futs = (0..4).map(|_| {
            let pool = pool.clone();
            let reservations = reservations.clone();
            async move {
                reserve_filename(&pool, "t-1", &reservations, "chart.png")
                    .await
                    .unwrap()
            }
        });
        let names: Vec<String> = rama::futures::future::join_all(futs).await;
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 4, "reservations must be unique: {names:?}");
        assert!(names.contains(&"chart.png".to_string()), "{names:?}");
        // Suffixed names are 2..=4, in some order.
        for n in 2..=4 {
            assert!(
                names.contains(&format!("chart-{n}.png")),
                "missing chart-{n}.png in {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn reserve_filename_folds_in_committed_markers() {
        // The committed `content` already names `chart.png`; one fresh
        // reservation must skip it even when the in-flight set is empty.
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        let marker = marker_line(
            "t-1",
            &UploadOutcome {
                filename: "chart.png".into(),
                mime: "image/png".into(),
                bytes: 1,
            },
        );
        seed_turn(&pool, "t-1", &marker).await;
        let reservations = new_reservation_set();
        let got = reserve_filename(&pool, "t-1", &reservations, "chart.png")
            .await
            .unwrap();
        assert_eq!(got, "chart-2.png");
    }

    #[tokio::test]
    async fn reserve_basename_keeps_trio_distinct_under_concurrency() {
        // Mirror the typst scenario: two parallel calls each want the
        // {base}.pdf+png+typ trio. Each must walk away with a unique
        // stem.
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        seed_turn(&pool, "t-1", "").await;
        let reservations = new_reservation_set();
        let exts: &[&str] = &["pdf", "png", "typ"];
        let futs = (0..3).map(|_| {
            let pool = pool.clone();
            let reservations = reservations.clone();
            async move {
                reserve_basename(&pool, "t-1", &reservations, "letter", exts)
                    .await
                    .unwrap()
            }
        });
        let stems: Vec<String> = rama::futures::future::join_all(futs).await;
        let mut sorted = stems.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, vec!["letter", "letter-2", "letter-3"]);
    }
}
