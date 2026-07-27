// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Code-execution sandbox tools.
//!
//! The model writes Python or shell; the gateway forwards it to the
//! standalone `sandbox-runner` service, which executes it inside an
//! ephemeral, isolated sandbox and returns stdout/stderr plus any files
//! the run produced. Containers are single-use by default; `run_in_sandbox`
//! keeps one alive across a conversation turn (a [`SandboxLease`]) so
//! successive calls reuse `/work`. The runner enforces the real isolation
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
use gateway_core::server::config::SandboxConfig;
use gateway_features::server::chat_attachments::{self, AttachmentRef};
use gateway_features::server::file_refs;

/// Model-facing description of the `run_in_sandbox` tool. Extracted to a
/// module const so `RunInSandbox::schema` reads as structure (the JSON
/// argument schema) rather than being dominated by this ~70-line prose block
/// — the two are edited for entirely different reasons (what the sandbox can
/// do vs. the tool's argument shape).
const RUN_IN_SANDBOX_DESC: &str = "Run Python or shell in a secure, isolated sandbox (a throwaway VM \
     scoped to this conversation turn) and get back stdout, stderr, and \
     any files it produced — like a capable system-engineer shell. Use it to \
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
     pypdf/pdfplumber/pymupdf, reportlab (outline fonts only — \
     emoji need `pdfmetrics.registerFont(TTFont('NotoEmoji', \
     '/usr/share/fonts/truetype/notoemoji/NotoEmoji.ttf'))`, \
     monochrome; for COLOR emoji or mixed Latin+CJK+emoji PDFs \
     prefer weasyprint or typst — their font fallback handles it, \
     e.g. Noto Sans CJK JP covers Latin+Japanese+Chinese and Noto \
     Color Emoji chains in automatically), img2pdf, pillow, opencv, \
     pytesseract (OCR), segno + qrcode (QR codes in generated \
     documents; for a standalone QR code prefer the faster \
     `generate_qr_code` tool), \
     sqlalchemy/psycopg/pymysql, scapy, lxml, \
     beautifulsoup4, requests. \
     CLI tools: ripgrep (rg), jq, yq, jc, awk/sed, duckdb + sqlite3 \
     (SQL over CSV/JSON/Parquet/large logs), ffmpeg, imagemagick, vips, \
     tesseract (OCR), tshark/tcpdump (read .pcap), graphviz (dot), \
     LibreOffice (`soffice --headless` for office↔pdf), pandoc, typst \
     (with the offline `@preview/gribouille` ggplot-style charts \
     package), excalirender (`.excalidraw` scene → svg/png/pdf), \
     ghostscript/qpdf, poppler-utils (pdftotext/pdftoppm), \
     gzip/zstd/xz/bzip2/7z, git, curl/wget, dig/rsync, file/xxd, lnav, \
     and a C toolchain (gcc/make). Headless chromium is available too. \
     Fonts: the common document families are installed — Calibri/\
     Cambria/Arial/Times metric substitutes (Carlito, Caladea, \
     Liberation), Inter, Roboto, Open Sans, Lato, Montserrat, \
     Poppins, Oswald, Raleway, Work Sans, Nunito, EB Garamond, \
     Merriweather, Playfair Display, IBM Plex, JetBrains Mono, \
     Fira Code, Noto (incl. CJK + emoji) — `fc-list : family` \
     shows all. \
     The sandbox PERSISTS for this turn: files you write to the \
     working directory, and scratch state survive between calls, so \
     you can iterate — run something, read the output, adjust, and \
     run again — instead of cramming everything into one call. Set \
     `fresh: true` to start over in a clean container. \
     Files a user uploaded this turn are ALREADY waiting \
     in the working directory under their original names — just open \
     them. To also work on a file from earlier in the conversation — \
     including files a previous sandbox/render call produced — pass \
     its id (from an `[attached … id=\"<turn>/<file>\"]` stub) or \
     simply its filename in `attachments`; it gets fetched into the \
     working directory too. REUSE existing files this way instead of \
     regenerating them. \
     The result's `staged_files` lists what's in the directory and \
     `available_attachments` lists other files you can pull in by id. \
     The environment is a FIXED image: everything listed above is \
     preinstalled, and NOTHING can be installed — never try \
     pip/apt/npm install (there is normally no network, and the \
     sandbox is discarded at the end of the turn). If a library is \
     missing, solve the task with the preinstalled ones. Write files \
     to the current working directory to return them to the user. \
     When stdout/stderr is large you get a small preview plus a \
     `full_output_ref` — call read_sandbox_output with that ref to \
     grep/page the rest instead of pulling it all into context. Best \
     practice: filter/aggregate in-sandbox (grep, awk, duckdb, \
     head/tail) and print a concise summary rather than dumping raw \
     data.";

/// Appended to [`RUN_IN_SANDBOX_DESC`] when the runner can grant egress.
const RUN_IN_SANDBOX_NET_ON: &str = " Network is OFF by default: set \
     `network: true` for a run that needs web access. It is fixed when the \
     container starts, so changing it mid-turn also needs `fresh: true`.";

/// Appended instead when the runner positively reports no egress.
///
/// Stated as a property of the environment rather than as a missing option,
/// because a model told "you may not" retries and argues, while a model told
/// "there is no network here" adapts. The `network` argument is absent from
/// the schema in this case, so mentioning it at all would only invite a call
/// that gets rejected as an unknown property.
const RUN_IN_SANDBOX_NET_OFF: &str = " This sandbox has NO network at all: \
     web requests, downloads and installs are impossible, so don't attempt \
     them or suggest that a retry might work. Everything must come from the \
     preinstalled libraries and the files in the working directory.";

/// The model-facing description, which depends on whether this deployment's
/// runner can grant egress.
pub(crate) fn run_in_sandbox_desc(egress: bool) -> String {
    let suffix = if egress {
        RUN_IN_SANDBOX_NET_ON
    } else {
        RUN_IN_SANDBOX_NET_OFF
    };
    format!("{RUN_IN_SANDBOX_DESC}{suffix}")
}

/// Shared HTTP client + config for the sandbox tool family. Held behind an
/// `Arc` so the generic tool and each specialized wrapper share one
/// connection pool.
pub struct SandboxClient {
    cfg: Arc<SandboxConfig>,
    /// Gateway public base URL, for building absolute artifact download
    /// links on the API path. Cloned from `config.gateway.public_url`.
    public_url: String,
    http: reqwest::Client,
    /// What the runner said about egress on the last probe. Three-state, and
    /// the distinction matters: see [`SandboxClient::egress_available`].
    egress: std::sync::atomic::AtomicU8,
}

/// Encoding for [`SandboxClient::egress`]. An atomic rather than a lock: it is
/// read on every `schema()` call (once per advertised tool list) and written
/// once at boot.
const EGRESS_UNKNOWN: u8 = 0;
const EGRESS_YES: u8 = 1;
const EGRESS_NO: u8 = 2;

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
            egress: std::sync::atomic::AtomicU8::new(EGRESS_UNKNOWN),
        })
    }

    /// Ask the runner what it can do, and remember the answer.
    ///
    /// Called once at boot. Not refreshed: egress is a deployment property
    /// (a podman network + a proxy), so it changes when an operator edits a
    /// unit file and restarts things — at which point the gateway restarts
    /// too. A periodic re-probe would buy a capability set that changes under
    /// a running conversation, which is worse than a stale one.
    ///
    /// Never fatal. An unreachable runner leaves the state `UNKNOWN`, which
    /// [`Self::egress_available`] reads optimistically — see there for why.
    pub async fn probe_capabilities(&self) {
        use std::sync::atomic::Ordering;
        let url = format!("{}/healthz", self.cfg.runner_url.trim_end_matches('/'));
        let health = match self.http.get(&url).send().await {
            Ok(resp) => resp.json::<shared::sandbox::RunnerHealth>().await.ok(),
            Err(e) => {
                tracing::warn!(error = %e,
                    "sandbox runner health probe failed; assuming egress is available \
                     (the runner is unreachable, so every sandbox tool is failing anyway)");
                None
            }
        };
        match health.and_then(|h| h.egress) {
            Some(true) => {
                self.egress.store(EGRESS_YES, Ordering::Relaxed);
                tracing::info!("sandbox runner grants network egress; web tools enabled");
            }
            Some(false) => {
                self.egress.store(EGRESS_NO, Ordering::Relaxed);
                tracing::info!(
                    "sandbox runner has NO network egress configured \
                     (SANDBOX_EGRESS_NETWORK unset): capture_webpage / browse_page will not \
                     be offered, and run_in_sandbox will not advertise `network`"
                );
            }
            None => tracing::warn!(
                "sandbox runner did not report its egress capability (older runner?); \
                 assuming it is available"
            ),
        }
    }

    /// Whether to offer capabilities that need the network.
    ///
    /// `true` unless the runner *positively said no*. Unknown counts as
    /// available on purpose: the only ways to land there are a runner that is
    /// down (in which case every sandbox tool fails regardless, and hiding
    /// some of them would just make a transient outage look like a permanent
    /// capability loss) or one older than the health field. Withdrawing tools
    /// needs evidence; keeping them needs none.
    pub fn egress_available(&self) -> bool {
        self.egress.load(std::sync::atomic::Ordering::Relaxed) != EGRESS_NO
    }

    /// Wall-clock ceiling the tool runner should allow around a sandbox
    /// call: the HTTP timeout plus margin, so the client's own timeout
    /// (producing a clean error) fires before the loop cancels the future.
    /// `pub` so tools that call the sandbox indirectly (the typst templates,
    /// which run a pptx/docx export in the sandbox as part of a render) can
    /// size their own `max_duration` to it instead of the 30 s default.
    pub fn loop_timeout(&self) -> Duration {
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

    /// Release a leased (kept-alive) container on the runner. Best-effort:
    /// a failure is logged, never surfaced — the runner's TTL sweeper reaps
    /// anything a failed DELETE left behind, and the id is turn-scoped so a
    /// leak is never reachable by another turn. Idempotent on the runner
    /// side (unknown id → `204`).
    pub async fn release_container(&self, id: &str) {
        let url = format!(
            "{}/container/{}",
            self.cfg.runner_url.trim_end_matches('/'),
            urlencode_segment(id),
        );
        match self.http.delete(&url).send().await {
            Ok(resp) if !resp.status().is_success() => {
                tracing::warn!(container = %id, status = %resp.status(),
                    "sandbox container release returned non-2xx (TTL sweeper will reap)")
            }
            Err(e) => tracing::warn!(container = %id, error = %e,
                "sandbox container release failed (TTL sweeper will reap)"),
            _ => {}
        }
    }

    /// Run a job and shape the model-facing result, delivering any
    /// produced files appropriately for the call's context.
    async fn execute(&self, ctx: &ToolContext, req: RunRequest) -> Result<Value, ToolError> {
        let resp = self.call_runner(&req).await?;
        self.shape_response(ctx, resp).await
    }

    /// Turn a completed [`RunResponse`] into the model-facing tool result:
    /// deliver artifacts (chat attachment / API URL), and shape stdout/stderr
    /// into previews with `full_output_ref` handles. Split out of
    /// [`Self::execute`] so the lease-managed `run_in_sandbox` path can run
    /// the job through a [`SandboxLease`] and still share this shaping.
    async fn shape_response(
        &self,
        ctx: &ToolContext,
        resp: RunResponse,
    ) -> Result<Value, ToolError> {
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
                 repeat any marker/URL text in your prose. They are saved to this \
                 conversation permanently: to use one in a LATER tool call, pass \
                 its `id` (or just its filename) in `attachments` instead of \
                 regenerating it."
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
        s3: &gateway_core::server::config::S3Config,
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
        s3: &gateway_core::server::config::S3Config,
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

/// A per-turn hold on one sandbox container, so successive `run_in_sandbox`
/// calls in a single conversation turn reuse the same environment (`/work`
/// and scratch state persist between calls) instead of each getting a fresh,
/// destroyed-after container.
///
/// Lifecycle:
/// - **first call** — `container_id` is `None`; the runner creates a
///   container, keeps it alive, and echoes its id, which we store.
/// - **later calls** — we send the stored id; the runner `exec`s into the
///   same container. A network-posture change (or an explicit `fresh`)
///   releases the old container and starts a new one.
/// - **turn end** — the driver calls [`Self::release`]; the `Drop` guard is
///   the backstop for any path that skips it, and the runner's TTL sweeper
///   backs *that* up.
///
/// The [`tokio::sync::Mutex`] is held across the whole runner round-trip, so
/// concurrent `run_in_sandbox` calls in one tool round serialize: the first
/// establishes the lease and the rest reuse it (first-writer-wins on the
/// network posture) rather than racing to create parallel containers.
///
/// This serialization is **intentional and required**, not incidental: the
/// calls share one container's `/work`, and the in-container agent attributes
/// produced files by diffing `/work` against a snapshot taken just before the
/// job runs (see `sandbox-image/sandbox-agent`). Two execs mutating that
/// `/work` at once would race the snapshot and cross-attribute each other's
/// outputs. The cost is that multiple `run_in_sandbox` calls emitted in a
/// *single* round no longer run in parallel — but the iterate-on-`/work`
/// pattern this feature exists for is inherently sequential (run, read result,
/// adjust, rerun across rounds), so that case is rare. Callers that genuinely
/// need parallel independent sandboxes should emit them across rounds.
pub struct SandboxLease {
    client: Arc<SandboxClient>,
    state: tokio::sync::Mutex<LeaseState>,
}

#[derive(Default)]
struct LeaseState {
    /// The live container's id, once established.
    container_id: Option<String>,
    /// Whether the current container was created with egress. Used to
    /// auto-recreate when a later call flips the network posture (the runner
    /// fixes network at creation and ignores it on reuse).
    network: bool,
}

impl SandboxLease {
    pub fn new(client: Arc<SandboxClient>) -> Arc<Self> {
        Arc::new(Self {
            client,
            state: tokio::sync::Mutex::new(LeaseState::default()),
        })
    }

    /// Run one `run_in_sandbox` job against the turn's leased container,
    /// creating/reusing/recreating it as needed. `explicit_fresh` (the tool's
    /// `fresh: true`) forces a brand-new container even at the same network
    /// posture — a deliberate "start clean". Returns the raw [`RunResponse`];
    /// the caller shapes it via [`SandboxClient::shape_response`].
    pub async fn run(
        &self,
        mut req: RunRequest,
        explicit_fresh: bool,
    ) -> Result<RunResponse, ToolError> {
        let mut st = self.state.lock().await;
        let want_net = req.network;
        // A network-posture change can't be honored by reusing a container
        // (the runner fixes egress at creation), so recreate — same as an
        // explicit `fresh`.
        let need_fresh = explicit_fresh || (st.container_id.is_some() && st.network != want_net);
        // When recreating, DON'T release the old container up front: send the
        // job with `container_id: None` so the runner makes a new one, and only
        // release the old one AFTER that succeeds. Otherwise a recreate that
        // fails (e.g. egress requested on a runner without it) would have
        // already destroyed the turn's `/work` with nothing to fall back to.
        let old_on_recreate = if need_fresh {
            st.container_id.clone()
        } else {
            None
        };
        req.container_id = if need_fresh {
            None
        } else {
            st.container_id.clone()
        };
        req.keep_alive = true;
        let resp = self.client.call_runner(&req).await?;
        // Success: retire the old container now that the new one is running.
        // (Skip if the runner somehow handed us the same id back.)
        if let Some(old) = old_on_recreate
            && resp.container_id.as_deref() != Some(old.as_str())
        {
            self.client.release_container(&old).await;
        }
        // The runner echoes an id only while it holds the lease; if capacity
        // was exhausted it ran single-use and returned `None`, so we drop our
        // stored id and the next call starts fresh (still correct, just no
        // persistence).
        st.container_id = resp.container_id.clone();
        st.network = want_net;
        Ok(resp)
    }

    /// Release the leased container (turn end / reset). Idempotent.
    pub async fn release(&self) {
        let mut st = self.state.lock().await;
        if let Some(id) = st.container_id.take() {
            self.client.release_container(&id).await;
        }
    }
}

impl Drop for SandboxLease {
    /// Backstop for any turn-exit path that didn't call [`Self::release`]
    /// (a panic, an early return we missed): spawn a best-effort DELETE.
    /// `Drop` can't await, so we grab the id via `try_lock` (uncontended at
    /// drop time) and hand the release to a task. If we're outside a runtime
    /// or the lock is held, the runner's TTL sweeper is the final backstop.
    fn drop(&mut self) {
        let Ok(mut st) = self.state.try_lock() else {
            return;
        };
        let Some(id) = st.container_id.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            handle.spawn(async move { client.release_container(&id).await });
        }
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
    /// Canvas documents named in `attachments` rather than `documents`.
    /// Staging them needs the documents store, not S3, so they are handed
    /// back for the caller to merge into its `documents` list — one code
    /// path materialises documents, whichever argument named them.
    documents: Vec<DocumentArg>,
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
        documents: vec![],
    };
    let Some(session_id) = ctx.session_id.as_deref() else {
        // No session (proxy/`/v1`): nothing to resolve against. If the model
        // named ids anyway, say why they were ignored rather than failing the
        // whole run.
        if explicit.is_empty() {
            return Ok(empty());
        }
        let mut s = empty();
        s.notes.push(
            "Attachments can't be staged on this path (no chat session). Ran without them.".into(),
        );
        return Ok(s);
    };
    // Storage gates *attachments*, not canvas documents — those live in the
    // DB. A deployment without `[chat.s3]` can still stage a document the
    // model names here, so resolve first and only skip the S3 half.
    let s3 = match ctx.s3.as_ref() {
        Some(s3) => Some(s3),
        None => {
            if explicit.is_empty() {
                return Ok(empty());
            }
            None
        }
    };

    let (session_atts, round) =
        chat_attachments::session_and_round_attachments(&ctx.db, session_id)
            .await
            .map_err(|e| ToolError::Failed(format!("listing session attachments: {e}")))?;
    // Without storage the round's uploads can't be fetched either, so only
    // explicitly-named references (which may be documents) are worth walking.
    let round: Vec<_> = if s3.is_some() { round } else { Vec::new() };

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
    let mut doc_args: Vec<DocumentArg> = Vec::new();
    for arg in explicit {
        // One resolver, every spelling: an exact `<turn>/<file>` id, a bare
        // filename (newest match wins — models lose track of turn ids across
        // rounds), an `att:` ref, or a canvas document. A document id landing
        // in `attachments` used to be an "ignored, no such attachment" note
        // even though the file was right there; it now stages, so the model
        // doesn't have to know which of the two arguments an id belongs in.
        let resolved = match file_refs::resolve(&ctx.db, Some(session_id), &arg.id, None).await {
            Ok(r) => r,
            Err(err) => {
                notes.push(format!("Ignored `{}`: {err}", arg.id));
                continue;
            }
        };
        if resolved.is_document() {
            doc_args.push(DocumentArg {
                document_id: resolved.id(),
                version: None,
                name: arg.name.clone(),
            });
            continue;
        }
        let id = resolved.id();
        if id != arg.id {
            notes.push(format!("Resolved `{}` to attachment `{id}`.", arg.id));
        }
        if seen_ids.insert(id.clone()) {
            want.push((id, arg.name.clone()));
        } else if let Some(n) = &arg.name {
            // Already queued (it's a round file); honor a rename request.
            if let Some(slot) = want.iter_mut().find(|(qid, _)| qid == &id) {
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
        let Some(s3) = s3 else {
            notes.push(format!(
                "Could not stage attachment `{id}`: chat attachments are not configured \
                 on this gateway (canvas documents still work)."
            ));
            continue;
        };
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
        documents: doc_args,
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
    /// Canvas documents to materialize into `/work` server-side — the
    /// big-content path (a large typst/markdown source stays in the
    /// documents store instead of riding the tool-call payload).
    #[serde(default)]
    documents: Vec<DocumentArg>,
    #[serde(default)]
    network: bool,
    /// Start from a clean container, discarding anything earlier calls in
    /// this turn wrote to `/work`. Also the way to change `network` mid-turn
    /// (the sandbox fixes egress at creation).
    #[serde(default)]
    fresh: bool,
}

/// One canvas document to stage into a run's working directory.
#[derive(Deserialize)]
struct DocumentArg {
    /// Id from `create_document`.
    document_id: String,
    /// Specific version (default: latest).
    #[serde(default)]
    version: Option<i64>,
    /// Filename in `/work` (default: `<title>.<format-ext>`, sanitized).
    #[serde(default)]
    name: Option<String>,
}

/// Materialize the requested canvas documents as input files. Best-effort
/// per document (a bad id becomes a note, not a failed run); returns the
/// staged descriptors for the result body. Chat-path only — without a
/// session there is no documents store to read.
async fn stage_documents(
    ctx: &ToolContext,
    wanted: &[DocumentArg],
    files: &mut Vec<InputFile>,
    notes: &mut Vec<String>,
) -> Vec<Value> {
    use gateway_core::server::db::documents;
    if wanted.is_empty() {
        return Vec::new();
    }
    let Some(session_id) = ctx.session_id.as_deref() else {
        notes.push(
            "Canvas documents can't be staged on this path (no chat session). Ran without \
             them."
                .into(),
        );
        return Vec::new();
    };
    let mut staged = Vec::new();
    for d in wanted {
        match documents::get_version(&ctx.db, session_id, &d.document_id, d.version).await {
            Ok(Some((doc, _))) if doc.is_deleted() => {
                // Resolvable but deleted: skip it with a note rather than
                // staging content the user removed. A note (not an error) —
                // the rest of the run is still worth doing.
                notes.push(format!(
                    "Skipped canvas document `{}`: it is deleted. Call                      `undelete_document` first if you meant to use it.",
                    d.document_id
                ));
                continue;
            }
            Ok(Some((doc, ver))) => {
                let name = match d.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(n) => match sanitize_filename(n) {
                        Some(n) => n,
                        None => {
                            notes.push(format!(
                                "Ignored canvas document `{}`: `{n}` is not a valid filename.",
                                d.document_id
                            ));
                            continue;
                        }
                    },
                    None => format!("{}.{}", safe_stem(&doc.title), doc.format.file_ext()),
                };
                files.push(InputFile {
                    name: name.clone(),
                    content_b64: b64::encode(ver.content.as_bytes()),
                });
                staged.push(json!({
                    "document_id": d.document_id,
                    "version": ver.version,
                    "name": name,
                }));
            }
            Ok(None) => notes.push(format!(
                "Ignored canvas document `{}`: not found in this conversation.",
                d.document_id
            )),
            Err(e) => notes.push(format!(
                "Could not read canvas document `{}`: {e}",
                d.document_id
            )),
        }
    }
    staged
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

mod browse;
mod capture;
mod convert_edit;
mod generate_export;
mod read;
mod render;
mod run;

pub use browse::*;
pub use capture::*;
pub use convert_edit::*;
pub use generate_export::*;
pub use read::*;
pub use render::*;
pub use run::*;

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_core::server::db;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn ctx() -> ToolContext {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        ToolContext::for_test(pool)
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
    fn dead_s3() -> std::sync::Arc<gateway_core::server::config::S3Config> {
        std::sync::Arc::new(gateway_core::server::config::S3Config {
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
            matches!(err, ToolError::InvalidArgs(ref m) if m.contains("no attachment with id or filename")),
            "{err:?}"
        );

        // A bare filename resolves to the session attachment with that name
        // (the fetch then fails on the dead S3 stub, which proves resolution
        // got as far as fetching the right id).
        let err = resolve_one_attachment(&c, Some("b.pptx"), "PowerPoint (.pptx) file", is_pptx)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Failed(ref m) if m.contains("t-u1/b.pptx")),
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
        // The wording is the shared resolver's, so a wrong reference reads the
        // same here as it does from `fetch_attachment` or `offer_download` —
        // and it names both inventories, because either store could have held
        // what the model meant.
        assert!(
            s.notes.iter().any(|n| {
                n.contains("no file or document named")
                    && n.contains("list_attachments")
                    && n.contains("list_documents")
            }),
            "{:?}",
            s.notes
        );
    }

    #[tokio::test]
    async fn stage_attachments_resolves_bare_filenames_to_the_newest_match() {
        let mut c = ctx().await;
        let m = chat_attachments::marker_line(
            "t-u1",
            &chat_attachments::UploadOutcome {
                filename: "qr.png".into(),
                mime: "image/png".into(),
                bytes: 1,
            },
        );
        seed_session_with_upload(&c.db, "t-u1", &m).await;
        c.session_id = Some("s1".into());
        c.s3 = Some(dead_s3());
        let s = stage_attachments(
            &c,
            &[AttachmentArg {
                id: "qr.png".into(),
                name: None,
            }],
        )
        .await
        .unwrap();
        // Resolution happened (and is surfaced); only the fetch fails on the
        // dead S3 stub.
        assert!(
            s.notes
                .iter()
                .any(|n| n.contains("Resolved `qr.png` to attachment `t-u1/qr.png`")),
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

    // --- render_excalidraw / render_typst ---------------------------------

    #[test]
    fn image_format_ext_maps() {
        assert_eq!(ImageFormat::Svg.ext(), "svg");
        assert_eq!(ImageFormat::Png.ext(), "png");
        assert_eq!(ImageFormat::Pdf.ext(), "pdf");
    }

    #[test]
    fn render_excalidraw_schema_defaults_svg_and_needs_no_required() {
        let tool = RenderExcalidraw(client("http://unused".into()));
        assert_eq!(tool.id(), "render_excalidraw");
        let def = tool.schema();
        let params = serde_json::to_value(&def.function.parameters).unwrap();
        // Neither source is mandatory (scene OR attachment OR round upload).
        assert!(params.get("required").is_none() || params["required"] == json!([]));
        assert_eq!(
            params["properties"]["format"]["enum"],
            json!(["svg", "png", "pdf"])
        );
        for k in ["scene", "attachment_id", "format", "filename"] {
            assert!(params["properties"].get(k).is_some(), "missing prop {k}");
        }
    }

    #[tokio::test]
    async fn render_excalidraw_rejects_malformed_scene_before_any_runner_call() {
        // Invalid JSON must fail fast as InvalidArgs — note the runner URL is
        // unreachable, so a passing test also proves no HTTP call was made.
        let tool = RenderExcalidraw(client("http://127.0.0.1:1".into()));
        let err = tool
            .run(ctx().await, json!({"scene": "{not json", "format": "png"}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(ref m) if m.contains("not valid JSON")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn render_excalidraw_ships_scene_through_excalirender() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/run"))
            // The recipe must invoke excalirender on the staged scene and
            // honour the requested format + filename.
            .and(wiremock::matchers::body_string_contains(
                "excalirender diagram.excalidraw",
            ))
            .and(wiremock::matchers::body_string_contains("flow.png"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "exit_code": 0, "stdout": "", "stderr": "",
                "artifacts": [], "duration_ms": 5, "timed_out": false
            })))
            .mount(&server)
            .await;
        let tool = RenderExcalidraw(client(server.uri()));
        let out = tool
            .run(
                ctx().await,
                json!({
                    "scene": "{\"type\":\"excalidraw\",\"elements\":[]}",
                    "format": "png",
                    "filename": "flow"
                }),
            )
            .await
            .unwrap();
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["source"], json!({"from": "scene"}));
    }

    #[test]
    fn normalize_excalidraw_points_reshapes_flat_arrays() {
        let mut v = json!({"type": "excalidraw", "elements": [
            {"type": "arrow", "points": [0, 0, 0, 90]},      // flat -> pairs
            {"type": "line", "points": [[1, 2], [3, 4]]},    // already nested
            {"type": "arrow", "points": [1, 2, 3]},          // odd length: left as-is
            {"type": "rectangle", "x": 0}                    // no points
        ]});
        normalize_excalidraw_points(&mut v);
        let els = v["elements"].as_array().unwrap();
        assert_eq!(els[0]["points"], json!([[0, 0], [0, 90]]));
        assert_eq!(els[1]["points"], json!([[1, 2], [3, 4]]));
        assert_eq!(els[2]["points"], json!([1, 2, 3]));
        assert_eq!(els[3].get("points"), None);
    }

    #[test]
    fn normalize_excalidraw_points_tolerates_odd_shapes() {
        // Missing / non-array `elements` must not panic.
        let mut a = json!({"type": "excalidraw"});
        normalize_excalidraw_points(&mut a);
        let mut b = json!({"elements": "nope"});
        normalize_excalidraw_points(&mut b);
    }

    #[tokio::test]
    async fn render_excalidraw_ships_normalized_points() {
        // Pin the wiring: a scene with a FLAT `points` array must reach the
        // renderer reshaped into `[x,y]` pairs (the fix for the opaque
        // "number is not iterable" excalirender error).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/run"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "exit_code": 0, "stdout": "", "stderr": "",
                "artifacts": [], "duration_ms": 5, "timed_out": false
            })))
            .mount(&server)
            .await;
        let tool = RenderExcalidraw(client(server.uri()));
        tool.run(
            ctx().await,
            json!({
                "scene": "{\"type\":\"excalidraw\",\"elements\":[\
                    {\"type\":\"arrow\",\"points\":[0,0,0,90]}]}",
            }),
        )
        .await
        .unwrap();

        // Decode the scene the runner received and assert it carries nested pairs.
        let reqs = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let scene_b64 = body["files"][0]["content_b64"].as_str().unwrap();
        let scene_bytes = b64::decode(scene_b64).unwrap();
        let scene: Value = serde_json::from_slice(&scene_bytes).unwrap();
        assert_eq!(scene["elements"][0]["points"], json!([[0, 0], [0, 90]]));
    }

    #[test]
    fn render_typst_schema_offers_source_or_canvas_document() {
        let tool = RenderTypst(client("http://unused".into()));
        assert_eq!(tool.id(), "render_typst");
        let def = tool.schema();
        let params = serde_json::to_value(&def.function.parameters).unwrap();
        // `source` XOR `document_id` — neither is schema-required (the
        // runtime enforces exactly one), both are advertised.
        assert_eq!(params["required"], json!([]));
        assert!(params["properties"].get("source").is_some());
        assert!(params["properties"].get("document_id").is_some());
        assert!(params["properties"].get("version").is_some());
        assert_eq!(
            params["properties"]["format"]["enum"],
            json!(["pdf", "png", "svg"])
        );
        assert!(params["properties"].get("attachments").is_some());
    }

    #[tokio::test]
    async fn render_typst_rejects_empty_source() {
        let tool = RenderTypst(client("http://127.0.0.1:1".into()));
        let err = tool
            .run(ctx().await, json!({"source": "   "}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }

    #[tokio::test]
    async fn render_typst_compiles_source_to_requested_format() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/run"))
            .and(wiremock::matchers::body_string_contains(
                "typst compile in.typ",
            ))
            .and(wiremock::matchers::body_string_contains("document.svg"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "exit_code": 0, "stdout": "", "stderr": "",
                "artifacts": [], "duration_ms": 7, "timed_out": false
            })))
            .mount(&server)
            .await;
        let tool = RenderTypst(client(server.uri()));
        let out = tool
            .run(
                ctx().await,
                json!({"source": "#set page(width: auto)\n= Hi", "format": "svg"}),
            )
            .await
            .unwrap();
        assert_eq!(out["exit_code"], 0);
    }

    #[tokio::test]
    async fn render_typst_requires_source_xor_document_id() {
        let tool = RenderTypst(client("http://127.0.0.1:1".into()));
        // Neither → clear error naming both options.
        let err = tool.run(ctx().await, json!({})).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(ref m) if m.contains("document_id")),
            "{err:?}"
        );
        // Both → rejected.
        let err = tool
            .run(
                ctx().await,
                json!({"source": "= Hi", "document_id": "doc_x"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(ref m) if m.contains("not both")),
            "{err:?}"
        );
    }

    /// Seed a session + one canvas document; returns (ctx, document_id).
    async fn ctx_with_canvas_doc(format: &str, content: &str) -> (ToolContext, String) {
        use gateway_core::server::db::documents::{self, DocumentFormat};
        let mut c = ctx().await;
        seed_session_with_upload(&c.db, "t-seed", "hello").await;
        c.session_id = Some("s1".into());
        let id = documents::new_id();
        documents::create(
            &c.db,
            &id,
            "s1",
            "u",
            "My Deck",
            DocumentFormat::parse(format).unwrap(),
            content,
            None,
        )
        .await
        .unwrap();
        (c, id)
    }

    #[tokio::test]
    async fn render_typst_renders_from_canvas_typst_document() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/run"))
            .and(wiremock::matchers::body_string_contains(
                "typst compile in.typ",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "exit_code": 0, "stdout": "", "stderr": "",
                "artifacts": [], "duration_ms": 7, "timed_out": false
            })))
            .mount(&server)
            .await;
        let (c, id) = ctx_with_canvas_doc("typst", "= Hi from the canvas\n").await;
        let tool = RenderTypst(client(server.uri()));
        let out = tool
            .run(c, json!({"document_id": id, "format": "pdf"}))
            .await
            .unwrap();
        assert_eq!(out["exit_code"], 0);
        // The result names its canvas source, so the model re-renders the
        // same document after edits instead of pasting source inline.
        assert_eq!(out["canvas_document_id"], json!(id));
        assert_eq!(out["canvas_version"], 1);
    }

    #[tokio::test]
    async fn render_typst_rejects_non_typst_canvas_document() {
        let (c, id) = ctx_with_canvas_doc("markdown", "# Hi\n").await;
        let tool = RenderTypst(client("http://127.0.0.1:1".into()));
        let err = tool.run(c, json!({"document_id": id})).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(ref m) if m.contains("typst")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn export_document_compiles_typst_natively_for_pdf() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/run"))
            .and(wiremock::matchers::body_string_contains(
                "typst compile input.typ",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "exit_code": 0, "stdout": "", "stderr": "",
                "artifacts": [], "duration_ms": 7, "timed_out": false
            })))
            .mount(&server)
            .await;
        let (c, id) = ctx_with_canvas_doc("typst", "= Export me\n").await;
        let tool = ExportDocument(client(server.uri()));
        let out = tool
            .run(c.clone(), json!({"document_id": id, "format": "pdf"}))
            .await
            .unwrap();
        assert_eq!(out["exit_code"], 0);

        // pptx export of typst is a clear error pointing at typ2pptx.
        let err = tool
            .run(c, json!({"document_id": id, "format": "pptx"}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs(ref m) if m.contains("typ2pptx")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn stage_documents_materializes_canvas_content() {
        let (c, id) = ctx_with_canvas_doc("typst", "= Staged\n").await;
        let mut files = Vec::new();
        let mut notes = Vec::new();
        let staged = stage_documents(
            &c,
            &[
                DocumentArg {
                    document_id: id.clone(),
                    version: None,
                    name: None,
                },
                DocumentArg {
                    document_id: "doc_missing".into(),
                    version: None,
                    name: None,
                },
            ],
            &mut files,
            &mut notes,
        )
        .await;
        // Real document → an input file named from title + format ext.
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "My_Deck.typ");
        assert_eq!(b64::decode(&files[0].content_b64).unwrap(), b"= Staged\n");
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0]["version"], 1);
        // Unknown id → a note, not a failed run.
        assert!(notes.iter().any(|n| n.contains("doc_missing")), "{notes:?}");
    }

    /// A canvas document named in `attachments` is staged, not ignored: the
    /// model shouldn't have to know which of two arguments an id belongs in,
    /// and "no attachment with that id" was a lie when the file was right
    /// there in the other store.
    #[tokio::test]
    async fn a_document_id_in_attachments_is_staged_as_a_document() {
        let (c, id) = ctx_with_canvas_doc("typst", "= From attachments\n").await;
        let staged = stage_attachments(
            &c,
            &[AttachmentArg {
                id: id.clone(),
                name: Some("deck.typ".into()),
            }],
        )
        .await
        .unwrap();
        // Handed back for the caller's `stage_documents` pass (S3 staging
        // can't materialise DB content), carrying the rename through.
        assert_eq!(staged.documents.len(), 1, "{:?}", staged.notes);
        assert_eq!(staged.documents[0].document_id, id);
        assert_eq!(staged.documents[0].name.as_deref(), Some("deck.typ"));
        // And it is NOT reported as an ignored attachment.
        assert!(
            !staged.notes.iter().any(|n| n.contains("Ignored")),
            "{:?}",
            staged.notes
        );

        // The same call end-to-end: the document lands in /work under the
        // requested name.
        let mut files = staged.files;
        let mut notes = staged.notes;
        let materialised = stage_documents(&c, &staged.documents, &mut files, &mut notes).await;
        assert_eq!(materialised.len(), 1);
        let deck = files.iter().find(|f| f.name == "deck.typ").unwrap();
        assert_eq!(
            b64::decode(&deck.content_b64).unwrap(),
            b"= From attachments\n"
        );
    }

    #[tokio::test]
    async fn stage_documents_noop_without_session() {
        let c = ctx().await;
        let mut files = Vec::new();
        let mut notes = Vec::new();
        let staged = stage_documents(
            &c,
            &[DocumentArg {
                document_id: "doc_x".into(),
                version: None,
                name: None,
            }],
            &mut files,
            &mut notes,
        )
        .await;
        assert!(staged.is_empty() && files.is_empty());
        assert!(notes[0].contains("can't be staged"), "{notes:?}");
    }

    // ---- SandboxLease (per-turn container reuse) -----------------------

    fn lease_req() -> RunRequest {
        RunRequest {
            language: Language::Python,
            code: "print(1)".into(),
            files: vec![],
            timeout_secs: None,
            network: false,
            container_id: None,
            keep_alive: false,
        }
    }

    /// A `/run` mock that echoes back a fixed `container_id`, plus a `DELETE
    /// /container/{id}` mock — the shape a keep-alive-capable runner presents.
    async fn lease_server(container_id: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/run"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "exit_code": 0, "stdout": "ok", "stderr": "", "artifacts": [],
                "duration_ms": 1, "timed_out": false, "output_truncated": false,
                "container_id": container_id,
            })))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("/container/{container_id}")))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        server
    }

    fn body_of(reqs: &[wiremock::Request], p: &str) -> Vec<Value> {
        reqs.iter()
            .filter(|r| r.url.path() == p)
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn lease_reuses_one_container_then_releases_it() {
        let server = lease_server("c1").await;
        let lease = SandboxLease::new(client(server.uri()));

        // First call: no container yet → sends container_id null, keep_alive
        // true; stores the echoed id.
        let r1 = lease.run(lease_req(), false).await.unwrap();
        assert_eq!(r1.container_id.as_deref(), Some("c1"));
        // Second call: reuses the stored id.
        lease.run(lease_req(), false).await.unwrap();
        // Turn end.
        lease.release().await;

        let reqs = server.received_requests().await.unwrap();
        let runs = body_of(&reqs, "/run");
        assert_eq!(runs.len(), 2, "two runner calls");
        assert!(
            runs[0].get("container_id").is_none_or(Value::is_null),
            "first call creates (no container_id): {:?}",
            runs[0]
        );
        assert_eq!(
            runs[0]["keep_alive"],
            json!(true),
            "always keep-alive in a turn"
        );
        assert_eq!(
            runs[1]["container_id"],
            json!("c1"),
            "second call reuses c1"
        );
        // Release issued exactly one DELETE for the leased container.
        let deletes = reqs
            .iter()
            .filter(|r| r.url.path() == "/container/c1")
            .count();
        assert_eq!(deletes, 1, "released once at turn end");
        // Releasing again is a no-op (no stored id).
        lease.release().await;
        let after = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/container/c1")
            .count();
        assert_eq!(after, 1, "second release is idempotent");
    }

    /// A `/run` responder that hands out a fresh incrementing container id
    /// each call — so a recreate (which sends `container_id: null`) gets a
    /// *different* id back, letting the test observe the old one being freed.
    struct SeqContainerIds(std::sync::atomic::AtomicUsize);
    impl wiremock::Respond for SeqContainerIds {
        fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            ResponseTemplate::new(200).set_body_json(json!({
                "exit_code": 0, "stdout": "ok", "stderr": "", "artifacts": [],
                "duration_ms": 1, "timed_out": false, "output_truncated": false,
                "container_id": format!("c{n}"),
            }))
        }
    }

    async fn seq_lease_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/run"))
            .respond_with(SeqContainerIds(std::sync::atomic::AtomicUsize::new(0)))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(wiremock::matchers::path_regex(r"^/container/.+"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        server
    }

    fn deleted_ids(reqs: &[wiremock::Request]) -> Vec<String> {
        reqs.iter()
            .filter(|r| r.method.as_str().eq_ignore_ascii_case("DELETE"))
            .map(|r| r.url.path().trim_start_matches("/container/").to_string())
            .collect()
    }

    #[tokio::test]
    async fn concurrent_calls_serialize_onto_one_container() {
        // Two run_in_sandbox jobs issued in the same round must share one
        // container (the mutex serializes them: first creates, second reuses)
        // rather than racing to create two — otherwise they'd write a shared
        // /work concurrently and the agent's snapshot-diff would misattribute
        // outputs. See the SandboxLease doc comment.
        let server = lease_server("c1").await;
        let lease = SandboxLease::new(client(server.uri()));
        let (a, b) = tokio::join!(lease.run(lease_req(), false), lease.run(lease_req(), false));
        a.unwrap();
        b.unwrap();

        let reqs = server.received_requests().await.unwrap();
        let runs = body_of(&reqs, "/run");
        assert_eq!(runs.len(), 2, "both jobs ran");
        let created = runs
            .iter()
            .filter(|b| b.get("container_id").is_none_or(Value::is_null))
            .count();
        let reused = runs
            .iter()
            .filter(|b| b["container_id"] == json!("c1"))
            .count();
        assert_eq!(created, 1, "exactly one container created: {runs:?}");
        assert_eq!(reused, 1, "the other reused it: {runs:?}");
    }

    #[tokio::test]
    async fn lease_fresh_and_network_change_recreate_the_container() {
        let server = seq_lease_server().await;
        let lease = SandboxLease::new(client(server.uri()));

        // Establish c1.
        lease.run(lease_req(), false).await.unwrap();
        // `fresh: true` recreates → c2, releasing c1.
        lease.run(lease_req(), true).await.unwrap();
        // A network-posture change also recreates → c3, releasing c2.
        let mut net = lease_req();
        net.network = true;
        lease.run(net, false).await.unwrap();
        // Turn end releases c3.
        lease.release().await;

        let reqs = server.received_requests().await.unwrap();
        let runs = body_of(&reqs, "/run");
        assert_eq!(runs.len(), 3);
        // Calls 2 and 3 both re-create (send no container_id).
        assert!(runs[1].get("container_id").is_none_or(Value::is_null));
        assert!(runs[2].get("container_id").is_none_or(Value::is_null));
        // Each recreate freed the prior container, and turn-end freed the last.
        let mut deleted = deleted_ids(&reqs);
        deleted.sort();
        assert_eq!(
            deleted,
            vec!["c1", "c2", "c3"],
            "every container freed once"
        );
    }

    #[tokio::test]
    async fn failed_recreate_preserves_the_current_container() {
        // Finding #3: a recreate that fails must NOT have already destroyed the
        // turn's container — /work has to survive so the model can retry.
        let server = MockServer::start().await;
        // First /run establishes c1; every later /run fails (e.g. egress
        // requested on a runner without it).
        Mock::given(method("POST"))
            .and(path("/run"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "exit_code": 0, "stdout": "ok", "stderr": "", "artifacts": [],
                "duration_ms": 1, "timed_out": false, "output_truncated": false,
                "container_id": "c1",
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/run"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "network egress requested but not configured",
            })))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(wiremock::matchers::path_regex(r"^/container/.+"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let lease = SandboxLease::new(client(server.uri()));
        lease.run(lease_req(), false).await.unwrap(); // establish c1
        // A network flip forces a recreate, which the runner rejects.
        let mut net = lease_req();
        net.network = true;
        assert!(lease.run(net, false).await.is_err(), "recreate fails");

        // c1 must NOT have been released by the failed recreate.
        let reqs = server.received_requests().await.unwrap();
        assert!(
            deleted_ids(&reqs).is_empty(),
            "old container must survive a failed recreate: {:?}",
            deleted_ids(&reqs)
        );
        // The lease still holds c1: releasing at turn end frees exactly it.
        lease.release().await;
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(
            deleted_ids(&reqs),
            vec!["c1"],
            "c1 still leased after failure"
        );
    }
}
