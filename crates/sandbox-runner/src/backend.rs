// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Container backend: the thin seam between the pool/HTTP logic and
//! `podman`. The real [`PodmanBackend`] shells out to drive single-use,
//! gVisor-isolated containers; the test-only `FakeBackend` lets the pool
//! logic be unit-tested without a container runtime present.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use shared::sandbox::{RunRequest, RunResponse};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::config::Config;

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("spawning podman failed: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("podman {op} exited {code}: {stderr}")]
    Command {
        op: &'static str,
        code: String,
        stderr: String,
    },
    #[error("sandbox-agent protocol error: {0}")]
    Protocol(String),
}

/// Network posture a sandbox container is created with. Pooled (warm)
/// containers are always [`Network::None`]; a call that requests and is
/// granted egress gets a fresh [`Network::Egress`] container instead, so
/// the default-deny pool is never reused for a networked job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    None,
    Egress,
}

/// Abstracts "boot a sandbox", "run a job in it", "destroy it". Object-
/// safe via `async_trait` so the pool can hold `Arc<dyn ContainerBackend>`
/// and tests can swap in a fake.
#[async_trait::async_trait]
pub trait ContainerBackend: Send + Sync + 'static {
    /// Boot one fresh, idle sandbox container and return its id.
    async fn create(&self, network: Network) -> Result<String, BackendError>;

    /// Run one job inside an existing container. `timeout` is the hard
    /// wall-clock stop; on overrun the returned response has
    /// `timed_out = true` (the caller destroys the container regardless).
    async fn exec(
        &self,
        id: &str,
        req: &RunRequest,
        timeout: Duration,
    ) -> Result<RunResponse, BackendError>;

    /// Tear a container down. Best-effort: failures are logged, not
    /// surfaced — a leaked container is a monitoring concern, not a
    /// request error.
    async fn destroy(&self, id: &str);
}

/// Drives `podman` to run each job under the configured OCI runtime
/// (`runsc` by default). Every container is locked down: read-only rootfs,
/// all capabilities dropped, no-new-privileges, tmpfs `/work`, resource
/// caps, and no network unless [`Network::Egress`] is requested.
pub struct PodmanBackend {
    cfg: Arc<Config>,
}

impl PodmanBackend {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }

    /// Hardening + lifecycle flags shared by every `podman run`.
    fn run_args(&self, network: Network) -> Vec<String> {
        let c = &self.cfg;
        let mut a: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--runtime".into(),
            c.runtime.clone(),
            "--read-only".into(),
            // Writable scratch for the job; rootfs stays read-only. `/work`
            // is the job's CWD; `/tmp` is exec-mounted because chromium and
            // LibreOffice drop helper binaries there.
            "--tmpfs".into(),
            format!("/work:rw,size={},mode=1777", c.work_size),
            "--tmpfs".into(),
            format!("/tmp:rw,exec,size={},mode=1777", c.tmp_size),
            "--cap-drop=ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--memory".into(),
            c.memory.clone(),
            // Pin memory+swap to the same value so the guest can't use swap to
            // exceed --memory. Without this podman defaults --memory-swap to
            // 2x --memory, doubling the effective cap and letting a memory bomb
            // run far past it. Equal values = swap disabled, hard cap at --memory.
            "--memory-swap".into(),
            c.memory.clone(),
            "--cpus".into(),
            c.cpus.clone(),
            "--pids-limit".into(),
            c.pids_limit.to_string(),
            // Make a runaway sandbox the first thing the host OOM-killer reaps.
            // Even with the cgroup cap above, gVisor's sentry holds host memory
            // proportional to guest use; under host pressure this ensures the
            // sandbox dies, not the runner (which sets OOMScoreAdjust=-800).
            "--oom-score-adj".into(),
            "1000".into(),
            "--label".into(),
            "app=llm-gateway-sandbox".into(),
        ];
        match network {
            Network::None => {
                a.push("--network".into());
                a.push("none".into());
            }
            Network::Egress => {
                a.push("--network".into());
                a.push(c.egress_network.clone());
                if !c.egress_proxy.is_empty() {
                    for var in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                        a.push("--env".into());
                        a.push(format!("{var}={}", c.egress_proxy));
                    }
                }
            }
        }
        a.push(c.image.clone());
        // Keep the container alive and idle until we `exec` a job into it.
        a.push("sleep".into());
        a.push("infinity".into());
        a
    }
}

#[async_trait::async_trait]
impl ContainerBackend for PodmanBackend {
    async fn create(&self, network: Network) -> Result<String, BackendError> {
        let args = self.run_args(network);
        let out = tokio::process::Command::new(&self.cfg.podman)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .await?;
        if !out.status.success() {
            return Err(BackendError::Command {
                op: "run",
                code: out.status.code().map(|c| c.to_string()).unwrap_or_default(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if id.is_empty() {
            return Err(BackendError::Protocol("podman run printed no id".into()));
        }
        Ok(id)
    }

    async fn exec(
        &self,
        id: &str,
        req: &RunRequest,
        timeout: Duration,
    ) -> Result<RunResponse, BackendError> {
        // The job marshalling lives inside the image: pipe the RunRequest to
        // `sandbox-agent` on stdin, read a RunResponse back on stdout.
        let child = tokio::process::Command::new(&self.cfg.podman)
            .arg("exec")
            .arg("-i")
            .arg(id)
            .arg("/usr/local/bin/sandbox-agent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        drive_agent(child, req, timeout).await
    }

    async fn destroy(&self, id: &str) {
        // `rm -f` (no `-t`) so the same command works under both podman and
        // docker — handy for dev (SANDBOX_PODMAN=docker on macOS).
        let res = tokio::process::Command::new(&self.cfg.podman)
            .args(["rm", "-f", id])
            .stdin(Stdio::null())
            .output()
            .await;
        match res {
            Ok(o) if !o.status.success() => tracing::warn!(
                container = id,
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "podman rm failed; container may be leaked"
            ),
            Err(e) => tracing::warn!(container = id, error = %e, "podman rm could not run"),
            _ => {}
        }
    }
}

/// Pipe a job to a spawned `sandbox-agent` process, enforce the wall-clock
/// timeout, and decode its RunResponse. Shared by every backend so the
/// agent protocol lives in exactly one place.
async fn drive_agent(
    mut child: tokio::process::Child,
    req: &RunRequest,
    timeout: Duration,
) -> Result<RunResponse, BackendError> {
    let job =
        serde_json::to_vec(req).map_err(|e| BackendError::Protocol(format!("encode job: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A write error usually means the agent already exited; fall through
        // and let wait_with_output surface its stderr.
        let _ = stdin.write_all(&job).await;
        let _ = stdin.shutdown().await;
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            if !out.status.success() {
                return Err(BackendError::Command {
                    op: "exec",
                    code: out.status.code().map(|c| c.to_string()).unwrap_or_default(),
                    stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
                });
            }
            serde_json::from_slice::<RunResponse>(&out.stdout).map_err(|e| {
                BackendError::Protocol(format!(
                    "agent output not a RunResponse: {e}; stderr={}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            })
        }
        // Outer timeout: report it; the caller destroys the sandbox, which
        // kills the in-flight process.
        Ok(Err(e)) => Err(BackendError::Spawn(e)),
        Err(_elapsed) => Ok(RunResponse {
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("sandbox timed out after {}s", timeout.as_secs()),
            artifacts: Vec::new(),
            duration_ms: timeout.as_millis() as u64,
            timed_out: true,
            output_truncated: false,
        }),
    }
}

#[cfg(test)]
mod podman_args_tests {
    use super::*;

    fn cfg() -> Arc<Config> {
        Arc::new(Config {
            bind: "127.0.0.1:9000".into(),
            image: "img".into(),
            runtime: "runsc".into(),
            podman: "podman".into(),
            pool_size: 3,
            max_concurrent: 6,
            default_timeout_secs: 60,
            max_timeout_secs: 300,
            memory: "1024m".into(),
            cpus: "2".into(),
            pids_limit: 256,
            work_size: "512m".into(),
            tmp_size: "512m".into(),
            max_output_bytes: 131_072,
            egress_network: String::new(),
            egress_proxy: String::new(),
        })
    }

    fn has_pair(args: &[String], flag: &str, val: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == val)
    }

    /// Pins the host-cgroup resource caps so a refactor can't silently drop the
    /// DoS-hardening flags (memory bomb regression: an unbounded guest crashed
    /// the runner before these were enforced).
    #[test]
    fn run_args_enforce_resource_caps() {
        let a = PodmanBackend::new(cfg()).run_args(Network::None);
        assert!(has_pair(&a, "--memory", "1024m"));
        // swap pinned to memory → guest can't escape the cap via swap
        assert!(has_pair(&a, "--memory-swap", "1024m"));
        assert!(has_pair(&a, "--cpus", "2"));
        assert!(has_pair(&a, "--pids-limit", "256"));
        // a memory bomb is reaped before the runner (which is OOMScoreAdjust=-800)
        assert!(has_pair(&a, "--oom-score-adj", "1000"));
        // core lockdown still in place
        assert!(a.iter().any(|s| s == "--read-only"));
        assert!(a.iter().any(|s| s == "--cap-drop=ALL"));
        assert!(has_pair(&a, "--network", "none"));
        // scratch tmpfs sizes come from config (operator-tunable for large jobs)
        assert!(a.iter().any(|s| s == "/work:rw,size=512m,mode=1777"));
        assert!(a.iter().any(|s| s == "/tmp:rw,exec,size=512m,mode=1777"));
    }
}

/// The in-image agent source, embedded so the [`LocalBackend`] runs the
/// exact same marshaller as the container image (single source of truth).
const AGENT_SRC: &str = include_str!("../../../sandbox-image/sandbox-agent");

/// **DEV-ONLY, NO ISOLATION.** Runs the agent directly on the host (a temp
/// dir per job), so the full HTTP→runner→agent path is exercisable on a
/// machine without podman (e.g. macOS). Code runs with the runner's
/// own privileges — never select this in production. Activated by
/// `SANDBOX_RUNTIME=local-unsafe`.
pub struct LocalBackend {
    agent: std::path::PathBuf,
}

/// Process-wide counter for unique agent + workdir paths, so two
/// `LocalBackend` instances in one process (e.g. parallel tests) never
/// collide on the same file or working directory.
static LOCAL_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn local_seq() -> usize {
    LOCAL_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

impl LocalBackend {
    pub fn new() -> std::io::Result<Self> {
        let base = std::env::temp_dir().join("llm-sandbox-local");
        std::fs::create_dir_all(&base)?;
        let agent = base.join(format!(
            "sandbox-agent-{}-{}.py",
            std::process::id(),
            local_seq()
        ));
        std::fs::write(&agent, AGENT_SRC)?;
        Ok(Self { agent })
    }
}

#[async_trait::async_trait]
impl ContainerBackend for LocalBackend {
    async fn create(&self, _network: Network) -> Result<String, BackendError> {
        let dir = std::env::temp_dir().join("llm-sandbox-local").join(format!(
            "work-{}-{}",
            std::process::id(),
            local_seq()
        ));
        std::fs::create_dir_all(&dir)?;
        Ok(dir.to_string_lossy().into_owned())
    }

    async fn exec(
        &self,
        id: &str,
        req: &RunRequest,
        timeout: Duration,
    ) -> Result<RunResponse, BackendError> {
        let child = tokio::process::Command::new("python3")
            .arg(&self.agent)
            .env("SANDBOX_AGENT_WORK", id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        drive_agent(child, req, timeout).await
    }

    async fn destroy(&self, id: &str) {
        let _ = tokio::fs::remove_dir_all(id).await;
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! In-memory backend for the pool unit tests. Records create/destroy
    //! so tests can assert warm-pool refill and single-use teardown.

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use shared::sandbox::Language;

    #[derive(Default)]
    pub struct FakeBackend {
        next: AtomicUsize,
        pub created: Mutex<Vec<(String, Network)>>,
        pub destroyed: Mutex<Vec<String>>,
        pub execs: AtomicUsize,
    }

    impl FakeBackend {
        pub fn new() -> Self {
            Self::default()
        }
        pub fn live_count(&self) -> usize {
            self.created.lock().unwrap().len() - self.destroyed.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl ContainerBackend for FakeBackend {
        async fn create(&self, network: Network) -> Result<String, BackendError> {
            let n = self.next.fetch_add(1, Ordering::SeqCst);
            let id = format!("fake-{n}");
            self.created.lock().unwrap().push((id.clone(), network));
            Ok(id)
        }

        async fn exec(
            &self,
            id: &str,
            req: &RunRequest,
            _timeout: Duration,
        ) -> Result<RunResponse, BackendError> {
            self.execs.fetch_add(1, Ordering::SeqCst);
            Ok(RunResponse {
                exit_code: 0,
                stdout: format!("ran {} in {id}", req.language.as_str()),
                stderr: String::new(),
                artifacts: Vec::new(),
                duration_ms: 1,
                timed_out: false,
                output_truncated: false,
            })
        }

        async fn destroy(&self, id: &str) {
            self.destroyed.lock().unwrap().push(id.to_string());
        }
    }

    pub fn req() -> RunRequest {
        RunRequest {
            language: Language::Python,
            code: "print(1)".into(),
            files: Vec::new(),
            timeout_secs: None,
            network: false,
        }
    }
}

#[cfg(test)]
mod local_tests {
    //! End-to-end test of the dev `LocalBackend` against a real `python3`.
    //! Exercises the full agent contract (file inputs, stdout, artifact
    //! collection) on any host with python3 — notably macOS, where
    //! podman / gVisor aren't available.

    use super::*;
    use shared::sandbox::{InputFile, Language, RunRequest};

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn runs_python_collects_artifact_and_reads_input() {
        if !python3_available() {
            eprintln!("skipping local_backend test: python3 not on PATH");
            return;
        }
        let be = LocalBackend::new().unwrap();
        let id = be.create(Network::None).await.unwrap();
        let req = RunRequest {
            language: Language::Python,
            // Reads an input file, writes an output artifact, prints to stdout.
            code: "print('in=' + open('data.txt').read()); open('out.txt','w').write('hi')".into(),
            files: vec![InputFile {
                name: "data.txt".into(),
                content_b64: "Zm9v".into(), // "foo"
            }],
            timeout_secs: None,
            network: false,
        };
        let resp = be.exec(&id, &req, Duration::from_secs(30)).await.unwrap();
        be.destroy(&id).await;

        assert_eq!(resp.exit_code, 0, "stderr={}", resp.stderr);
        assert!(resp.stdout.contains("in=foo"), "stdout={}", resp.stdout);
        let art = resp
            .artifacts
            .iter()
            .find(|a| a.name == "out.txt")
            .expect("out.txt artifact");
        assert_eq!(art.content_b64, "aGk="); // "hi"
        // The input file must NOT be reported as a produced artifact.
        assert!(!resp.artifacts.iter().any(|a| a.name == "data.txt"));
    }

    #[tokio::test]
    async fn large_stdout_is_preserved_as_an_attachment() {
        if !python3_available() {
            eprintln!("skipping local_backend test: python3 not on PATH");
            return;
        }
        let be = LocalBackend::new().unwrap();
        let id = be.create(Network::None).await.unwrap();
        // Print ~300 KB (> the agent's 128 KB preserve threshold).
        let req = RunRequest {
            language: Language::Python,
            code: "print('X' * 300000)".into(),
            files: vec![],
            timeout_secs: None,
            network: false,
        };
        let resp = be.exec(&id, &req, Duration::from_secs(30)).await.unwrap();
        be.destroy(&id).await;
        let art = resp.artifacts.iter().find(|a| a.name == "stdout.txt");
        assert!(
            art.is_some(),
            "no stdout.txt; exit={} stdout_len={} timed_out={} artifacts={:?} stderr={:.200}",
            resp.exit_code,
            resp.stdout.len(),
            resp.timed_out,
            resp.artifacts
                .iter()
                .map(|a| (a.name.clone(), a.size))
                .collect::<Vec<_>>(),
            resp.stderr,
        );
        assert!(art.unwrap().size >= 300_000, "preserved full stream");
    }
}
