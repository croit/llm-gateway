// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Code-execution sandbox tools.
//!
//! The model writes Python or shell; the gateway forwards it to the
//! standalone `sandbox-runner` service, which executes it inside an
//! ephemeral, single-use sandbox and returns stdout/stderr plus any
//! files the run produced. The runner enforces the real isolation
//! (gVisor boundary, default-deny network behind an egress allowlist,
//! resource caps); the gateway only does the tool plumbing.
//!
//! Three tools are registered when `[sandbox]` is configured:
//!   - `run_in_sandbox` — the generic escape hatch (any python/bash).
//!   - `generate_document` — markdown → pdf/docx/pptx via pandoc (a thin,
//!     injection-safe preset over the generic path).
//!   - `capture_webpage` — headless-chromium screenshot/pdf/text of a URL
//!     (needs runner egress).
//!
//! Produced files are delivered two ways, matching where the call came
//! from: on the chat page they're uploaded and spliced inline as chat
//! attachments; on the `/v1` API path they're stored per-user and the
//! result carries a bearer-authed download URL (see
//! `rama_server::sandbox_api`). With no `[chat.s3]` configured, files are
//! reported as metadata only.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use session_core::db as chat;
use shared::api::ToolDef;
use shared::sandbox::{Artifact, InputFile, Language, RunError, RunRequest, RunResponse};

use super::{Tool, ToolContext, ToolError, ToolFuture};
use crate::server::chat_attachments::{self, AttachmentRef};
use crate::server::config::SandboxConfig;

/// Shared HTTP client + config for the sandbox tool family. Held behind an
/// `Arc` so the generic tool and each specialized wrapper share one
/// connection pool.
pub struct SandboxClient {
    cfg: Arc<SandboxConfig>,
    /// Gateway public base URL, for building absolute artifact download
    /// links on the API path. Cloned from `config.gateway.public_url`.
    public_url: String,
    http: reqwest::Client,
}

impl SandboxClient {
    pub fn new(cfg: Arc<SandboxConfig>, public_url: String) -> Arc<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .user_agent(concat!(
                "llm-gateway/",
                env!("CARGO_PKG_VERSION"),
                " sandbox"
            ))
            .build()
            .unwrap_or_default();
        Arc::new(Self {
            cfg,
            public_url,
            http,
        })
    }

    /// Wall-clock ceiling the tool runner should allow around a sandbox
    /// call: the HTTP timeout plus margin, so the client's own timeout
    /// (producing a clean error) fires before the loop cancels the future.
    fn loop_timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.timeout_secs.saturating_add(15))
    }

    /// POST one job to the runner and decode the result.
    async fn call_runner(&self, req: &RunRequest) -> Result<RunResponse, ToolError> {
        let url = format!("{}/run", self.cfg.runner_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(|e| ToolError::Failed(format!("sandbox runner unreachable: {e}")))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ToolError::Failed(format!("reading runner response: {e}")))?;
        if !status.is_success() {
            let msg = serde_json::from_slice::<RunError>(&bytes)
                .map(|e| e.error)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).trim().to_string());
            return Err(ToolError::Failed(format!("sandbox runner {status}: {msg}")));
        }
        serde_json::from_slice::<RunResponse>(&bytes)
            .map_err(|e| ToolError::Failed(format!("runner response not a RunResponse: {e}")))
    }

    /// Run a job and hand back the raw [`RunResponse`] (exit code,
    /// streams, produced artifacts) without delivering anything to the
    /// chat. Callers that want to attach a specific produced file under
    /// their own naming/dedup — e.g. the typst tools wiring a `.pptx`
    /// into a render's attachment cluster — use this instead of
    /// [`Self::execute`], which auto-attaches every artifact.
    pub async fn run_job(&self, req: RunRequest) -> Result<RunResponse, ToolError> {
        self.call_runner(&req).await
    }

    /// Run a job and shape the model-facing result, delivering any
    /// produced files appropriately for the call's context.
    async fn execute(&self, ctx: &ToolContext, req: RunRequest) -> Result<Value, ToolError> {
        let resp = self.call_runner(&req).await?;
        let artifacts = self.deliver_artifacts(ctx, &resp.artifacts).await;
        // If any file was actually attached inline (chat path), tell the
        // model not to echo the marker text. Derived from the delivery
        // results so we don't re-scan the raw artifacts.
        let any_attached = artifacts
            .iter()
            .any(|a| a.get("status").and_then(Value::as_str) == Some("attached"));

        // Pointers-as-context: when a stream was large the runner's agent
        // preserved the FULL text as a stdout.txt/stderr.txt artifact. Rather
        // than inlining the whole (already runner-capped) stream and bloating
        // the context, return a small preview + a handle the model can
        // grep/read on demand via `read_sandbox_output`. The delivered
        // artifacts list is index-aligned with `resp.artifacts`, so we look
        // up each stream's stored entry by position (robust to filename
        // de-duplication like stdout-2.txt).
        let ref_for = |name: &str| -> Option<&Value> {
            resp.artifacts
                .iter()
                .position(|a| a.name == name)
                .and_then(|i| artifacts.get(i))
        };
        let stdout = shape_stream(&resp.stdout, ref_for("stdout.txt"));
        let stderr = shape_stream(&resp.stderr, ref_for("stderr.txt"));

        let mut out = json!({
            "exit_code": resp.exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "timed_out": resp.timed_out,
            "output_truncated": resp.output_truncated,
            "duration_ms": resp.duration_ms,
            "artifacts": artifacts,
        });
        if any_attached {
            out["note"] = json!(
                "Produced files are now attached inline in your message — do NOT \
                 repeat any marker/URL text in your prose."
            );
        }
        Ok(out)
    }

    /// Store each artifact and describe where it landed. Never fails the
    /// whole tool call: a per-file problem is reported in that file's
    /// entry so the model still sees stdout/stderr.
    async fn deliver_artifacts(&self, ctx: &ToolContext, artifacts: &[Artifact]) -> Vec<Value> {
        let mut out = Vec::with_capacity(artifacts.len());
        for a in artifacts {
            if a.size > self.cfg.max_artifact_bytes {
                out.push(json!({
                    "name": a.name, "size": a.size, "mime": a.mime, "status": "dropped",
                    "note": format!("exceeds max_artifact_bytes ({})", self.cfg.max_artifact_bytes),
                }));
                continue;
            }
            let Some(bytes) = b64::decode(&a.content_b64) else {
                out.push(
                    json!({"name": a.name, "status": "error", "note": "artifact base64 invalid"}),
                );
                continue;
            };
            let entry = match (
                &ctx.assistant_turn_id,
                &ctx.s3,
                &ctx.attachment_reservations,
            ) {
                (Some(turn), Some(s3), Some(res)) => {
                    self.deliver_chat(ctx, turn, s3, res, a, bytes).await
                }
                (None, Some(s3), _) => self.deliver_api(ctx, s3, a, bytes).await,
                _ => Ok(json!({
                    "name": a.name, "size": a.size, "mime": a.mime, "status": "not_stored",
                    "note": "no attachment storage configured ([chat.s3]); file was produced but not retained",
                })),
            };
            out.push(
                entry.unwrap_or_else(|e| json!({"name": a.name, "status": "error", "note": e})),
            );
        }
        out
    }

    /// Chat path: upload + splice an inline attachment marker, exactly like
    /// the typst tool, so the file shows in the message bubble.
    async fn deliver_chat(
        &self,
        ctx: &ToolContext,
        turn: &str,
        s3: &crate::server::config::S3Config,
        reservations: &tokio::sync::Mutex<std::collections::HashSet<String>>,
        a: &Artifact,
        bytes: Vec<u8>,
    ) -> Result<Value, String> {
        let filename = chat_attachments::reserve_filename(&ctx.db, turn, reservations, &a.name)
            .await
            .map_err(|e| e.to_string())?;
        let outcome = chat_attachments::upload(s3, turn, &filename, &a.mime, bytes)
            .await
            .map_err(|e| e.to_string())?;
        let marker = chat_attachments::marker_line(turn, &outcome);
        chat::append_content(&ctx.db, turn, &format!("\n\n{marker}\n\n"))
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "name": filename, "size": outcome.bytes, "mime": a.mime, "status": "attached",
            "id": format!("{turn}/{filename}"),
        }))
    }

    /// API path: store under `sandbox/<user>/<run>/<file>` and hand back a
    /// bearer-authed download URL. The user segment scopes retrieval to
    /// the owning token (see `sandbox_api::download`).
    async fn deliver_api(
        &self,
        ctx: &ToolContext,
        s3: &crate::server::config::S3Config,
        a: &Artifact,
        bytes: Vec<u8>,
    ) -> Result<Value, String> {
        let safe = sanitize_filename(&a.name).ok_or("unsafe artifact filename")?;
        let run = uuid::Uuid::new_v4().to_string();
        let scope = format!("sandbox/{}/{}", ctx.user_id, run);
        let outcome = chat_attachments::upload(s3, &scope, &safe, &a.mime, bytes)
            .await
            .map_err(|e| e.to_string())?;
        let url = format!(
            "{}/v1/sandbox/files/{}/{}",
            self.public_url.trim_end_matches('/'),
            run,
            urlencode_segment(&safe),
        );
        Ok(json!({
            "name": safe, "size": outcome.bytes, "mime": a.mime, "status": "available",
            "download_url": url,
            "note": "GET this URL with your API bearer token to download the file",
        }))
    }
}

/// Derive a safe filename stem from an optional model-supplied name:
/// strip any extension (the caller appends the format-correct one) and
/// sanitize it, falling back to `default`. Shared by the document /
/// capture wrappers.
fn filename_stem(supplied: Option<&str>, default: &str) -> String {
    supplied
        .and_then(|f| f.rsplit_once('.').map(|(s, _)| s).or(Some(f)))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(sanitize_filename)
        .unwrap_or_else(|| default.to_string())
}

/// Reject path-separators / traversal in a model-supplied filename; the
/// runner sanitizes too, this is defence in depth.
fn sanitize_filename(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return None;
    }
    Some(name.to_string())
}

/// Inline preview budget for a stream whose full text is also stored as an
/// artifact. Deliberately small (~4 KiB ≈ ~1k tokens): the model reads the
/// rest on demand via `read_sandbox_output`.
const STREAM_PREVIEW_BYTES: usize = 4096;

/// Shape one stream for the model. If its full content was preserved as an
/// artifact (`stored` = that artifact's delivery entry), return a compact
/// `{preview, full_output_ref|full_output_url, note}` so the model pulls the
/// rest on demand instead of us inlining the whole thing. Otherwise return
/// the (already runner-capped) stream string as-is.
fn shape_stream(stream: &str, stored: Option<&Value>) -> Value {
    let Some(entry) = stored else {
        return json!(stream);
    };
    let preview = head_tail_preview(stream, STREAM_PREVIEW_BYTES);
    let mut obj = serde_json::Map::new();
    obj.insert("preview".into(), json!(preview));
    obj.insert("truncated".into(), json!(true));
    if let Some(id) = entry.get("id").and_then(Value::as_str) {
        obj.insert("full_output_ref".into(), json!(id));
        obj.insert(
            "note".into(),
            json!(format!(
                "Output is large; only a preview is shown. Call read_sandbox_output \
                 with id=\"{id}\" (action: grep/head/tail/range) to read the rest."
            )),
        );
    } else if let Some(url) = entry.get("download_url").and_then(Value::as_str) {
        obj.insert("full_output_url".into(), json!(url));
        obj.insert(
            "note".into(),
            json!(
                "Output is large; only a preview is shown. GET full_output_url \
                   with your API bearer token for the complete output."
            ),
        );
    }
    Value::Object(obj)
}

/// Keep ~60% head + ~40% tail of `s` within `max` bytes (char-boundary safe),
/// with a marker in between. Head+tail rather than head-only so a trailing
/// error/exit isn't hidden.
fn head_tail_preview(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head = (max * 6 / 10).max(1);
    let tail = max.saturating_sub(head);
    let mut h = head.min(s.len());
    while h > 0 && !s.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = s.len().saturating_sub(tail);
    while t < s.len() && !s.is_char_boundary(t) {
        t += 1;
    }
    if t < h {
        t = h;
    }
    format!(
        "{}\n…[middle omitted — read the full output via read_sandbox_output]…\n{}",
        &s[..h],
        &s[t..]
    )
}

/// Percent-encode a single path segment (RFC 3986 unreserved set kept).
fn urlencode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Attachment staging: pull uploaded chat files into a run's /work.
//
// The model never holds an uploaded file's bytes, so it can't base64
// them into a tool call. Instead the gateway resolves attachment ids
// server-side (scoped to the caller's session) and materializes the
// bytes as binary `InputFile`s. Two sources combine:
//   - the current round's uploads, staged automatically (the common
//     "here's a deck, do X" case); and
//   - any other session attachment the model names by id.
// Chat-path only — the proxy/`/v1` path has no session and no S3-backed
// uploads, so staging is a no-op there (the tool still runs with any
// inline text files).

/// Total bytes of staged attachments allowed into one run's `/work`.
/// Bounds the (base64-inflated) request payload and the runner's disk;
/// files past the budget are skipped with a note rather than silently
/// dropping the model's inputs.
const STAGE_TOTAL_MAX_BYTES: usize = 50 * 1024 * 1024;

/// A file the model asked to pull into the run by attachment id.
#[derive(Deserialize)]
struct AttachmentArg {
    /// `<turn_id>/<filename>` from an attachment replay stub.
    id: String,
    /// Optional override for the name the file gets in `/work`
    /// (defaults to the attachment's own filename).
    #[serde(default)]
    name: Option<String>,
}

/// One resolved file ready to drop into `/work`: its desired name, the
/// id it came from, and the raw bytes. The unit [`assemble_inputs`]
/// consumes — kept separate from the S3 fetch so the dedup/budget
/// logic is pure and unit-testable.
struct StageItem {
    name: String,
    id: String,
    bytes: Vec<u8>,
}

/// Everything staging produced for one run.
struct Staged {
    /// Binary inputs to prepend to the run's `files`.
    files: Vec<InputFile>,
    /// `[{name, id, size}]` — what actually landed in `/work`.
    staged: Vec<Value>,
    /// `[{id, filename, mime, size}]` — other session attachments the
    /// model can pull in by id on a follow-up run.
    available: Vec<Value>,
    /// Human-readable notes (skips, renames) surfaced to the model.
    notes: Vec<String>,
}

/// Pure assembler: dedup names against `/work`, enforce the byte budget,
/// base64 each kept file. Skips (budget) and renames (collision) become
/// notes. No I/O — the caller fetches bytes first.
fn assemble_inputs(
    items: Vec<StageItem>,
    budget: usize,
) -> (Vec<InputFile>, Vec<Value>, Vec<String>) {
    let mut files = Vec::new();
    let mut staged = Vec::new();
    let mut notes = Vec::new();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut total: usize = 0;
    for item in items {
        let size = item.bytes.len();
        if total.saturating_add(size) > budget {
            notes.push(format!(
                "Skipped staging `{}` ({size} bytes): the {budget}-byte input budget for this \
                 run would be exceeded. Work on fewer/smaller files per run.",
                item.name,
            ));
            continue;
        }
        let name = session_core::attachments::dedupe_filename_against(&used, &item.name);
        if name != item.name {
            notes.push(format!(
                "Staged `{}` as `{name}` to avoid a filename collision in /work.",
                item.name,
            ));
        }
        used.insert(name.clone());
        total += size;
        files.push(InputFile {
            name: name.clone(),
            content_b64: b64::encode(&item.bytes),
        });
        staged.push(json!({"name": name, "id": item.id, "size": size}));
    }
    (files, staged, notes)
}

/// Resolve + fetch the round's uploads (always) plus any model-named ids
/// (validated against the session), and assemble them for `/work`.
/// Returns an empty [`Staged`] on paths without a session (`/v1`).
async fn stage_attachments(
    ctx: &ToolContext,
    explicit: &[AttachmentArg],
) -> Result<Staged, ToolError> {
    let empty = || Staged {
        files: vec![],
        staged: vec![],
        available: vec![],
        notes: vec![],
    };
    let (Some(session_id), Some(s3)) = (ctx.session_id.as_deref(), ctx.s3.as_ref()) else {
        // No session (proxy/`/v1`) or no attachment storage configured:
        // nothing to stage. If the model named ids anyway, say why they
        // were ignored rather than failing the whole run.
        if explicit.is_empty() {
            return Ok(empty());
        }
        let mut s = empty();
        s.notes.push(
            "Attachments can't be staged on this path (no chat session / attachment storage). \
             Ran without them."
                .into(),
        );
        return Ok(s);
    };

    let (session_atts, round) =
        chat_attachments::session_and_round_attachments(&ctx.db, session_id)
            .await
            .map_err(|e| ToolError::Failed(format!("listing session attachments: {e}")))?;

    // Build the to-stage list: round uploads first, then explicit ids.
    // De-dupe by id so a file named explicitly *and* in the round is
    // staged once. `desired` is the explicit `name` override or the
    // attachment's own filename.
    let mut want: Vec<(String, Option<String>)> = Vec::new(); // (id, name-override)
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for a in &round {
        if seen_ids.insert(a.id.clone()) {
            want.push((a.id.clone(), None));
        }
    }
    let mut notes: Vec<String> = Vec::new();
    for arg in explicit {
        if !chat_attachments::attachment_in_session(&session_atts, &arg.id) {
            notes.push(format!(
                "Ignored attachment id `{}`: not found in this conversation.",
                arg.id,
            ));
            continue;
        }
        if seen_ids.insert(arg.id.clone()) {
            want.push((arg.id.clone(), arg.name.clone()));
        } else if let Some(n) = &arg.name {
            // Already queued (it's a round file); honor a rename request.
            if let Some(slot) = want.iter_mut().find(|(id, _)| id == &arg.id) {
                slot.1 = Some(n.clone());
            }
        }
    }

    // Fetch bytes for each wanted id and turn into StageItems.
    let mut items: Vec<StageItem> = Vec::new();
    for (id, name_override) in &want {
        let meta = session_atts.iter().find(|a| &a.id == id);
        let desired = match name_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(n) => sanitize_filename(n).ok_or_else(|| {
                ToolError::InvalidArgs(format!("attachment name `{n}` is not a valid filename"))
            })?,
            None => meta
                .map(|a| a.filename.clone())
                .unwrap_or_else(|| id.rsplit('/').next().unwrap_or("file").to_string()),
        };
        // `id` is `<turn>/<filename>`; fetch by those parts.
        let (turn, filename) = id
            .split_once('/')
            .ok_or_else(|| ToolError::Failed(format!("malformed attachment id `{id}`")))?;
        match chat_attachments::fetch(s3, turn, filename).await {
            Ok(f) => items.push(StageItem {
                name: desired,
                id: id.clone(),
                bytes: f.bytes,
            }),
            Err(e) => notes.push(format!("Could not fetch attachment `{id}`: {e}")),
        }
    }

    let (files, staged, mut asm_notes) = assemble_inputs(items, STAGE_TOTAL_MAX_BYTES);
    notes.append(&mut asm_notes);

    // Advertise session files that weren't staged this run, so the model
    // knows what else it can pull by id.
    let staged_ids: std::collections::HashSet<&str> =
        staged.iter().filter_map(|s| s["id"].as_str()).collect();
    let available: Vec<Value> = session_atts
        .iter()
        .filter(|a| !staged_ids.contains(a.id.as_str()))
        .map(|a| json!({"id": a.id, "filename": a.filename, "mime": a.mime, "size": a.size}))
        .collect();

    Ok(Staged {
        files,
        staged,
        available,
        notes,
    })
}

// ---------------------------------------------------------------------------
// Generic tool: run_in_sandbox

#[derive(Deserialize)]
struct RunArgs {
    language: Language,
    code: String,
    #[serde(default)]
    files: Vec<TextFile>,
    #[serde(default)]
    attachments: Vec<AttachmentArg>,
    #[serde(default)]
    network: bool,
}

/// Splice staging metadata into a sandbox result so the model knows
/// what files landed in `/work` and what else it can pull by id.
fn augment_with_staging(
    out: &mut Value,
    staged: Vec<Value>,
    available: Vec<Value>,
    notes: Vec<String>,
) {
    let Some(obj) = out.as_object_mut() else {
        return;
    };
    if !staged.is_empty() {
        obj.insert("staged_files".into(), json!(staged));
    }
    if !available.is_empty() {
        obj.insert("available_attachments".into(), json!(available));
    }
    if !notes.is_empty() {
        obj.insert("attachment_notes".into(), json!(notes));
    }
}

/// A small UTF-8 text input file the model wants in `/work` (e.g. a CSV
/// or a config). Binary inputs aren't expressible from a tool call; for
/// those the model fetches via other tools or generates them in-sandbox.
#[derive(Deserialize)]
struct TextFile {
    name: String,
    content: String,
}

impl TextFile {
    fn into_input(self) -> InputFile {
        InputFile {
            name: self.name,
            content_b64: b64::encode(self.content.as_bytes()),
        }
    }
}

pub struct RunInSandbox(pub Arc<SandboxClient>);

impl Tool for RunInSandbox {
    fn id(&self) -> &str {
        "run_in_sandbox"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Run Python or shell in a secure, isolated, single-use sandbox (a \
             throwaway VM) and get back stdout, stderr, and any files it \
             produced — like a capable system-engineer shell. Use it to \
             inspect/debug large or compressed log files, analyze data, work \
             with office documents, convert between file formats, run CLI \
             tools, and generate files. \
             Python libs: pandas, numpy, scipy, scikit-learn, statsmodels, \
             sympy, polars, pyarrow, duckdb, matplotlib/seaborn, \
             openpyxl/xlsxwriter/xlrd, python-docx, python-pptx, odfpy, \
             typ2pptx (Typst→editable .pptx: real text/shapes/gradients — \
             compile the .typ with its fonts on TYPST_FONT_PATHS, run \
             `typ2pptx in.typ --root <dir> --detect-paragraphs -o out.pptx`; \
             if the deck's font comes out as Consolas, set the run typeface to \
             the real font name in ppt/slides/*.xml), \
             pypdf/pdfplumber/pymupdf, reportlab, img2pdf, pillow, opencv, \
             pytesseract (OCR), sqlalchemy/psycopg/pymysql, scapy, lxml, \
             beautifulsoup4, requests. \
             CLI tools: ripgrep (rg), jq, yq, jc, awk/sed, duckdb + sqlite3 \
             (SQL over CSV/JSON/Parquet/large logs), ffmpeg, imagemagick, vips, \
             tesseract (OCR), tshark/tcpdump (read .pcap), graphviz (dot), \
             LibreOffice (`soffice --headless` for office↔pdf), pandoc, typst, \
             ghostscript/qpdf, poppler-utils (pdftotext/pdftoppm), \
             gzip/zstd/xz/bzip2/7z, git, curl/wget, dig/rsync, file/xxd, lnav, \
             and a C toolchain (gcc/make). Headless chromium is available too. \
             Each call starts clean — nothing persists between calls, so do \
             all the work for one job (combine/compare several files, \
             multi-step pipelines) in a SINGLE call rather than spreading it \
             across calls. Files a user uploaded this turn are ALREADY waiting \
             in the working directory under their original names — just open \
             them. To also work on a file from earlier in the conversation, \
             pass its id (from an `[attached … id=\"<turn>/<file>\"]` stub) in \
             `attachments`; it gets fetched into the working directory too. \
             The result's `staged_files` lists what's in the directory and \
             `available_attachments` lists other files you can pull in by id. \
             Network \
             is OFF unless you set `network: true` (and the operator enabled \
             egress); without it pip install and web access fail. Write files \
             to the current working directory to return them to the user. \
             When stdout/stderr is large you get a small preview plus a \
             `full_output_ref` — call read_sandbox_output with that ref to \
             grep/page the rest instead of pulling it all into context. Best \
             practice: filter/aggregate in-sandbox (grep, awk, duckdb, \
             head/tail) and print a concise summary rather than dumping raw \
             data.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["language", "code"],
                "properties": {
                    "language": {
                        "type": "string", "enum": ["python", "bash"],
                        "description": "Interpreter for `code`."
                    },
                    "code": {
                        "type": "string",
                        "description": "The program to run. Write output files to the \
                                        current working directory to return them."
                    },
                    "files": {
                        "type": "array",
                        "description": "Optional UTF-8 text files to place in the working \
                                        directory before running.",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["name", "content"],
                            "properties": {
                                "name": {"type": "string"},
                                "content": {"type": "string"}
                            }
                        }
                    },
                    "attachments": {
                        "type": "array",
                        "description": "Optional chat attachments to fetch into the working \
                                        directory (binary-safe — use this for uploaded \
                                        .pptx/.xlsx/.pdf/images/zip you want to process). \
                                        The current turn's uploads are staged automatically; \
                                        list ids here only to pull in files from EARLIER in \
                                        the conversation (see `available_attachments` in a \
                                        prior result).",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["id"],
                            "properties": {
                                "id": {"type": "string", "description": "Attachment id \
                                       `<turn>/<file>` from an attachment stub."},
                                "name": {"type": "string", "description": "Optional filename \
                                         to give the file in the working directory."}
                            }
                        }
                    },
                    "network": {
                        "type": "boolean",
                        "description": "Request network egress for this run (pip / web). \
                                        Default false; only honored if the operator \
                                        configured an egress allowlist."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: RunArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{language, code, files?, attachments?, network?}}: {e}"
                ))
            })?;
            if args.code.trim().is_empty() {
                return Err(ToolError::InvalidArgs("code must be non-empty".into()));
            }
            // Stage the round's uploads + any named attachments first, then
            // append the model's inline text files (so an explicit text file
            // wins over a same-named staged file).
            let Staged {
                files: staged_files,
                staged,
                available,
                notes,
            } = stage_attachments(&ctx, &args.attachments).await?;
            let mut files = staged_files;
            files.extend(args.files.into_iter().map(TextFile::into_input));
            let req = RunRequest {
                language: args.language,
                code: args.code,
                files,
                timeout_secs: None,
                network: args.network,
            };
            let mut out = self.0.execute(&ctx, req).await?;
            augment_with_staging(&mut out, staged, available, notes);
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------------------
// Wrapper: generate_document (markdown -> pdf/docx/pptx via pandoc)

#[derive(Deserialize)]
struct DocArgs {
    markdown: String,
    format: DocFormat,
    #[serde(default)]
    filename: Option<String>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum DocFormat {
    Pdf,
    Docx,
    Pptx,
}

impl DocFormat {
    fn ext(self) -> &'static str {
        match self {
            DocFormat::Pdf => "pdf",
            DocFormat::Docx => "docx",
            DocFormat::Pptx => "pptx",
        }
    }
}

pub struct GenerateDocument(pub Arc<SandboxClient>);

impl Tool for GenerateDocument {
    fn id(&self) -> &str {
        "generate_document"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Turn Markdown into a finished PDF, Word (.docx), or PowerPoint \
             (.pptx) document and return it to the user. Write normal Markdown; \
             for slides, separate them with `---`. This is the easy path for \
             document generation — no code required. For anything Markdown can't \
             express, use `run_in_sandbox` directly.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["markdown", "format"],
                "properties": {
                    "markdown": {"type": "string", "description": "Document content as Markdown."},
                    "format": {
                        "type": "string", "enum": ["pdf", "docx", "pptx"],
                        "description": "Output format."
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
            let args: DocArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{markdown, format, filename?}}: {e}"))
            })?;
            if args.markdown.trim().is_empty() {
                return Err(ToolError::InvalidArgs("markdown must be non-empty".into()));
            }
            let ext = args.format.ext();
            let stem = filename_stem(args.filename.as_deref(), "document");
            let out = format!("{stem}.{ext}");
            // The markdown rides in as an input file (never interpolated into
            // the command), so its content can't break out into the shell.
            let pdf_engine = if matches!(args.format, DocFormat::Pdf) {
                " --pdf-engine=weasyprint"
            } else {
                ""
            };
            let code = format!("set -e\npandoc input.md -o {out:?}{pdf_engine}\n");
            let req = RunRequest {
                language: Language::Bash,
                code,
                files: vec![InputFile {
                    name: "input.md".into(),
                    content_b64: b64::encode(args.markdown.as_bytes()),
                }],
                timeout_secs: None,
                network: false,
            };
            self.0.execute(&ctx, req).await
        })
    }
}

// ---------------------------------------------------------------------------
// Wrapper: export_document — render a canvas document to a downloadable file

#[derive(Deserialize)]
struct ExportArgs {
    document_id: String,
    format: DocFormat,
    #[serde(default)]
    filename: Option<String>,
}

/// Bridge the document canvas to the sandbox's pandoc path: take a
/// document the model built with `create_document`/`edit_document` and
/// render its current content to a downloadable PDF/DOCX/PPTX. Reuses the
/// exact `generate_document` recipe — the only difference is the Markdown
/// comes from the `documents` store rather than the tool args.
pub struct ExportDocument(pub Arc<SandboxClient>);

impl Tool for ExportDocument {
    fn id(&self) -> &str {
        "export_document"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Export a document from the canvas (one you created with \
             `create_document`) to a finished PDF, Word (.docx), or PowerPoint \
             (.pptx) file and attach it for the user to download. Give the \
             `document_id` and a `format`. Markdown and text documents only. \
             For one-off Markdown you haven't put in the canvas, use \
             `generate_document` instead.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["document_id", "format"],
                "properties": {
                    "document_id": {"type": "string", "description": "The id from `create_document`."},
                    "format": {
                        "type": "string", "enum": ["pdf", "docx", "pptx"],
                        "description": "Output format."
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
            use crate::server::db::documents::{self, DocumentFormat};
            let session_id = ctx.session_id.as_deref().ok_or_else(|| {
                ToolError::Failed("export_document is only available inside a chat session".into())
            })?;
            let args: ExportArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{document_id, format, filename?}}: {e}"))
            })?;
            let (doc, ver) = documents::get_version(&ctx.db, session_id, &args.document_id, None)
                .await
                .map_err(|e| ToolError::Failed(format!("reading document: {e}")))?
                .ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "no document `{}` in this conversation",
                        args.document_id
                    ))
                })?;
            // Pandoc reads the content as Markdown; only prose formats export
            // sensibly. Structured/HTML docs would come out garbled.
            if !matches!(doc.format, DocumentFormat::Markdown | DocumentFormat::Text) {
                return Err(ToolError::InvalidArgs(format!(
                    "only markdown or text documents can be exported; `{}` is {}",
                    args.document_id,
                    doc.format.as_str()
                )));
            }
            let ext = args.format.ext();
            let stem = filename_stem(args.filename.as_deref(), "document");
            let out = format!("{stem}.{ext}");
            let pdf_engine = if matches!(args.format, DocFormat::Pdf) {
                " --pdf-engine=weasyprint"
            } else {
                ""
            };
            let code = format!("set -e\npandoc input.md -o {out:?}{pdf_engine}\n");
            let req = RunRequest {
                language: Language::Bash,
                code,
                files: vec![InputFile {
                    name: "input.md".into(),
                    content_b64: b64::encode(ver.content.as_bytes()),
                }],
                timeout_secs: None,
                network: false,
            };
            self.0.execute(&ctx, req).await
        })
    }
}

// ---------------------------------------------------------------------------
// Wrapper: capture_webpage (headless chromium)

#[derive(Deserialize)]
struct CaptureArgs {
    url: String,
    #[serde(default = "default_capture_output")]
    output: CaptureOutput,
    #[serde(default)]
    filename: Option<String>,
}

fn default_capture_output() -> CaptureOutput {
    CaptureOutput::Png
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum CaptureOutput {
    Png,
    Pdf,
    Text,
}

impl CaptureOutput {
    fn ext(self) -> &'static str {
        match self {
            CaptureOutput::Png => "png",
            CaptureOutput::Pdf => "pdf",
            CaptureOutput::Text => "txt",
        }
    }
    fn mode(self) -> &'static str {
        match self {
            CaptureOutput::Png => "png",
            CaptureOutput::Pdf => "pdf",
            CaptureOutput::Text => "text",
        }
    }
}

pub struct CaptureWebpage(pub Arc<SandboxClient>);

impl Tool for CaptureWebpage {
    fn id(&self) -> &str {
        "capture_webpage"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Load a web page in a headless browser and capture it as a \
             full-page PNG screenshot, a PDF, or extracted text. Requires the \
             operator to have enabled sandbox network egress. Returns the \
             capture as a file.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["url"],
                "properties": {
                    "url": {"type": "string", "description": "The http(s) URL to load."},
                    "output": {
                        "type": "string", "enum": ["png", "pdf", "text"],
                        "description": "What to capture. Default png."
                    },
                    "filename": {"type": "string", "description": "Optional output filename."}
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: CaptureArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{url, output?, filename?}}: {e}"))
            })?;
            let url = args.url.trim();
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(ToolError::InvalidArgs("url must be http(s)".into()));
            }
            let ext = args.output.ext();
            let stem = filename_stem(args.filename.as_deref(), "capture");
            let out = format!("{stem}.{ext}");
            // The URL rides in as a file (read at runtime) so it's never
            // interpolated into the script; only the validated mode/filename
            // are templated.
            let code = format!(
                "import pathlib\n\
                 from playwright.sync_api import sync_playwright\n\
                 url = pathlib.Path('url.txt').read_text().strip()\n\
                 out = {out:?}\n\
                 with sync_playwright() as p:\n\
                 \x20   b = p.chromium.launch(args=['--no-sandbox'])\n\
                 \x20   pg = b.new_page()\n\
                 \x20   pg.goto(url, wait_until='networkidle', timeout=30000)\n\
                 \x20   mode = {mode:?}\n\
                 \x20   if mode == 'png':\n\
                 \x20       pg.screenshot(path=out, full_page=True)\n\
                 \x20   elif mode == 'pdf':\n\
                 \x20       pg.pdf(path=out)\n\
                 \x20   else:\n\
                 \x20       pathlib.Path(out).write_text(pg.inner_text('body'))\n\
                 \x20   b.close()\n",
                out = out,
                mode = args.output.mode(),
            );
            let req = RunRequest {
                language: Language::Python,
                code,
                files: vec![InputFile {
                    name: "url.txt".into(),
                    content_b64: b64::encode(url.as_bytes()),
                }],
                timeout_secs: None,
                network: true,
            };
            self.0.execute(&ctx, req).await
        })
    }
}

// ---------------------------------------------------------------------------
// Document presets over uploaded files: convert_document + edit_presentation.
//
// Both resolve a single uploaded attachment (the round's file by default,
// or one named by id), fetch it server-side, stage it into `/work`, and
// run a fixed recipe in the sandbox — the file-in / file-out cousins of
// `generate_document`. The generic escape hatch is `run_in_sandbox`.

/// A safe `/work` filename stem from an attachment's name: keep only
/// `[A-Za-z0-9_-]`, drop the extension, fall back to `document`. Used so
/// staged input + produced output names can be interpolated into the
/// recipe without any shell-meta risk.
fn safe_stem(filename: &str) -> String {
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(filename);
    let s: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim_matches('_').to_string();
    if s.is_empty() {
        "document".to_string()
    } else {
        s
    }
}

/// Lowercase alphanumeric extension of `filename`, if it has a clean one.
fn safe_ext(filename: &str) -> Option<String> {
    filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Does this attachment look like a PowerPoint deck?
fn is_pptx(a: &AttachmentRef) -> bool {
    a.filename.to_ascii_lowercase().ends_with(".pptx")
        || a.mime.contains("presentation")
        || a.mime.contains("powerpoint")
}

/// Resolve exactly one uploaded attachment for a preset, then fetch its
/// bytes. Picks the round's file when `explicit_id` is `None` (erroring
/// clearly on none / several so the model chooses), validates a
/// model-named id against the session, and enforces `want` (the file
/// kind the tool handles). Chat-path only — needs a session + storage.
async fn resolve_one_attachment(
    ctx: &ToolContext,
    explicit_id: Option<&str>,
    kind: &str,
    want: impl Fn(&AttachmentRef) -> bool,
) -> Result<(AttachmentRef, Vec<u8>), ToolError> {
    let (Some(session_id), Some(s3)) = (ctx.session_id.as_deref(), ctx.s3.as_ref()) else {
        return Err(ToolError::Failed(
            "this tool works on uploaded chat files and needs the chat path with attachment \
             storage configured ([chat.s3]); it isn't available here."
                .into(),
        ));
    };
    let (session_atts, round) =
        chat_attachments::session_and_round_attachments(&ctx.db, session_id)
            .await
            .map_err(|e| ToolError::Failed(format!("listing session attachments: {e}")))?;

    let chosen: AttachmentRef = match explicit_id {
        Some(id) => {
            let a = session_atts
                .iter()
                .find(|a| a.id == id)
                .ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "attachment id `{id}` is not in this conversation"
                    ))
                })?
                .clone();
            if !want(&a) {
                return Err(ToolError::InvalidArgs(format!(
                    "attachment `{}` is not a {kind}",
                    a.filename
                )));
            }
            a
        }
        None => {
            let mut candidates: Vec<AttachmentRef> = round.into_iter().filter(&want).collect();
            match candidates.len() {
                1 => candidates.pop().unwrap(),
                0 => {
                    let avail: Vec<&str> = session_atts
                        .iter()
                        .filter(|a| want(a))
                        .map(|a| a.id.as_str())
                        .collect();
                    let hint = if avail.is_empty() {
                        String::new()
                    } else {
                        format!(" Earlier {kind}s you can pass as attachment_id: {avail:?}.")
                    };
                    return Err(ToolError::InvalidArgs(format!(
                        "no {kind} was uploaded in this message; upload one or pass \
                         attachment_id.{hint}"
                    )));
                }
                _ => {
                    let ids: Vec<&str> = candidates.iter().map(|a| a.id.as_str()).collect();
                    return Err(ToolError::InvalidArgs(format!(
                        "several {kind}s were uploaded this message — pass attachment_id to \
                         choose one of: {ids:?}"
                    )));
                }
            }
        }
    };

    let (turn, filename) = chosen
        .id
        .split_once('/')
        .ok_or_else(|| ToolError::Failed(format!("malformed attachment id `{}`", chosen.id)))?;
    let fetched = chat_attachments::fetch(s3, turn, filename)
        .await
        .map_err(|e| ToolError::Failed(format!("fetching `{}`: {e}", chosen.id)))?;
    Ok((chosen, fetched.bytes))
}

// ---------------------------------------------------------------------------
// convert_document — uploaded office/pdf file -> pdf/docx/txt/html/images

#[derive(Deserialize)]
struct ConvertArgs {
    #[serde(default)]
    attachment_id: Option<String>,
    target: ConvertTarget,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum ConvertTarget {
    Pdf,
    Docx,
    Txt,
    Html,
    /// One PNG per page/slide (via pdf, rendered at 150 dpi).
    Images,
}

impl ConvertTarget {
    fn as_str(self) -> &'static str {
        match self {
            ConvertTarget::Pdf => "pdf",
            ConvertTarget::Docx => "docx",
            ConvertTarget::Txt => "txt",
            ConvertTarget::Html => "html",
            ConvertTarget::Images => "images",
        }
    }
}

pub struct ConvertDocument(pub Arc<SandboxClient>);

impl ConvertDocument {
    /// The sandbox recipe. `stem`/`ext` are pre-sanitized (safe charset),
    /// so interpolating them carries no shell-meta risk.
    fn script(target: ConvertTarget, stem: &str, ext: &str) -> String {
        let infile = format!("{stem}.{ext}");
        match target {
            ConvertTarget::Images => format!(
                "set -e\n\
                 soffice --headless --convert-to pdf --outdir . {infile}\n\
                 pdftoppm -png -r 150 {stem}.pdf {stem}-slide\n\
                 rm -f {stem}.pdf\n"
            ),
            other => {
                let t = other.as_str();
                format!("set -e\nsoffice --headless --convert-to {t} --outdir . {infile}\n")
            }
        }
    }
}

impl Tool for ConvertDocument {
    fn id(&self) -> &str {
        "convert_document"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Convert a file the user uploaded (PowerPoint, Word, Excel, \
             ODF, PDF, …) to another format and return the result. Targets: \
             `pdf`, `docx`, `txt`, `html`, or `images` (one PNG per \
             page/slide). By default it converts the file uploaded in the \
             current message; pass `attachment_id` (from an attachment stub) \
             to convert a file from earlier in the conversation. Conversion \
             runs through LibreOffice. For edits or anything custom, use \
             `edit_presentation` or `run_in_sandbox`.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["target"],
                "properties": {
                    "attachment_id": {
                        "type": "string",
                        "description": "Optional `<turn>/<file>` id of the file to convert. \
                                        Defaults to the file uploaded in the current message."
                    },
                    "target": {
                        "type": "string",
                        "enum": ["pdf", "docx", "txt", "html", "images"],
                        "description": "Output format. `images` returns one PNG per page/slide."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: ConvertArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{attachment_id?, target}}: {e}"))
            })?;
            let (att, bytes) =
                resolve_one_attachment(&ctx, args.attachment_id.as_deref(), "file", |_| true)
                    .await?;
            let stem = safe_stem(&att.filename);
            let ext = safe_ext(&att.filename).ok_or_else(|| {
                ToolError::InvalidArgs(format!(
                    "`{}` has no file extension, so its type can't be determined",
                    att.filename
                ))
            })?;
            let infile = format!("{stem}.{ext}");
            let code = Self::script(args.target, &stem, &ext);
            let req = RunRequest {
                language: Language::Bash,
                code,
                files: vec![InputFile {
                    name: infile,
                    content_b64: b64::encode(&bytes),
                }],
                timeout_secs: None,
                network: false,
            };
            let mut out = self.0.execute(&ctx, req).await?;
            if let Some(obj) = out.as_object_mut() {
                obj.insert(
                    "converted".into(),
                    json!({"id": att.id, "target": args.target.as_str()}),
                );
            }
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------------------
// edit_presentation — run python-pptx against an uploaded .pptx

#[derive(Deserialize)]
struct EditPptxArgs {
    #[serde(default)]
    attachment_id: Option<String>,
    code: String,
}

pub struct EditPresentation(pub Arc<SandboxClient>);

impl Tool for EditPresentation {
    fn id(&self) -> &str {
        "edit_presentation"
    }

    fn max_duration(&self) -> Option<std::time::Duration> {
        Some(self.0.loop_timeout())
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Modify a PowerPoint (.pptx) the user uploaded, using python-pptx. \
             Your `code` runs in the sandbox with the deck already saved as \
             `input.pptx` in the working directory; load it, make your \
             changes, and save the result as `output.pptx` — it's returned to \
             the user. By default it edits the .pptx uploaded in the current \
             message; pass `attachment_id` to edit one from earlier in the \
             conversation. Example: `from pptx import Presentation; p = \
             Presentation('input.pptx'); p.slides[0].shapes.title.text = 'Hi'; \
             p.save('output.pptx')`. For other file types or free-form work, \
             use `run_in_sandbox`.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["code"],
                "properties": {
                    "attachment_id": {
                        "type": "string",
                        "description": "Optional `<turn>/<file>` id of the .pptx to edit. \
                                        Defaults to the deck uploaded in the current message."
                    },
                    "code": {
                        "type": "string",
                        "description": "Python (python-pptx) that reads `input.pptx` and writes \
                                        `output.pptx` in the working directory."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: EditPptxArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{attachment_id?, code}}: {e}"))
            })?;
            if args.code.trim().is_empty() {
                return Err(ToolError::InvalidArgs("code must be non-empty".into()));
            }
            let (att, bytes) = resolve_one_attachment(
                &ctx,
                args.attachment_id.as_deref(),
                "PowerPoint (.pptx) file",
                is_pptx,
            )
            .await?;
            // The deck rides in as a fixed-name binary input; the model's
            // code (which references `input.pptx`) is the program.
            let req = RunRequest {
                language: Language::Python,
                code: args.code,
                files: vec![InputFile {
                    name: "input.pptx".into(),
                    content_b64: b64::encode(&bytes),
                }],
                timeout_secs: None,
                network: false,
            };
            let mut out = self.0.execute(&ctx, req).await?;
            if let Some(obj) = out.as_object_mut() {
                obj.insert("edited".into(), json!({"id": att.id}));
            }
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------------------
// Retrieval tool: read_sandbox_output — grep/head/tail/range over a stored
// large output (the searchable-object / pointers-as-context pattern).

const READ_DEFAULT_LIMIT: usize = 200;
const READ_MAX_LIMIT: usize = 2000;
const READ_DEFAULT_MAX_BYTES: usize = 16 * 1024;
const READ_HARD_MAX_BYTES: usize = 64 * 1024;

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ReadAction {
    Grep,
    Head,
    Tail,
    Range,
}

fn default_read_action() -> ReadAction {
    ReadAction::Head
}

#[derive(Deserialize)]
struct ReadArgs {
    /// `full_output_ref` from a run_in_sandbox result, e.g. "<turn>/stdout.txt".
    id: String,
    #[serde(default = "default_read_action")]
    action: ReadAction,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

/// Reads slices of a stored sandbox output on demand, so the model can drill
/// into a large stdout/stderr (or any produced text file) without inlining
/// the whole thing. Chat-path only — it resolves the `full_output_ref` against
/// the current turn's attachments; API callers fetch `full_output_url` instead.
pub struct ReadSandboxOutput;

impl Tool for ReadSandboxOutput {
    fn id(&self) -> &str {
        "read_sandbox_output"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Read part of a large output a previous run_in_sandbox produced \
             (the value of its `full_output_ref`). Use this to drill into big \
             logs/results without pulling the whole thing into context: grep \
             for matching lines, or page through with head/tail/range.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": {"type": "string", "description": "The full_output_ref from a run_in_sandbox result."},
                    "action": {"type": "string", "enum": ["grep", "head", "tail", "range"],
                               "description": "grep matching lines (needs `query`), or head/tail/range. Default head."},
                    "query": {"type": "string", "description": "Regex for action=grep."},
                    "start_line": {"type": "integer", "description": "1-based first line for action=range."},
                    "end_line": {"type": "integer", "description": "1-based last line for action=range."},
                    "limit": {"type": "integer", "description": "Max lines to return (default 200)."},
                    "max_bytes": {"type": "integer", "description": "Max bytes to return (default 16384)."}
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: ReadArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{id, action?, …}}: {e}")))?;
            let s3 = ctx.s3.as_ref().ok_or_else(|| {
                ToolError::Failed("attachment storage not configured ([chat.s3])".into())
            })?;
            // `id` is "<turn>/<filename>". Restrict to the CURRENT turn so the
            // model can only read outputs it just produced — never another
            // conversation's attachments.
            let (turn, filename) = args
                .id
                .split_once('/')
                .ok_or_else(|| ToolError::InvalidArgs("id must be \"<turn>/<filename>\"".into()))?;
            match ctx.assistant_turn_id.as_deref() {
                Some(cur) if cur == turn => {}
                Some(_) | None => {
                    return Err(ToolError::Failed(
                        "read_sandbox_output can only read outputs from the current chat turn"
                            .into(),
                    ));
                }
            }
            let fetched = chat_attachments::fetch(s3, turn, filename)
                .await
                .map_err(|e| ToolError::Failed(format!("fetch output: {e}")))?;
            let text = String::from_utf8_lossy(&fetched.bytes);
            let limit = args
                .limit
                .unwrap_or(READ_DEFAULT_LIMIT)
                .clamp(1, READ_MAX_LIMIT);
            let max_bytes = args
                .max_bytes
                .unwrap_or(READ_DEFAULT_MAX_BYTES)
                .clamp(256, READ_HARD_MAX_BYTES);
            slice_text(
                &text,
                args.action,
                args.query.as_deref(),
                args.start_line,
                args.end_line,
                limit,
                max_bytes,
            )
        })
    }
}

/// Pure slicing over a stored text output. Returns a model-facing JSON object;
/// kept free of I/O so it's unit-testable.
fn slice_text(
    text: &str,
    action: ReadAction,
    query: Option<&str>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    limit: usize,
    max_bytes: usize,
) -> Result<Value, ToolError> {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let numbered = |i: usize| (i + 1, lines[i]);

    // `window_total` = how many lines the action covers in the whole file
    // (before the `limit`), so `more_available` can tell the model there's
    // more to page through even when `selected` was capped.
    let (selected, window_total, matched_total): (Vec<(usize, &str)>, usize, Option<usize>) =
        match action {
            ReadAction::Head => ((0..total).take(limit).map(numbered).collect(), total, None),
            ReadAction::Tail => {
                let from = total.saturating_sub(limit);
                ((from..total).map(numbered).collect(), total, None)
            }
            ReadAction::Range => {
                let s = start_line.unwrap_or(1).max(1);
                let e = end_line.unwrap_or(s + limit - 1).max(s);
                let lo = s.min(total + 1);
                let hi = e.min(total);
                let window = (hi + 1).saturating_sub(lo); // count of lines in [s,e]
                let sel: Vec<(usize, &str)> = (lo..=hi)
                    .take(limit)
                    .map(|ln| (ln, lines[ln - 1]))
                    .collect();
                (sel, window, None)
            }
            ReadAction::Grep => {
                let q =
                    query.ok_or_else(|| ToolError::InvalidArgs("grep requires `query`".into()))?;
                let re = regex::Regex::new(q)
                    .map_err(|e| ToolError::InvalidArgs(format!("invalid regex: {e}")))?;
                let all: Vec<(usize, &str)> = (0..total)
                    .map(numbered)
                    .filter(|(_, l)| re.is_match(l))
                    .collect();
                let matched = all.len();
                (
                    all.into_iter().take(limit).collect(),
                    matched,
                    Some(matched),
                )
            }
        };

    let mut content = String::new();
    let mut returned = 0usize;
    let mut byte_capped = false;
    for (ln, l) in &selected {
        let piece = format!("{ln}: {l}\n");
        if !content.is_empty() && content.len() + piece.len() > max_bytes {
            byte_capped = true;
            break;
        }
        content.push_str(&piece);
        returned += 1;
    }
    let more = byte_capped || returned < window_total;
    Ok(json!({
        "total_lines": total,
        "returned_lines": returned,
        "matched_lines": matched_total,
        "more_available": more,
        "content": content,
    }))
}

// ---------------------------------------------------------------------------
// Minimal base64 (standard, padded). Encode for input files, decode for
// artifacts. Self-contained to keep the tool off a base64 dependency,
// matching the codecs in `chat_attachments` / `upload_attachment`.

pub(crate) mod b64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = chunk.get(1).copied().unwrap_or(0);
            let b2 = chunk.get(2).copied().unwrap_or(0);
            out.push(ALPHABET[(b0 >> 2) as usize] as char);
            out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
            out.push(if chunk.len() >= 2 {
                ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() >= 3 {
                ALPHABET[(b2 & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    pub fn decode(s: &str) -> Option<Vec<u8>> {
        fn val(c: u8) -> Option<u8> {
            Some(match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return None,
            })
        }
        let mut quad = [0u8; 4];
        let mut qn = 0usize;
        let mut pads = 0usize;
        let mut out = Vec::with_capacity(s.len() / 4 * 3);
        for &c in s.as_bytes() {
            if c.is_ascii_whitespace() {
                continue;
            }
            if c == b'=' {
                quad[qn] = 0;
                pads += 1;
            } else {
                if pads > 0 {
                    return None;
                }
                quad[qn] = val(c)?;
            }
            qn += 1;
            if qn == 4 {
                out.push((quad[0] << 2) | (quad[1] >> 4));
                if pads < 2 {
                    out.push((quad[1] << 4) | (quad[2] >> 2));
                }
                if pads < 1 {
                    out.push((quad[2] << 6) | quad[3]);
                }
                qn = 0;
                if pads > 0 {
                    break;
                }
            }
        }
        if qn != 0 { None } else { Some(out) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn ctx() -> ToolContext {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        ToolContext {
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
        }
    }

    fn client(runner_url: String) -> Arc<SandboxClient> {
        SandboxClient::new(
            Arc::new(SandboxConfig {
                enabled: true,
                runner_url,
                timeout_secs: 5,
                max_artifact_bytes: 1024,
            }),
            "https://gw.example".into(),
        )
    }

    /// An S3 config whose credential env vars are unset, so `open_bucket`
    /// fails fast with `MissingCredential` before any network — lets us
    /// drive the staging orchestrator deterministically (a fetch attempt
    /// turns into a clean "could not fetch" note) without a live bucket.
    fn dead_s3() -> std::sync::Arc<crate::server::config::S3Config> {
        std::sync::Arc::new(crate::server::config::S3Config {
            endpoint: "http://127.0.0.1:1".into(),
            region: "us-east-1".into(),
            bucket: "b".into(),
            access_key_env: "SANDBOX_STAGE_TEST_UNSET".into(),
            secret_key_env: "SANDBOX_STAGE_TEST_UNSET".into(),
            key_prefix: "chat-attachments".into(),
        })
    }

    async fn seed_session_with_upload(pool: &db::Pool, turn_id: &str, marker: &str) {
        for q in [
            "INSERT INTO users (id, email, created_at, updated_at) VALUES \
             ('u', 'u@e', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z') ON CONFLICT(id) DO NOTHING",
            "INSERT INTO chat_sessions (id, user_id, created_at, updated_at) VALUES \
             ('s1', 'u', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z') ON CONFLICT(id) DO NOTHING",
        ] {
            sqlx::query(q).execute(pool).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO chat_turns (id, session_id, seq, role, user_content, status, created_at) \
             VALUES (?, 's1', 0, 'user', ?, 'completed', '2026-01-01T00:00:00Z')",
        )
        .bind(turn_id)
        .bind(marker)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stage_attachments_noop_without_session() {
        // Proxy/`/v1` path: no session → nothing staged, even if ids given.
        let c = ctx().await; // session_id None, s3 None
        let s = stage_attachments(&c, &[]).await.unwrap();
        assert!(s.files.is_empty() && s.staged.is_empty() && s.notes.is_empty());
        // Explicit ids on a session-less path get a note, not a hard error.
        let s = stage_attachments(
            &c,
            &[AttachmentArg {
                id: "t/x.pptx".into(),
                name: None,
            }],
        )
        .await
        .unwrap();
        assert!(s.files.is_empty());
        assert_eq!(s.notes.len(), 1);
        assert!(s.notes[0].contains("can't be staged"), "{:?}", s.notes);
    }

    #[tokio::test]
    async fn stage_attachments_auto_stages_round_and_reports_fetch_failure() {
        let mut c = ctx().await;
        let marker = chat_attachments::marker_line(
            "t-u1",
            &chat_attachments::UploadOutcome {
                filename: "deck.pptx".into(),
                mime: "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                    .into(),
                bytes: 10,
            },
        );
        seed_session_with_upload(&c.db, "t-u1", &marker).await;
        c.session_id = Some("s1".into());
        c.s3 = Some(dead_s3());

        // No explicit attachments: the round's upload must still be picked
        // up automatically. The fetch fails (no creds) → a clean note, and
        // the file stays in `available_attachments` since it wasn't staged.
        let s = stage_attachments(&c, &[]).await.unwrap();
        assert!(s.files.is_empty(), "fetch failed, so nothing staged");
        assert!(
            s.notes
                .iter()
                .any(|n| n.contains("deck.pptx") && n.contains("Could not fetch")),
            "round upload should be discovered and a fetch attempted: {:?}",
            s.notes
        );
        assert!(
            s.available.iter().any(|a| a["id"] == "t-u1/deck.pptx"),
            "unstaged session file should be advertised: {:?}",
            s.available
        );
    }

    #[test]
    fn safe_stem_and_ext_sanitize() {
        assert_eq!(safe_stem("My Deck (final).pptx"), "My_Deck__final");
        assert_eq!(safe_stem("..weird.."), "weird");
        assert_eq!(safe_stem(""), "document");
        assert_eq!(safe_ext("a.PPTX").as_deref(), Some("pptx"));
        assert_eq!(safe_ext("noext"), None);
        assert_eq!(safe_ext("bad.ex t"), None);
    }

    #[test]
    fn is_pptx_matches_name_or_mime() {
        let r = |f: &str, m: &str| AttachmentRef {
            id: format!("t/{f}"),
            turn_id: "t".into(),
            filename: f.into(),
            mime: m.into(),
            size: 1,
        };
        assert!(is_pptx(&r("a.pptx", "application/octet-stream")));
        assert!(is_pptx(&r("a.bin", "application/vnd.ms-powerpoint")));
        assert!(!is_pptx(&r("a.csv", "text/csv")));
    }

    #[test]
    fn convert_script_uses_safe_names_and_renders_images() {
        let pdf = ConvertDocument::script(ConvertTarget::Pdf, "deck", "pptx");
        assert!(pdf.contains("--convert-to pdf"));
        assert!(pdf.contains("deck.pptx"));
        let imgs = ConvertDocument::script(ConvertTarget::Images, "deck", "pptx");
        assert!(imgs.contains("--convert-to pdf"), "images go via pdf");
        assert!(imgs.contains("pdftoppm -png"));
        assert!(imgs.contains("deck-slide"));
    }

    #[tokio::test]
    async fn presets_need_chat_path() {
        // No session → both presets fail with a clear message, never panic.
        let c = ctx().await;
        let err = resolve_one_attachment(&c, None, "file", |_| true)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Failed(ref m) if m.contains("chat path")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn resolve_one_attachment_default_resolution_cases() {
        // Two pptx uploaded this round → edit_presentation can't guess.
        let mut c = ctx().await;
        let m1 = chat_attachments::marker_line(
            "t-u1",
            &chat_attachments::UploadOutcome {
                filename: "a.pptx".into(),
                mime: "application/vnd.ms-powerpoint".into(),
                bytes: 1,
            },
        );
        let m2 = chat_attachments::marker_line(
            "t-u1",
            &chat_attachments::UploadOutcome {
                filename: "b.pptx".into(),
                mime: "application/vnd.ms-powerpoint".into(),
                bytes: 1,
            },
        );
        seed_session_with_upload(&c.db, "t-u1", &format!("{m1}\n{m2}")).await;
        c.session_id = Some("s1".into());
        c.s3 = Some(dead_s3());

        let err = resolve_one_attachment(&c, None, "PowerPoint (.pptx) file", is_pptx)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(ref m) if m.contains("several") && m.contains("a.pptx")),
            "{err:?}"
        );

        // An explicit id outside the session is rejected.
        let err =
            resolve_one_attachment(&c, Some("t-x/c.pptx"), "PowerPoint (.pptx) file", is_pptx)
                .await
                .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(ref m) if m.contains("not in this conversation")),
            "{err:?}"
        );

        // An explicit id that exists but is the wrong kind is rejected.
        let csv = chat_attachments::marker_line(
            "t-u2",
            &chat_attachments::UploadOutcome {
                filename: "data.csv".into(),
                mime: "text/csv".into(),
                bytes: 1,
            },
        );
        sqlx::query(
            "INSERT INTO chat_turns (id, session_id, seq, role, user_content, status, created_at) \
             VALUES ('t-u2', 's1', 2, 'user', ?, 'completed', '2026-01-01T00:00:00Z')",
        )
        .bind(&csv)
        .execute(&c.db)
        .await
        .unwrap();
        let err = resolve_one_attachment(
            &c,
            Some("t-u2/data.csv"),
            "PowerPoint (.pptx) file",
            is_pptx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(ref m) if m.contains("not a PowerPoint")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn resolve_one_attachment_none_uploaded_errors_with_hint() {
        // Latest user turn carried only a non-pptx → edit can't default,
        // and there's no earlier pptx to hint at.
        let mut c = ctx().await;
        let csv = chat_attachments::marker_line(
            "t-u1",
            &chat_attachments::UploadOutcome {
                filename: "data.csv".into(),
                mime: "text/csv".into(),
                bytes: 1,
            },
        );
        seed_session_with_upload(&c.db, "t-u1", &csv).await;
        c.session_id = Some("s1".into());
        c.s3 = Some(dead_s3());
        let err = resolve_one_attachment(&c, None, "PowerPoint (.pptx) file", is_pptx)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(ref m) if m.contains("no PowerPoint")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn stage_attachments_ignores_ids_outside_session() {
        let mut c = ctx().await;
        seed_session_with_upload(&c.db, "t-u1", "no files here").await;
        c.session_id = Some("s1".into());
        c.s3 = Some(dead_s3());
        let s = stage_attachments(
            &c,
            &[AttachmentArg {
                id: "t-other/secret.pptx".into(),
                name: None,
            }],
        )
        .await
        .unwrap();
        assert!(s.files.is_empty());
        assert!(
            s.notes
                .iter()
                .any(|n| n.contains("not found in this conversation")),
            "{:?}",
            s.notes
        );
    }

    #[test]
    fn b64_round_trips() {
        assert_eq!(b64::encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64::decode("Zm9vYmFy").as_deref(), Some(&b"foobar"[..]));
    }

    #[test]
    fn schema_names_match_ids() {
        let c = client("http://x".into());
        assert_eq!(RunInSandbox(c.clone()).id(), "run_in_sandbox");
        assert_eq!(
            RunInSandbox(c.clone()).id(),
            RunInSandbox(c.clone()).schema().function.name
        );
        assert_eq!(GenerateDocument(c.clone()).id(), "generate_document");
        assert_eq!(CaptureWebpage(c.clone()).id(), "capture_webpage");
        assert_eq!(
            ConvertDocument(c.clone()).id(),
            ConvertDocument(c.clone()).schema().function.name
        );
        assert_eq!(ConvertDocument(c.clone()).id(), "convert_document");
        assert_eq!(
            EditPresentation(c.clone()).id(),
            EditPresentation(c.clone()).schema().function.name
        );
        assert_eq!(EditPresentation(c).id(), "edit_presentation");
        assert_eq!(ReadSandboxOutput.id(), "read_sandbox_output");
        assert_eq!(
            ReadSandboxOutput.id(),
            ReadSandboxOutput.schema().function.name
        );
    }

    #[test]
    fn shape_stream_passthrough_vs_pointer() {
        // No stored artifact → the stream is returned verbatim.
        assert_eq!(shape_stream("hello", None), json!("hello"));
        // Stored (large) → compact preview + ref, not the whole stream.
        let stored = json!({"name": "stdout.txt", "id": "t-1/stdout.txt", "status": "attached"});
        let big = "Z".repeat(20_000);
        let v = shape_stream(&big, Some(&stored));
        assert_eq!(v["full_output_ref"], json!("t-1/stdout.txt"));
        assert_eq!(v["truncated"], json!(true));
        assert!(v["preview"].as_str().unwrap().len() < big.len());
    }

    #[test]
    fn slice_text_head_tail_range_grep() {
        let text = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cap = 16 * 1024;

        let h = slice_text(&text, ReadAction::Head, None, None, None, 3, cap).unwrap();
        assert_eq!(h["returned_lines"], 3);
        assert_eq!(h["total_lines"], 20);
        assert_eq!(h["more_available"], json!(true)); // 17 more lines
        assert!(h["content"].as_str().unwrap().starts_with("1: line 1\n"));

        let t = slice_text(&text, ReadAction::Tail, None, None, None, 2, cap).unwrap();
        assert!(t["content"].as_str().unwrap().contains("20: line 20"));

        let r = slice_text(&text, ReadAction::Range, None, Some(5), Some(7), 100, cap).unwrap();
        assert_eq!(r["returned_lines"], 3);
        assert_eq!(r["more_available"], json!(false)); // whole range returned
        assert!(r["content"].as_str().unwrap().contains("5: line 5"));

        // `line 1$` matches only "line 1", not "line 10".."line 19".
        let g = slice_text(
            &text,
            ReadAction::Grep,
            Some("line 1$"),
            None,
            None,
            100,
            cap,
        )
        .unwrap();
        assert_eq!(g["matched_lines"], json!(1));
        assert_eq!(g["more_available"], json!(false));

        let e = slice_text(&text, ReadAction::Grep, None, None, None, 10, cap).unwrap_err();
        assert!(matches!(e, ToolError::InvalidArgs(_)));
    }

    #[test]
    fn sandbox_tools_override_loop_timeout_to_cover_http_timeout() {
        // The runner enforces max_duration around the tool; it must exceed
        // the client's own HTTP timeout (so the clean reqwest timeout fires
        // first), which is `timeout_secs + 15`.
        let c = client("http://x".into()); // timeout_secs = 5
        let d = RunInSandbox(c.clone())
            .max_duration()
            .expect("override set");
        assert_eq!(d, std::time::Duration::from_secs(5 + 15));
        assert!(GenerateDocument(c.clone()).max_duration().is_some());
        assert!(CaptureWebpage(c).max_duration().is_some());

        // With the real default HTTP timeout (120s) it comfortably exceeds
        // the runner's 30s default ceiling — the gap this fixes.
        let real = SandboxClient::new(
            Arc::new(SandboxConfig {
                enabled: true,
                runner_url: "http://x".into(),
                timeout_secs: 120,
                max_artifact_bytes: 1024,
            }),
            "https://gw.example".into(),
        );
        assert!(RunInSandbox(real).max_duration().unwrap() > std::time::Duration::from_secs(30));
    }

    #[test]
    fn assemble_inputs_stages_within_budget() {
        let items = vec![
            StageItem {
                name: "deck.pptx".into(),
                id: "t/deck.pptx".into(),
                bytes: vec![1, 2, 3],
            },
            StageItem {
                name: "data.csv".into(),
                id: "t/data.csv".into(),
                bytes: vec![4, 5],
            },
        ];
        let (files, staged, notes) = assemble_inputs(items, 1024);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "deck.pptx");
        assert_eq!(b64::decode(&files[0].content_b64).unwrap(), vec![1, 2, 3]);
        assert_eq!(staged[0]["id"], "t/deck.pptx");
        assert_eq!(staged[0]["size"], 3);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn assemble_inputs_skips_over_budget_with_note() {
        let items = vec![
            StageItem {
                name: "small.bin".into(),
                id: "t/small.bin".into(),
                bytes: vec![0; 10],
            },
            StageItem {
                name: "big.bin".into(),
                id: "t/big.bin".into(),
                bytes: vec![0; 100],
            },
        ];
        // Budget fits the first file but not the second.
        let (files, staged, notes) = assemble_inputs(items, 50);
        assert_eq!(files.len(), 1, "only the in-budget file is staged");
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0]["name"], "small.bin");
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("big.bin") && notes[0].contains("budget"),
            "{notes:?}"
        );
    }

    #[test]
    fn assemble_inputs_dedupes_colliding_names_with_note() {
        let items = vec![
            StageItem {
                name: "deck.pptx".into(),
                id: "t1/deck.pptx".into(),
                bytes: vec![1],
            },
            StageItem {
                name: "deck.pptx".into(),
                id: "t2/deck.pptx".into(),
                bytes: vec![2],
            },
        ];
        let (files, staged, notes) = assemble_inputs(items, 1024);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "deck.pptx");
        assert_eq!(files[1].name, "deck-2.pptx", "second collides → suffixed");
        assert_eq!(staged[1]["name"], "deck-2.pptx");
        assert!(notes.iter().any(|n| n.contains("deck-2.pptx")), "{notes:?}");
    }

    #[test]
    fn sanitize_rejects_traversal() {
        assert!(sanitize_filename("ok.pptx").is_some());
        assert!(sanitize_filename("../etc/passwd").is_none());
        assert!(sanitize_filename("a/b").is_none());
        assert!(sanitize_filename("").is_none());
    }

    #[tokio::test]
    async fn run_in_sandbox_posts_and_maps_result() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/run"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "exit_code": 0, "stdout": "hello\n", "stderr": "",
                "artifacts": [], "duration_ms": 12, "timed_out": false
            })))
            .mount(&server)
            .await;
        let tool = RunInSandbox(client(server.uri()));
        let out = tool
            .run(
                ctx().await,
                json!({"language": "python", "code": "print('hello')"}),
            )
            .await
            .unwrap();
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["stdout"], "hello\n");
        assert_eq!(out["artifacts"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn artifact_without_s3_is_reported_not_stored() {
        let server = MockServer::start().await;
        // 3-byte file "PNG" base64 = "UE5H".
        Mock::given(method("POST"))
            .and(path("/run"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "exit_code": 0, "stdout": "", "stderr": "",
                "artifacts": [{"name": "out.png", "size": 3, "mime": "image/png", "content_b64": "UE5H"}],
                "duration_ms": 5, "timed_out": false
            })))
            .mount(&server)
            .await;
        let tool = RunInSandbox(client(server.uri()));
        let out = tool
            .run(ctx().await, json!({"language": "bash", "code": "true"}))
            .await
            .unwrap();
        let arts = out["artifacts"].as_array().unwrap();
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0]["status"], "not_stored");
        assert_eq!(arts[0]["name"], "out.png");
    }

    #[tokio::test]
    async fn runner_error_envelope_surfaces_to_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/run"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "network egress requested but not configured on this runner"
            })))
            .mount(&server)
            .await;
        let tool = CaptureWebpage(client(server.uri()));
        let err = tool
            .run(ctx().await, json!({"url": "https://example.com"}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Failed(ref m) if m.contains("egress")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_empty_code_and_bad_url() {
        let c = client("http://unused".into());
        let e1 = RunInSandbox(c.clone())
            .run(ctx().await, json!({"language": "python", "code": "  "}))
            .await
            .unwrap_err();
        assert!(matches!(e1, ToolError::InvalidArgs(_)));
        let e2 = CaptureWebpage(c)
            .run(ctx().await, json!({"url": "ftp://nope"}))
            .await
            .unwrap_err();
        assert!(matches!(e2, ToolError::InvalidArgs(_)));
    }
}
