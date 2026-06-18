// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};

/// Thin HTTP client for the gateway.
///
/// Two flavours of state: an anonymous client for /auth/cli/* endpoints, and
/// a bearer-bearing client for /v1/* identity calls. `with_bearer()` upgrades
/// the former to the latter without mutating the original.
#[derive(Clone)]
pub struct GatewayClient {
    base_url: String,
    bearer: Option<String>,
    http: reqwest::Client,
}

impl GatewayClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            bearer: None,
            http: reqwest::Client::new(),
        }
    }

    pub fn with_bearer(mut self, bearer: impl Into<String>) -> Self {
        self.bearer = Some(bearer.into());
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    fn add_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.bearer {
            Some(b) => builder.bearer_auth(b),
            None => builder,
        }
    }

    // ----- /healthz ---------------------------------------------------------

    pub async fn healthz(&self) -> anyhow::Result<()> {
        let url = self.url("healthz");
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!(
                "gateway returned {status} for /healthz (expected 2xx)"
            ));
        }
        let body = response
            .text()
            .await
            .context("reading /healthz response body")?;
        if body.trim() != shared::HEALTHZ_BODY {
            return Err(anyhow!(
                "unexpected /healthz body: got {body:?}, expected {expected:?}",
                expected = shared::HEALTHZ_BODY
            ));
        }
        Ok(())
    }

    // ----- CLI login flow ---------------------------------------------------

    pub async fn auth_cli_start(&self, pkce_challenge: &str) -> anyhow::Result<AuthCliStart> {
        let url = self.url("auth/cli/start");
        let body = serde_json::json!({ "pkce_challenge": pkce_challenge });
        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "gateway returned {status} for /auth/cli/start: {body}"
            ));
        }
        let r: AuthCliStart = response.json().await.context("parsing start response")?;
        Ok(r)
    }

    /// `Ok(Some(token))` when the login completes, `Ok(None)` while still
    /// pending, `Err` on protocol failure.
    pub async fn auth_cli_poll(
        &self,
        state: &str,
        pkce_verifier: &str,
    ) -> anyhow::Result<Option<String>> {
        let url = self.url("auth/cli/poll");
        let body = serde_json::json!({
            "state": state,
            "pkce_verifier": pkce_verifier,
        });
        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = response.status();
        match status {
            s if s == reqwest::StatusCode::NO_CONTENT => Ok(None),
            s if s.is_success() => {
                let r: AuthCliPoll = response.json().await.context("parsing poll response")?;
                Ok(Some(r.token))
            }
            reqwest::StatusCode::UNAUTHORIZED => {
                Err(anyhow!("sign-in failed or session expired; please retry"))
            }
            other => {
                let body = response.text().await.unwrap_or_default();
                Err(anyhow!(
                    "gateway returned {other} for /auth/cli/poll: {body}"
                ))
            }
        }
    }

    // ----- bearer-authenticated calls --------------------------------------

    pub async fn me(&self) -> anyhow::Result<Me> {
        let url = self.url("v1/me");
        let response = self
            .add_auth(self.http.get(&url))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("gateway returned {status} for /v1/me: {body}"));
        }
        response.json().await.context("parsing /v1/me response")
    }

    pub async fn logout(&self) -> anyhow::Result<()> {
        let url = self.url("v1/auth/logout");
        let response = self
            .add_auth(self.http.post(&url))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        if !response.status().is_success() {
            let status = response.status();
            return Err(anyhow!("gateway returned {status} for /v1/auth/logout"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthCliStart {
    pub state: String,
    pub login_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthCliPoll {
    token: String,
}

/// Re-export from shared to keep the CLI's surface tight.
pub use shared::api::{Me, ToolSummary};
