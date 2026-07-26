// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Minimal GitHub REST client for the feedback widget.
//!
//! Files a user's feedback as a labelled issue, optionally embedding a
//! viewport screenshot. The screenshot is committed as a file on a dedicated
//! assets branch (created off the repo's default branch if missing) and
//! linked into the issue body via its raw URL — the same approach the
//! `yachtlistings2` / `croit.erp` widgets use, ported here.
//!
//! Only the handful of endpoints the feature needs are wrapped; everything
//! goes through `reqwest` (the shared `AppState.http` client) with the three
//! headers GitHub requires (`User-Agent`, `Accept`, `X-GitHub-Api-Version`).

use serde_json::json;

use crate::server::config::FeedbackConfig;

/// What the caller wants filed. Text fields are already validated/trimmed by
/// the handler; `screenshot_png_base64` is raw standard-base64 PNG bytes
/// (no `data:` prefix).
pub struct IssueInput {
    pub title: String,
    pub description: String,
    pub business_value: String,
    pub acceptance_criteria: String,
    /// One of `low` / `medium` / `high`.
    pub priority: String,
    /// Email of the signed-in user who filed it (for attribution in the body).
    pub reporter_email: String,
    pub screenshot_png_base64: Option<String>,
    /// Free-form diagnostics (url, viewport, user agent, …) rendered as a
    /// collapsible table. A JSON object; non-object values are ignored.
    pub system_info: serde_json::Value,
}

/// The created issue, surfaced back to the browser so it can toast a link.
pub struct IssueResult {
    pub number: u64,
    pub url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    #[error("feedback is not configured")]
    NotConfigured,
    #[error("github request failed: {0}")]
    Transport(String),
    #[error("github api error ({status}): {body}")]
    Api { status: u16, body: String },
}

const ACCEPT: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "croit-llm-gateway-feedback";

/// File a feedback issue. Uploads the screenshot first (best-effort — a
/// screenshot upload failure degrades to an issue without the image rather
/// than failing the whole submission), then creates the issue.
pub async fn create_feedback_issue(
    http: &reqwest::Client,
    cfg: &FeedbackConfig,
    input: IssueInput,
) -> Result<IssueResult, GithubError> {
    if !cfg.is_configured() {
        return Err(GithubError::NotConfigured);
    }
    let token = cfg.github_token().ok_or(GithubError::NotConfigured)?;

    // Screenshot → committed asset → raw URL. Non-fatal: if any step fails we
    // log and proceed without the image.
    let screenshot_url = match input.screenshot_png_base64.as_deref() {
        Some(b64) if !b64.is_empty() => match upload_screenshot(http, cfg, &token, b64).await {
            Ok(url) => Some(url),
            Err(err) => {
                tracing::warn!(error = %err, "feedback screenshot upload failed; filing without it");
                None
            }
        },
        _ => None,
    };

    let body = build_issue_body(&input, screenshot_url.as_deref());
    let mut labels = cfg.labels.clone();
    labels.push(format!("priority:{}", normalise_priority(&input.priority)));

    let url = format!(
        "{}/repos/{}/{}/issues",
        cfg.github_api_base.trim_end_matches('/'),
        cfg.github_owner,
        cfg.github_repo,
    );
    let resp = http
        .post(&url)
        .bearer_auth(&token)
        .header("Accept", ACCEPT)
        .header("X-GitHub-Api-Version", API_VERSION)
        .header("User-Agent", USER_AGENT)
        .json(&json!({
            "title": issue_title(&input.title),
            "body": body,
            "labels": labels,
        }))
        .send()
        .await
        .map_err(|e| GithubError::Transport(e.to_string()))?;

    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GithubError::Transport(e.to_string()))?;
    if !status.is_success() {
        return Err(GithubError::Api {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).chars().take(400).collect(),
        });
    }
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| GithubError::Transport(e.to_string()))?;
    let number = v.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
    let html_url = v
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    Ok(IssueResult {
        number,
        url: html_url,
    })
}

/// Commit the screenshot to the assets branch and return a raw URL that
/// renders inline in the issue body.
async fn upload_screenshot(
    http: &reqwest::Client,
    cfg: &FeedbackConfig,
    token: &str,
    png_base64: &str,
) -> Result<String, GithubError> {
    // Cheap sanity check before spending two round trips. We don't decode
    // (the gateway has no base64 dependency and GitHub validates the payload
    // anyway) — just reject obvious garbage and oversize blobs. ~14MB of
    // base64 ≈ ~10MB PNG, far above any real viewport screenshot.
    const MAX_B64_LEN: usize = 14 * 1024 * 1024;
    if png_base64.len() > MAX_B64_LEN {
        return Err(GithubError::Transport("screenshot too large".into()));
    }
    if !png_base64
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    {
        return Err(GithubError::Transport(
            "screenshot is not valid base64".into(),
        ));
    }

    ensure_assets_branch(http, cfg, token).await?;

    // A stable, collision-free path. `uuid::simple` keeps it filesystem- and
    // URL-clean.
    let path = format!("screenshots/{}.png", uuid::Uuid::new_v4().simple());
    let url = format!(
        "{}/repos/{}/{}/contents/{}",
        cfg.github_api_base.trim_end_matches('/'),
        cfg.github_owner,
        cfg.github_repo,
        path,
    );
    let resp = http
        .put(&url)
        .bearer_auth(token)
        .header("Accept", ACCEPT)
        .header("X-GitHub-Api-Version", API_VERSION)
        .header("User-Agent", USER_AGENT)
        .json(&json!({
            "message": "feedback: add screenshot",
            "content": png_base64,
            "branch": cfg.assets_branch,
        }))
        .send()
        .await
        .map_err(|e| GithubError::Transport(e.to_string()))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GithubError::Transport(e.to_string()))?;
    if !status.is_success() {
        return Err(GithubError::Api {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).chars().take(400).collect(),
        });
    }
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| GithubError::Transport(e.to_string()))?;
    // Prefer the canonical download URL GitHub hands back; fall back to the
    // web `/raw/` form (renders inline for viewers with repo access).
    let download = v
        .pointer("/content/download_url")
        .and_then(|u| u.as_str())
        .map(str::to_string);
    Ok(download.unwrap_or_else(|| {
        format!(
            "{}/{}/{}/raw/{}/{}",
            web_base(&cfg.github_api_base),
            cfg.github_owner,
            cfg.github_repo,
            cfg.assets_branch,
            path,
        )
    }))
}

/// Create the assets branch off the repo's default branch if it doesn't
/// exist yet. Idempotent: an existing branch (or a lost race that 422s on
/// "Reference already exists") is treated as success.
async fn ensure_assets_branch(
    http: &reqwest::Client,
    cfg: &FeedbackConfig,
    token: &str,
) -> Result<(), GithubError> {
    let api = cfg.github_api_base.trim_end_matches('/');
    let (owner, repo) = (&cfg.github_owner, &cfg.github_repo);

    // Already there?
    let head_url = format!(
        "{api}/repos/{owner}/{repo}/git/ref/heads/{}",
        cfg.assets_branch
    );
    let head = http
        .get(&head_url)
        .bearer_auth(token)
        .header("Accept", ACCEPT)
        .header("X-GitHub-Api-Version", API_VERSION)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| GithubError::Transport(e.to_string()))?;
    if head.status().is_success() {
        return Ok(());
    }

    // Default branch name → its head sha → create the new ref off it.
    let repo_url = format!("{api}/repos/{owner}/{repo}");
    let repo_v = get_json(http, &repo_url, token).await?;
    let default_branch = repo_v
        .get("default_branch")
        .and_then(|b| b.as_str())
        .unwrap_or("main");

    let base_ref_url = format!("{api}/repos/{owner}/{repo}/git/ref/heads/{default_branch}");
    let base_v = get_json(http, &base_ref_url, token).await?;
    let sha = base_v
        .pointer("/object/sha")
        .and_then(|s| s.as_str())
        .ok_or_else(|| GithubError::Transport("default branch has no head sha".into()))?
        .to_string();

    let create_url = format!("{api}/repos/{owner}/{repo}/git/refs");
    let resp = http
        .post(&create_url)
        .bearer_auth(token)
        .header("Accept", ACCEPT)
        .header("X-GitHub-Api-Version", API_VERSION)
        .header("User-Agent", USER_AGENT)
        .json(&json!({
            "ref": format!("refs/heads/{}", cfg.assets_branch),
            "sha": sha,
        }))
        .send()
        .await
        .map_err(|e| GithubError::Transport(e.to_string()))?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 422 {
        // 422 == "Reference already exists" (a concurrent first submit won).
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(GithubError::Api {
        status: status.as_u16(),
        body: body.chars().take(400).collect(),
    })
}

async fn get_json(
    http: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<serde_json::Value, GithubError> {
    let resp = http
        .get(url)
        .bearer_auth(token)
        .header("Accept", ACCEPT)
        .header("X-GitHub-Api-Version", API_VERSION)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| GithubError::Transport(e.to_string()))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GithubError::Transport(e.to_string()))?;
    if !status.is_success() {
        return Err(GithubError::Api {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).chars().take(400).collect(),
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| GithubError::Transport(e.to_string()))
}

/// Derive the web host from the API base. `api.github.com` → `github.com`;
/// an Enterprise `https://host/api/v3` → `https://host`.
fn web_base(api_base: &str) -> String {
    let trimmed = api_base.trim_end_matches('/');
    if trimmed == "https://api.github.com" {
        return "https://github.com".to_string();
    }
    trimmed
        .strip_suffix("/api/v3")
        .unwrap_or(trimmed)
        .to_string()
}

fn normalise_priority(p: &str) -> &str {
    match p.trim().to_ascii_lowercase().as_str() {
        "low" => "low",
        "high" => "high",
        _ => "medium",
    }
}

/// GitHub issue titles cap at 256 chars; keep margin.
fn issue_title(title: &str) -> String {
    let t = title.trim();
    if t.chars().count() <= 200 {
        t.to_string()
    } else {
        t.chars().take(200).collect()
    }
}

/// Build the issue body markdown. Mirrors the source widgets' layout:
/// description, business value, acceptance criteria, screenshot, then a
/// collapsible system-info table.
fn build_issue_body(input: &IssueInput, screenshot_url: Option<&str>) -> String {
    let mut out = String::new();

    if !input.description.trim().is_empty() {
        out.push_str(input.description.trim());
        out.push_str("\n\n");
    }
    if !input.business_value.trim().is_empty() {
        out.push_str("## Why / Business value\n\n");
        out.push_str(input.business_value.trim());
        out.push_str("\n\n");
    }
    if !input.acceptance_criteria.trim().is_empty() {
        out.push_str("## Acceptance criteria\n\n");
        out.push_str(input.acceptance_criteria.trim());
        out.push_str("\n\n");
    }
    if let Some(url) = screenshot_url {
        out.push_str("## Screenshot\n\n");
        out.push_str(&format!("![screenshot]({url})\n\n"));
    }

    out.push_str("---\n\n");
    if !input.reporter_email.trim().is_empty() {
        out.push_str(&format!(
            "Reported by **{}** · priority **{}**\n\n",
            input.reporter_email.trim(),
            normalise_priority(&input.priority),
        ));
    }

    if let Some(map) = input.system_info.as_object()
        && !map.is_empty()
    {
        // Scalar fields → a compact table. The structured diagnostics
        // (console/network/chat) get their own collapsible sections below.
        out.push_str("<details>\n<summary>System information</summary>\n\n");
        out.push_str("| Field | Value |\n|---|---|\n");
        for (k, v) in map {
            if matches!(k.as_str(), "console_logs" | "network_logs" | "chat") {
                continue;
            }
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                // e.g. `allowed_tools` is a string array → comma-join.
                serde_json::Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                other => other.to_string(),
            };
            out.push_str(&format!(
                "| {} | {} |\n",
                md_cell(k),
                md_cell(&val.chars().take(400).collect::<String>()),
            ));
        }
        out.push_str("\n</details>\n\n");

        if let Some(chat) = map.get("chat").and_then(|c| c.as_object()) {
            let sid = chat
                .get("session_id")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let tail = chat
                .get("transcript_tail")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if !tail.is_empty() {
                out.push_str(&format!(
                    "<details>\n<summary>Chat context (session {})</summary>\n\n```\n{}\n```\n\n</details>\n\n",
                    md_cell(sid),
                    fence_safe(tail),
                ));
            }
        }

        if let Some(logs) = map.get("console_logs").and_then(|l| l.as_array())
            && !logs.is_empty()
        {
            out.push_str(&render_console_logs(logs));
        }
        if let Some(logs) = map.get("network_logs").and_then(|l| l.as_array())
            && !logs.is_empty()
        {
            out.push_str(&render_network_logs(logs));
        }
    }

    out
}

/// Escape the pipe + newlines so a value can't break the markdown table.
fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// Neutralise a closing code fence inside fenced content.
fn fence_safe(s: &str) -> String {
    s.replace("```", "ʼʼʼ")
}

/// Collapsible console-log table. Renders the last 50 entries (the client
/// already caps the buffer at 100) so a chatty page can't bloat the issue.
fn render_console_logs(logs: &[serde_json::Value]) -> String {
    let mut out = format!(
        "<details>\n<summary>Console logs ({})</summary>\n\n| Time | Level | Message |\n|---|---|---|\n",
        logs.len()
    );
    for entry in logs.iter().rev().take(50).rev() {
        let o = match entry.as_object() {
            Some(o) => o,
            None => continue,
        };
        let time = short_time(o.get("timestamp").and_then(|t| t.as_str()).unwrap_or(""));
        let level = o.get("level").and_then(|l| l.as_str()).unwrap_or("log");
        let message = o
            .get("args")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            md_cell(&time),
            md_cell(level),
            md_cell(&message.chars().take(300).collect::<String>()),
        ));
    }
    out.push_str("\n</details>\n\n");
    out
}

/// Collapsible network-activity table.
fn render_network_logs(logs: &[serde_json::Value]) -> String {
    let mut out = format!(
        "<details>\n<summary>Network activity ({})</summary>\n\n| Time | Method | Status | ms | URL |\n|---|---|---|---|---|\n",
        logs.len()
    );
    for entry in logs.iter().rev().take(50).rev() {
        let o = match entry.as_object() {
            Some(o) => o,
            None => continue,
        };
        let time = short_time(o.get("timestamp").and_then(|t| t.as_str()).unwrap_or(""));
        let method = o.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let status = o
            .get("status")
            .and_then(serde_json::Value::as_i64)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let dur = o
            .get("duration")
            .and_then(serde_json::Value::as_f64)
            .map(|d| format!("{d:.0}"))
            .unwrap_or_default();
        let url = o.get("url").and_then(|u| u.as_str()).unwrap_or("");
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            md_cell(&time),
            md_cell(method),
            md_cell(&status),
            md_cell(&dur),
            md_cell(&url.chars().take(200).collect::<String>()),
        ));
    }
    out.push_str("\n</details>\n\n");
    out
}

/// Keep just the `HH:MM:SS` of an ISO timestamp for compact tables.
fn short_time(iso: &str) -> String {
    iso.split('T')
        .nth(1)
        .map(|t| {
            t.trim_end_matches('Z')
                .split('.')
                .next()
                .unwrap_or(t)
                .to_string()
        })
        .unwrap_or_else(|| iso.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_base_maps_public_and_enterprise() {
        assert_eq!(web_base("https://api.github.com"), "https://github.com");
        assert_eq!(web_base("https://api.github.com/"), "https://github.com");
        assert_eq!(
            web_base("https://ghe.example.com/api/v3"),
            "https://ghe.example.com"
        );
    }

    #[test]
    fn priority_normalises() {
        assert_eq!(normalise_priority("HIGH"), "high");
        assert_eq!(normalise_priority(" low "), "low");
        assert_eq!(normalise_priority("garbage"), "medium");
        assert_eq!(normalise_priority(""), "medium");
    }

    #[test]
    fn body_includes_sections_and_escapes_table() {
        let input = IssueInput {
            title: "t".into(),
            description: "It broke".into(),
            business_value: "Saves time".into(),
            acceptance_criteria: "- works".into(),
            priority: "high".into(),
            reporter_email: "a@b.c".into(),
            screenshot_png_base64: None,
            system_info: serde_json::json!({ "url": "https://x/y|z", "dpr": 2 }),
        };
        let body = build_issue_body(&input, Some("https://img/x.png"));
        assert!(body.contains("It broke"));
        assert!(body.contains("## Why / Business value"));
        assert!(body.contains("## Acceptance criteria"));
        assert!(body.contains("![screenshot](https://img/x.png)"));
        assert!(body.contains("Reported by **a@b.c**"));
        assert!(body.contains("https://x/y\\|z")); // pipe escaped
        assert!(body.contains("| dpr | 2 |"));
    }

    #[test]
    fn body_renders_console_and_network_sections() {
        let input = IssueInput {
            title: "t".into(),
            description: "d".into(),
            business_value: String::new(),
            acceptance_criteria: String::new(),
            priority: "low".into(),
            reporter_email: String::new(),
            screenshot_png_base64: None,
            system_info: serde_json::json!({
                "url": "https://x/y",
                "allowed_tools": ["search_web", "fetch_url"],
                "console_logs": [{ "timestamp": "2026-06-23T10:00:01.123Z", "level": "error", "args": ["boom", "x"] }],
                "network_logs": [{ "timestamp": "2026-06-23T10:00:02.000Z", "method": "POST", "url": "/api/v0/x", "status": 500, "duration": 42.7 }],
                "chat": { "session_id": "abc", "transcript_tail": "hello world" },
            }),
        };
        let body = build_issue_body(&input, None);
        assert!(body.contains("<summary>Console logs (1)</summary>"));
        assert!(body.contains("| 10:00:01 | error | boom x |"));
        assert!(body.contains("<summary>Network activity (1)</summary>"));
        assert!(body.contains("| 10:00:02 | POST | 500 | 43 | /api/v0/x |"));
        assert!(body.contains("Chat context (session abc)"));
        assert!(body.contains("hello world"));
        // allowed_tools is a string array → comma-joined in the table.
        assert!(body.contains("search_web, fetch_url"));
        // structured keys are not dumped as raw rows.
        assert!(!body.contains("| console_logs |"));
    }

    #[test]
    fn issue_title_is_capped() {
        let long: String = "x".repeat(300);
        assert_eq!(issue_title(&long).chars().count(), 200);
        assert_eq!(issue_title("  hi  "), "hi");
    }
}
