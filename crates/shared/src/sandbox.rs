// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Wire contract for the code-execution sandbox.
//!
//! The gateway's `run_in_sandbox` tool (and its specialized wrappers)
//! POST a [`RunRequest`] to the standalone `sandbox-runner` service,
//! which executes the code inside an ephemeral, single-use sandbox
//! and returns a [`RunResponse`]. Keeping the shapes here — in the I/O-
//! free `shared` crate — means the producer (gateway) and consumer
//! (runner) can never drift on the JSON format.
//!
//! File bytes ride the wire as standard base64 (RFC 4648, padded) in the
//! `*_b64` fields; both sides own a small codec rather than pulling a
//! `base64` dependency (see `gateway::server::chat_attachments` and
//! `sandbox_runner::b64`).

use serde::{Deserialize, Serialize};

/// Interpreter the sandbox runs the submitted `code` with. A single
/// generic tool plus a handful of specialized wrappers all funnel down
/// to "run this Python or this shell script", which covers document
/// generation, data analysis, CLI tools, and headless-browser work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Python,
    Bash,
}

impl Language {
    /// Stable wire token, for logs and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Python => "python",
            Language::Bash => "bash",
        }
    }
}

/// One file to materialize in the sandbox working directory (`/work`)
/// before the code runs. `name` is a single path segment — no `/`, no
/// `..` — enforced by the runner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InputFile {
    pub name: String,
    /// Standard base64 (padded) of the file's raw bytes.
    pub content_b64: String,
}

/// A code-execution request. `timeout_secs` and `network` are requests,
/// not guarantees: the runner clamps the timeout to its configured
/// maximum and only grants egress when it has an allowlist configured
/// and the caller is permitted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunRequest {
    pub language: Language,
    pub code: String,
    /// Files to drop into `/work` before running. Empty by default.
    #[serde(default)]
    pub files: Vec<InputFile>,
    /// Desired wall-clock budget. `None` → the runner's default.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Request network egress for this call. Default `false` (the
    /// sandbox runs with no network). Honored only against the runner's
    /// curated egress allowlist.
    #[serde(default)]
    pub network: bool,
}

/// A single file the run produced under `/work` (anything that wasn't an
/// input file is collected as an artifact).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Artifact {
    pub name: String,
    pub size: u64,
    /// Best-effort content type, sniffed from the extension by the runner.
    pub mime: String,
    /// Standard base64 (padded) of the file's raw bytes.
    pub content_b64: String,
}

/// Result of a completed run. A non-zero `exit_code` or a `timed_out`
/// flag is still a `200 OK` on the wire — the code ran, it just failed
/// or overran; only infrastructure problems (bad request, no capacity,
/// backend error) produce a non-2xx [`RunError`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    pub duration_ms: u64,
    pub timed_out: bool,
    /// True when stdout/stderr were clipped to the runner's output cap.
    #[serde(default)]
    pub output_truncated: bool,
}

/// Error envelope for a non-2xx runner response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunError {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Language::Python).unwrap(),
            "\"python\""
        );
        assert_eq!(serde_json::to_string(&Language::Bash).unwrap(), "\"bash\"");
    }

    #[test]
    fn run_request_defaults_are_lenient() {
        // A minimal request from a hand-written client must deserialize:
        // files/timeout/network all default.
        let req: RunRequest =
            serde_json::from_str(r#"{"language":"python","code":"print(1)"}"#).unwrap();
        assert_eq!(req.language, Language::Python);
        assert!(req.files.is_empty());
        assert_eq!(req.timeout_secs, None);
        assert!(!req.network);
    }

    #[test]
    fn run_response_round_trips() {
        let resp = RunResponse {
            exit_code: 0,
            stdout: "hi".into(),
            stderr: String::new(),
            artifacts: vec![Artifact {
                name: "slides.pptx".into(),
                size: 3,
                mime: "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                    .into(),
                content_b64: "AAAA".into(),
            }],
            duration_ms: 42,
            timed_out: false,
            output_truncated: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: RunResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, back);
    }
}
