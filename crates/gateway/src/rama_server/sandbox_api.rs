// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `/v1/sandbox/files/{run}/{filename}` — bearer-authed download of a file
//! a sandbox run produced for an API caller.
//!
//! The sandbox tool stores API-path artifacts at
//! `sandbox/<user_id>/<run>/<filename>` in the chat bucket and returns
//! this URL. The handler rebuilds that key from the **authenticated**
//! user id (never the URL), so a token can only ever read its own
//! artifacts even though `<run>` is an unguessable UUID. Bytes are
//! streamed through the gateway — the bucket stays private and no
//! presigned URL is ever minted (mirrors the chat attachment proxy).

use std::sync::Arc;

use rama::http::service::web::extract::State;
use rama::http::{Request, Response, StatusCode, header};

use crate::rama_server::auth::require_bearer;
use crate::rama_server::state::RamaState;
use crate::server::chat_attachments;

const PREFIX: &str = "/v1/sandbox/files/";

pub async fn download(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (parts, _body) = req.into_parts();
    let user = match require_bearer(&state, &parts.headers).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let Some(cfg) = state.config.chat.s3.as_ref() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "attachment storage not configured",
        );
    };

    // Read run + filename from the raw URI, not the Path extractor: rama's
    // router lowercases matched path segments, which would mangle a
    // case-sensitive filename like `Report.pdf` (same reason
    // proxy::retrieve_model parses the URI by hand).
    let tail = parts.uri.path().strip_prefix(PREFIX).unwrap_or_default();
    let Some((run, filename)) = tail.split_once('/') else {
        return err(StatusCode::BAD_REQUEST, "expected <run>/<filename>");
    };
    let run = percent_decode(run);
    let filename = percent_decode(filename);

    // `run` becomes part of the S3 key, so guard it against path traversal /
    // key injection (it's a UUID in practice). `filename` is validated by
    // `chat_attachments::fetch`, plus an explicit traversal check here.
    if run.is_empty() || !run.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return err(StatusCode::BAD_REQUEST, "bad run id");
    }
    if filename.is_empty() || filename.contains('/') || filename.contains("..") {
        return err(StatusCode::BAD_REQUEST, "bad filename");
    }

    // Key scope comes from the authenticated user — not the URL — so one
    // token can't read another user's artifacts.
    let scope = format!("sandbox/{}/{}", user.user_id, run);
    let fetched = match chat_attachments::fetch(cfg, &scope, &filename).await {
        Ok(f) => f,
        Err(chat_attachments::AttachmentError::BadFilename(_)) => {
            return err(StatusCode::BAD_REQUEST, "bad filename");
        }
        Err(e) => {
            tracing::warn!(error = %e, %run, %filename, "sandbox artifact fetch");
            return err(StatusCode::NOT_FOUND, "not found");
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, fetched.mime)
        .header(header::CONTENT_LENGTH, fetched.bytes.len())
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename.replace('"', "")),
        )
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .body(fetched.bytes.into())
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "sandbox artifact response build");
            err(StatusCode::INTERNAL_SERVER_ERROR, "response build")
        })
}

fn err(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(msg.to_string().into())
        .unwrap()
}

/// Percent-decode a path segment. Local copy (the codebase keeps small
/// per-module copies — see `proxy.rs` / `pages::skills`) to avoid widening
/// a big module's API.
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_preserves_case_and_decodes() {
        assert_eq!(percent_decode("Report.pdf"), "Report.pdf");
        assert_eq!(percent_decode("my%20file.PDF"), "my file.PDF");
    }
}
