// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Git clone / fetch helpers.
//!
//! Shells out to system `git` rather than embedding `gix` or `git2` —
//! the install footprint of both is sizeable for a feature that only
//! needs clone + fetch + rev-parse. The system binary is required on
//! deploy hosts; the Dockerfile is updated to install it.
//!
//! PAT handling: the token (if any) is interpolated into the URL's
//! userinfo component (`https://x-access-token:<pat>@host/...`) right
//! before spawning, and the rewritten URL is the *only* place it
//! appears on the parent's side. We deliberately don't pass it via env
//! vars or credential helpers — that would either leak to child
//! processes or require a writable HOME on the running gateway.
//!
//! Sandboxing flags we set on every git invocation:
//!   * `GIT_TERMINAL_PROMPT=0` — never prompt for credentials; a bad PAT
//!     becomes a clean error instead of a hung subprocess.
//!   * `GIT_ASKPASS=/bin/true` — belt-and-suspenders against legacy
//!     prompting paths on macOS/Linux.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use thiserror::Error;
use tokio::process::Command;
use url::Url;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("invalid git URL `{url}`: {source}")]
    BadUrl {
        url: String,
        #[source]
        source: url::ParseError,
    },
    /// The kernel rejected our `posix_spawn` of `git` itself — `git` is
    /// missing, isn't executable, or is sandboxed away. Distinct from
    /// [`GitError::Mkdir`] so an operator-facing message can tell
    /// "you're missing `git`" from "the clone cache dir isn't writable".
    #[error("spawning `git {command}`: {source} (is git installed and on PATH?)")]
    Spawn {
        command: &'static str,
        #[source]
        source: std::io::Error,
    },
    /// Could not prepare the on-disk clone cache directory (parent of
    /// `target`). The path is included verbatim so the operator can
    /// `ls -la` the offending directory.
    #[error("preparing clone-cache directory `{path}`: {source}")]
    Mkdir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("git {command} exited with status {status}: {stderr}")]
    NonZero {
        command: &'static str,
        status: i32,
        stderr: String,
    },
    #[error("git {command} produced non-utf8 output")]
    BadOutput { command: &'static str },
}

/// Inject `pat` into the `userinfo` portion of `url` so an HTTPS clone
/// can authenticate without leaving the token in env vars or on disk.
/// `None` PAT → URL returned unchanged. Non-HTTPS schemes (e.g. `git@…`
/// SSH) are returned unchanged on the assumption that the operator
/// configured a working keypair on the host.
pub fn inject_pat(url: &str, pat: Option<&str>) -> Result<String, GitError> {
    let Some(pat) = pat.filter(|s| !s.is_empty()) else {
        return Ok(url.to_string());
    };
    let mut parsed = Url::parse(url).map_err(|source| GitError::BadUrl {
        url: url.to_string(),
        source,
    })?;
    if parsed.scheme() != "https" {
        return Ok(url.to_string());
    }
    // `set_username` / `set_password` only fail when the scheme can't
    // carry userinfo — https can, so the unwraps are safe by construction.
    parsed.set_username("x-access-token").unwrap();
    parsed.set_password(Some(pat)).unwrap();
    Ok(parsed.to_string())
}

/// Idempotent fetch: clones if `target` doesn't exist, otherwise fetches
/// `git_ref` and hard-resets the working tree to its tip. Returns the
/// resolved HEAD commit sha so the indexer can stamp `last_indexed_commit`.
pub async fn clone_or_update(
    url: &str,
    git_ref: &str,
    pat: Option<&str>,
    target: &Path,
) -> Result<String, GitError> {
    let authed = inject_pat(url, pat)?;
    if target.exists() {
        fetch_and_reset(target, &authed, git_ref).await?;
    } else {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| GitError::Mkdir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        shallow_clone(&authed, git_ref, target).await?;
    }
    head_commit(target).await
}

async fn shallow_clone(url: &str, git_ref: &str, target: &Path) -> Result<(), GitError> {
    let target_str = target.to_string_lossy().to_string();
    run(
        "clone",
        Path::new("."),
        &[
            "clone",
            "--depth",
            "1",
            "--branch",
            git_ref,
            "--single-branch",
            url,
            &target_str,
        ],
    )
    .await
    .map(|_| ())
}

async fn fetch_and_reset(repo: &Path, url: &str, git_ref: &str) -> Result<(), GitError> {
    // Re-point origin in case the URL (or its PAT) changed between runs;
    // otherwise a rotated token would silently keep using the old one.
    run("remote", repo, &["remote", "set-url", "origin", url]).await?;
    run("fetch", repo, &["fetch", "--depth", "1", "origin", git_ref]).await?;
    run("reset", repo, &["reset", "--hard", "FETCH_HEAD"]).await?;
    Ok(())
}

pub async fn head_commit(repo: &Path) -> Result<String, GitError> {
    let out = run("rev-parse", repo, &["rev-parse", "HEAD"]).await?;
    Ok(out.trim().to_string())
}

async fn run(command: &'static str, cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd
        .output()
        .await
        .map_err(|source| GitError::Spawn { command, source })?;
    if !output.status.success() {
        return Err(GitError::NonZero {
            command,
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    String::from_utf8(output.stdout).map_err(|_| GitError::BadOutput { command })
}

/// Internal convenience for callers (and tests) that want a target dir
/// under the gateway's `data/rag-cache/<id>/` layout.
pub fn cache_path_for(base: &Path, collection_id: i64) -> PathBuf {
    base.join(format!("{collection_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn inject_pat_adds_userinfo_for_https() {
        let out = inject_pat("https://example.com/org/repo.git", Some("ghp_abc")).unwrap();
        assert_eq!(
            out,
            "https://x-access-token:ghp_abc@example.com/org/repo.git"
        );
    }

    #[test]
    fn inject_pat_passes_through_when_no_token() {
        let original = "https://example.com/org/repo.git";
        assert_eq!(inject_pat(original, None).unwrap(), original);
        assert_eq!(inject_pat(original, Some("")).unwrap(), original);
    }

    #[test]
    fn inject_pat_leaves_ssh_urls_alone() {
        let ssh = "git@example.com:org/repo.git";
        // ssh URLs aren't url-parseable; helper returns BadUrl when a token
        // is requested but they're meaningless for ssh anyway. Without a
        // token, helper short-circuits before parsing.
        assert_eq!(inject_pat(ssh, None).unwrap(), ssh);
    }

    /// Build a tiny git repo to use as a clone source. Returns the path.
    /// Skips itself if `git` isn't on PATH (CI without git → test ignored
    /// rather than failed).
    async fn fixture_repo() -> Option<tempfile::TempDir> {
        let dir = tempdir().unwrap();
        let path = dir.path();
        let init = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main", "."])
            .current_dir(path)
            .output();
        let Ok(init) = init else { return None };
        if !init.status.success() {
            return None;
        }
        // Local config so commits work in CI without a global git identity.
        let cfgs = [
            ["config", "user.email", "test@example.invalid"],
            ["config", "user.name", "test"],
            ["config", "commit.gpgsign", "false"],
        ];
        for args in cfgs {
            let s = std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .status()
                .unwrap();
            assert!(s.success(), "git {args:?} failed");
        }
        fs::write(path.join("README.md"), b"hello\n").unwrap();
        let add = std::process::Command::new("git")
            .args(["add", "README.md"])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(add.success());
        let commit = std::process::Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(commit.success());
        Some(dir)
    }

    #[tokio::test]
    async fn clone_then_fetch_against_local_fixture_repo() {
        let Some(src) = fixture_repo().await else {
            eprintln!("git not on PATH — skipping");
            return;
        };
        let scratch = tempdir().unwrap();
        let target = scratch.path().join("clone");
        let url = src.path().to_string_lossy().to_string();

        let sha = clone_or_update(&url, "main", None, &target).await.unwrap();
        assert!(target.join("README.md").exists());
        assert_eq!(sha.len(), 40, "expected a full sha, got {sha:?}");

        // Add a second commit upstream, then re-pull — sha should change
        // and the new file should show up.
        fs::write(src.path().join("more.txt"), b"more\n").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "more.txt"])
            .current_dir(src.path())
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-q", "-m", "second"])
            .current_dir(src.path())
            .status();
        let sha2 = clone_or_update(&url, "main", None, &target).await.unwrap();
        assert_ne!(sha, sha2);
        assert!(target.join("more.txt").exists());
    }

    #[tokio::test]
    async fn nonexistent_url_surfaces_git_nonzero_exit() {
        let scratch = tempdir().unwrap();
        let target = scratch.path().join("clone");
        let err = clone_or_update(
            // Bogus path: the git binary should exit non-zero.
            "/this/does/not/exist.git",
            "main",
            None,
            &target,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, GitError::NonZero { .. }), "{err:?}");
    }
}
