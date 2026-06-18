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
    ParsedAttachment, dedupe_filename, is_inline_text, parse_markers, strip_markers_for_replay,
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
