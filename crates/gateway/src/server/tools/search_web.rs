// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Web search. The operator picks the backend via env (mirrors how
//! Open-WebUI does it):
//!
//!   SEARCH_PROVIDER = searxng | brave   (default: searxng)
//!
//! - **searxng** (default): self-hosted federated search. Reads
//!   `SEARXNG_URL` (e.g. `https://searxng.example.com`). No API key,
//!   no per-query cost if the operator runs their own instance.
//!   Hits `<url>/search?q=...&format=json`.
//! - **brave**: Brave Search API. Reads `BRAVE_SEARCH_API_KEY`. Has a
//!   free tier (~2 k q/month) and a clean JSON shape.
//!
//! If the chosen backend is missing its env, the tool fails closed
//! with a clear message — the operator sees it in the model's
//! response and fixes their config. (We deliberately don't fall back
//! between backends; ambiguity about *which* engine answered a query
//! makes debugging miserable.)

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_N_RESULTS: usize = 5;
const MAX_N_RESULTS: usize = 20;

pub struct SearchWeb;

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    /// How many results to return. Defaults to 5; hard-capped at 20.
    #[serde(default)]
    n_results: Option<usize>,
}

impl Tool for SearchWeb {
    fn id(&self) -> &str {
        "search_web"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Search the web. Returns a list of {title, url, snippet} \
             results. Useful for current events, niche facts, anything \
             outside the model's training cutoff.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["query"],
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query."
                    },
                    "n_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_N_RESULTS,
                        "description": "Optional cap on number of results. Defaults to 5."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, _ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: SearchArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!("expected {{query, n_results?}}: {e}"))
            })?;
            if args.query.trim().is_empty() {
                return Err(ToolError::InvalidArgs("query must be non-empty".into()));
            }
            let n = args
                .n_results
                .unwrap_or(DEFAULT_N_RESULTS)
                .clamp(1, MAX_N_RESULTS);

            let provider = std::env::var("SEARCH_PROVIDER")
                .unwrap_or_else(|_| "searxng".to_string())
                .to_lowercase();

            let client = reqwest::Client::builder()
                .timeout(SEARCH_TIMEOUT)
                .user_agent(concat!(
                    "llm-gateway/",
                    env!("CARGO_PKG_VERSION"),
                    " search_web"
                ))
                .build()
                .map_err(|e| ToolError::Failed(format!("HTTP client build: {e}")))?;

            let results = match provider.as_str() {
                "searxng" => searxng(&client, &args.query, n).await?,
                "brave" => brave(&client, &args.query, n).await?,
                other => {
                    return Err(ToolError::Failed(format!(
                        "unsupported SEARCH_PROVIDER `{other}` — expected `searxng` or `brave`"
                    )));
                }
            };

            Ok(json!({
                "provider": provider,
                "query": args.query,
                "results": results,
            }))
        })
    }
}

/// SearXNG `/search?q=...&format=json` returns an envelope with
/// `results: [{title, url, content, ...}]`. We map `content` →
/// `snippet` for shape parity with the brave path.
async fn searxng(client: &reqwest::Client, query: &str, n: usize) -> Result<Vec<Value>, ToolError> {
    let base = std::env::var("SEARXNG_URL").map_err(|_| {
        ToolError::Failed(
            "SEARXNG_URL env var not set — point it at a SearXNG instance \
             (e.g. https://searxng.example.com) or set SEARCH_PROVIDER=brave"
                .into(),
        )
    })?;
    let url = format!("{}/search", base.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .query(&[("q", query), ("format", "json")])
        .send()
        .await
        .map_err(|e| ToolError::Failed(format!("searxng request failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(ToolError::Failed(format!(
            "searxng returned {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        )));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| ToolError::Failed(format!("searxng response is not JSON: {e}")))?;
    let items = body
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ToolError::Failed("searxng response missing `results` array".into()))?;
    Ok(items
        .iter()
        .take(n)
        .map(|item| {
            json!({
                "title": item.get("title").cloned().unwrap_or(Value::Null),
                "url":   item.get("url").cloned().unwrap_or(Value::Null),
                "snippet": item.get("content").cloned().unwrap_or(Value::Null),
            })
        })
        .collect())
}

/// Brave Search API. JSON envelope at `web.results[]`, fields
/// `title`, `url`, `description`. We rename `description` → `snippet`
/// for parity with searxng.
async fn brave(client: &reqwest::Client, query: &str, n: usize) -> Result<Vec<Value>, ToolError> {
    let api_key = std::env::var("BRAVE_SEARCH_API_KEY").map_err(|_| {
        ToolError::Failed(
            "BRAVE_SEARCH_API_KEY env var not set — get a key at \
             https://api.search.brave.com/app/dashboard or set \
             SEARCH_PROVIDER=searxng"
                .into(),
        )
    })?;
    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[("q", query), ("count", &n.to_string())])
        .header("X-Subscription-Token", api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| ToolError::Failed(format!("brave request failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(ToolError::Failed(format!(
            "brave returned {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        )));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| ToolError::Failed(format!("brave response is not JSON: {e}")))?;
    let items = body
        .pointer("/web/results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ToolError::Failed("brave response missing /web/results".into()))?;
    Ok(items
        .iter()
        .take(n)
        .map(|item| {
            json!({
                "title": item.get("title").cloned().unwrap_or(Value::Null),
                "url":   item.get("url").cloned().unwrap_or(Value::Null),
                "snippet": item.get("description").cloned().unwrap_or(Value::Null),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;
    use wiremock::matchers::{method, path, query_param};
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

    /// Combined into one test because both scenarios mutate the same
    /// process-wide `SEARXNG_URL` env var. `#[tokio::test]` parallel
    /// execution would otherwise race two `EnvGuard`s against each
    /// other and flake intermittently. Sequencing both inside one
    /// test keeps the env-mutation window scoped without pulling in
    /// `serial_test`.
    #[tokio::test]
    async fn searxng_path_maps_content_and_fails_without_env() {
        // --- happy path: URL set, wiremock returns two results ---
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("q", "rust"))
            .and(query_param("format", "json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [
                    {"title": "Rust", "url": "https://rust-lang.org", "content": "systems language"},
                    {"title": "Crates.io", "url": "https://crates.io", "content": "package registry"},
                ],
            })))
            .mount(&server)
            .await;

        {
            let _provider = EnvGuard::set("SEARCH_PROVIDER", "searxng");
            let _url = EnvGuard::set("SEARXNG_URL", &server.uri());
            let out = SearchWeb
                .run(ctx().await, json!({"query": "rust"}))
                .await
                .unwrap();
            let results = out["results"].as_array().unwrap();
            assert_eq!(results.len(), 2);
            assert_eq!(results[0]["title"], "Rust");
            assert_eq!(results[0]["snippet"], "systems language");
        }

        // --- failure path: URL unset, error mentions the var ---
        let _provider = EnvGuard::set("SEARCH_PROVIDER", "searxng");
        let _url = EnvGuard::unset("SEARXNG_URL");
        let err = SearchWeb
            .run(ctx().await, json!({"query": "rust"}))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("SEARXNG_URL"), "{msg}");
    }

    #[tokio::test]
    async fn rejects_empty_query() {
        let err = SearchWeb
            .run(ctx().await, json!({"query": "  "}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "{err:?}");
    }

    #[test]
    fn schema_names_match_id() {
        assert_eq!(SearchWeb.id(), SearchWeb.schema().function.name);
    }

    /// Tests in this crate share a process so env vars race. Each
    /// `EnvGuard` sets one var on construction and restores the
    /// original on drop, and we serialise calls with a module-level
    /// mutex acquired via `#[serial]`… except we don't have
    /// `serial_test`, so we accept that two search_web tests running
    /// truly in parallel may interfere. In practice both tests
    /// finish in under 50 ms and SQLite's per-test isolation
    /// guarantees they don't share state otherwise. If this ever
    /// flakes we add `serial_test` or rework to inject the env via
    /// a parameter.
    struct EnvGuard {
        key: &'static str,
        prior: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prior = std::env::var(key).ok();
            // SAFETY: set_var/remove_var are unsafe in 2024 because of
            // racey reads from C library threads. Tests don't spawn
            // such threads; this is the conventional shape for env-
            // scoped tests until something better lands in std.
            unsafe { std::env::set_var(key, value) };
            Self { key, prior }
        }
        fn unset(key: &'static str) -> Self {
            let prior = std::env::var(key).ok();
            unsafe { std::env::remove_var(key) };
            Self { key, prior }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }
}
