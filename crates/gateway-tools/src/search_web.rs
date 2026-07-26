// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Web search. The operator picks the backend in the admin UI; the
//! settings live in the database (see [`search_settings`]):
//!
//! - **searxng** (default): self-hosted federated search. Needs a base URL
//!   (e.g. `https://searxng.example.com`). No API key, no per-query cost if
//!   the operator runs their own instance. Hits
//!   `<url>/search?q=...&format=json`.
//! - **brave**: Brave Search API. Needs a subscription token, which is
//!   sealed at rest. Has a free tier (~2 k q/month) and a clean JSON shape.
//!
//! If the chosen backend isn't configured the tool fails closed with a clear
//! message naming the admin page — the operator sees it in the model's
//! response and fixes it. (We deliberately don't fall back between backends;
//! ambiguity about *which* engine answered a query makes debugging
//! miserable.)
//!
//! The legacy `SEARCH_PROVIDER` / `SEARXNG_URL` / `BRAVE_SEARCH_API_KEY`
//! environment variables are imported into the database once at boot and
//! ignored from then on — see [`search_settings::import_env_once`].
//!
//! [`search_settings`]: gateway_features::server::search_settings

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use gateway_features::server::search_settings::{self, SearchProvider, SearchSettings};
use gateway_runtime::server::tools::{Tool, ToolContext, ToolError, ToolFuture};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_N_RESULTS: usize = 5;
const MAX_N_RESULTS: usize = 20;
/// Cap on `site` entries. A handful scopes a search usefully; a hundred
/// makes a query the engines reject and would let a model paste an entire
/// domain list into one request.
const MAX_SITES: usize = 8;

pub struct SearchWeb;

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    /// How many results to return. Defaults to 5; hard-capped at 20.
    #[serde(default)]
    n_results: Option<usize>,
    /// Restrict results to these domains.
    #[serde(default)]
    site: Option<Vec<String>>,
    /// Restrict results to this recency window.
    #[serde(default)]
    freshness: Option<Freshness>,
}

/// Recency window. Both providers support this natively, with different
/// spellings — see [`Freshness::brave`] / [`Freshness::searxng`].
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Freshness {
    Day,
    Week,
    Month,
    Year,
}

impl Freshness {
    /// Brave's `freshness` parameter.
    fn brave(self) -> &'static str {
        match self {
            Self::Day => "pd",
            Self::Week => "pw",
            Self::Month => "pm",
            Self::Year => "py",
        }
    }

    /// SearXNG's `time_range` parameter. It has no "past year" bucket, so a
    /// year falls back to `year`'s nearest equivalent — SearXNG accepts
    /// `day`, `week`, `month`, `year`.
    fn searxng(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

/// Validate and normalise the `site` list: strip a scheme/path a model may
/// have pasted, drop empties, cap the count.
fn normalize_sites(sites: Option<Vec<String>>) -> Result<Vec<String>, ToolError> {
    let Some(sites) = sites else {
        return Ok(Vec::new());
    };
    let cleaned: Vec<String> = sites
        .iter()
        .filter_map(|s| {
            let s = s.trim();
            let s = s
                .strip_prefix("https://")
                .or_else(|| s.strip_prefix("http://"))
                .unwrap_or(s);
            // Keep the host only; `docs.example.com/guide` is not a domain
            // and both engines' `site:` operators reject the path.
            let host = s.split('/').next().unwrap_or("").trim();
            (!host.is_empty()).then(|| host.to_ascii_lowercase())
        })
        .collect();
    if cleaned.len() > MAX_SITES {
        return Err(ToolError::InvalidArgs(format!(
            "at most {MAX_SITES} `site` entries (got {})",
            cleaned.len()
        )));
    }
    Ok(cleaned)
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
             outside the model's training cutoff. Narrow the search with \
             `site` (only these domains) and `freshness` (only recent \
             results) instead of adding those constraints as words to the \
             query — the engine filters far more reliably than the phrasing \
             does.",
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
                    },
                    "site": {
                        "type": "array",
                        "maxItems": MAX_SITES,
                        "items": {"type": "string"},
                        "description": "Optional domain allowlist — only \
                                        results from these domains, e.g. \
                                        [\"docs.ceph.com\"]. Bare domains, no \
                                        scheme and no path."
                    },
                    "freshness": {
                        "type": "string",
                        "enum": ["day", "week", "month", "year"],
                        "description": "Optional recency window. Use it for \
                                        \"what changed recently\" questions; \
                                        omit it when the answer is not \
                                        time-sensitive, since it discards \
                                        older but still-correct sources."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: SearchArgs = serde_json::from_value(args).map_err(|e| {
                ToolError::InvalidArgs(format!(
                    "expected {{query, n_results?, site?, freshness?}}: {e}"
                ))
            })?;
            if args.query.trim().is_empty() {
                return Err(ToolError::InvalidArgs("query must be non-empty".into()));
            }
            let n = args
                .n_results
                .unwrap_or(DEFAULT_N_RESULTS)
                .clamp(1, MAX_N_RESULTS);
            let sites = normalize_sites(args.site)?;

            // Settings come from the DB. Without a crypto handle a sealed
            // Brave key can't be opened; the searxng path is unaffected, so
            // we degrade rather than refuse outright.
            let settings = match ctx.crypto.as_ref() {
                Some(crypto) => search_settings::load(&ctx.db, crypto)
                    .await
                    .map_err(|e| ToolError::Failed(format!("loading search settings: {e}")))?,
                None => SearchSettings {
                    provider: search_settings::SearchProvider::default(),
                    searxng_url: None,
                    brave_api_key: None,
                },
            };

            let client = reqwest::Client::builder()
                .timeout(SEARCH_TIMEOUT)
                .user_agent(concat!(
                    "llm-gateway/",
                    env!("CARGO_PKG_VERSION"),
                    " search_web"
                ))
                .build()
                .map_err(|e| ToolError::Failed(format!("HTTP client build: {e}")))?;

            let query = Query {
                text: &args.query,
                n,
                sites: &sites,
                freshness: args.freshness,
            };
            let results = match settings.provider {
                SearchProvider::Searxng => {
                    searxng(&client, settings.searxng_url.as_deref(), query).await?
                }
                SearchProvider::Brave => {
                    brave(&client, settings.brave_api_key.as_deref(), query).await?
                }
            };

            Ok(json!({
                "provider": settings.provider.as_str(),
                "query": args.query,
                "site": sites,
                "freshness": args.freshness.map(|f| f.searxng()),
                "results": results,
            }))
        })
    }
}

/// One search request, provider-agnostic.
#[derive(Clone, Copy)]
struct Query<'a> {
    text: &'a str,
    n: usize,
    sites: &'a [String],
    freshness: Option<Freshness>,
}

impl Query<'_> {
    /// The query string with a `site:` clause appended when domains were
    /// requested. Both engines accept `(site:a OR site:b)`; SearXNG passes it
    /// through to the underlying engines and Brave parses it natively, so one
    /// spelling covers both.
    fn with_site_operator(&self) -> String {
        if self.sites.is_empty() {
            return self.text.to_string();
        }
        let clause = self
            .sites
            .iter()
            .map(|s| format!("site:{s}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        if self.sites.len() == 1 {
            format!("{} {clause}", self.text)
        } else {
            format!("{} ({clause})", self.text)
        }
    }
}

/// SearXNG `/search?q=...&format=json` returns an envelope with
/// `results: [{title, url, content, ...}]`. We map `content` →
/// `snippet` for shape parity with the brave path.
async fn searxng(
    client: &reqwest::Client,
    base: Option<&str>,
    query: Query<'_>,
) -> Result<Vec<Value>, ToolError> {
    let base = base.ok_or_else(|| {
        ToolError::Failed(
            "web search is not configured: no SearXNG URL is set. An admin \
             sets it (or switches to Brave) under Web search on /admin/models."
                .into(),
        )
    })?;
    let url = format!("{}/search", base.trim_end_matches('/'));
    let q = query.with_site_operator();
    let mut params: Vec<(&str, String)> = vec![("q", q), ("format", "json".into())];
    if let Some(f) = query.freshness {
        params.push(("time_range", f.searxng().into()));
    }
    let resp = client
        .get(&url)
        .query(&params)
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
        .take(query.n)
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
async fn brave(
    client: &reqwest::Client,
    api_key: Option<&str>,
    query: Query<'_>,
) -> Result<Vec<Value>, ToolError> {
    let api_key = api_key.ok_or_else(|| {
        ToolError::Failed(
            "web search is not configured: no Brave API key is set. An admin \
             sets it (or switches to SearXNG) under Web search on \
             /admin/models; keys come from \
             https://api.search.brave.com/app/dashboard."
                .into(),
        )
    })?;
    let mut params: Vec<(&str, String)> = vec![
        ("q", query.with_site_operator()),
        ("count", query.n.to_string()),
    ];
    if let Some(f) = query.freshness {
        params.push(("freshness", f.brave().into()));
    }
    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&params)
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
        .take(query.n)
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
    use gateway_core::server::crypto::Crypto;
    use gateway_core::server::db;
    use std::sync::Arc;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn ctx() -> ToolContext {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        ToolContext::for_test(pool)
    }

    /// A context whose DB already holds the given search settings. Replaces
    /// the old env-var juggling: settings are per-database now, so tests are
    /// fully isolated and can run in parallel.
    async fn ctx_with_searxng(url: &str) -> ToolContext {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        search_settings::set_provider(&pool, SearchProvider::Searxng)
            .await
            .unwrap();
        search_settings::set_searxng_url(&pool, url).await.unwrap();
        ToolContext {
            crypto: Some(Arc::new(Crypto::ephemeral())),
            ..ToolContext::for_test(pool)
        }
    }

    /// Mount a SearXNG-shaped JSON response and return the server.
    async fn searxng_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("format", "json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [
                    {"title": "Rust", "url": "https://rust-lang.org", "content": "systems language"},
                    {"title": "Crates.io", "url": "https://crates.io", "content": "package registry"},
                ],
            })))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn searxng_path_maps_content_to_snippet() {
        let server = searxng_server().await;
        let out = SearchWeb
            .run(
                ctx_with_searxng(&server.uri()).await,
                json!({"query": "rust"}),
            )
            .await
            .unwrap();
        assert_eq!(out["provider"], "searxng");
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["title"], "Rust");
        assert_eq!(results[0]["snippet"], "systems language");
    }

    #[tokio::test]
    async fn unconfigured_searxng_names_the_admin_page_not_an_env_var() {
        // The whole point of moving config into the DB: the operator-facing
        // error has to point at where the setting now lives.
        let err = SearchWeb
            .run(ctx().await, json!({"query": "rust"}))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("/admin/models"), "{msg}");
        assert!(
            !msg.contains("SEARXNG_URL"),
            "must not name an env var: {msg}"
        );
    }

    #[tokio::test]
    async fn brave_without_a_key_names_the_admin_page() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        search_settings::set_provider(&pool, SearchProvider::Brave)
            .await
            .unwrap();
        let ctx = ToolContext {
            crypto: Some(Arc::new(Crypto::ephemeral())),
            ..ToolContext::for_test(pool)
        };
        let msg = format!(
            "{}",
            SearchWeb.run(ctx, json!({"query": "x"})).await.unwrap_err()
        );
        assert!(msg.contains("/admin/models"), "{msg}");
        assert!(!msg.contains("BRAVE_SEARCH_API_KEY"), "{msg}");
    }

    #[tokio::test]
    async fn provider_comes_from_the_database() {
        // Provider = brave in the DB, so the searxng URL is irrelevant and
        // the missing Brave key is what surfaces.
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        search_settings::set_searxng_url(&pool, "https://ignored.example")
            .await
            .unwrap();
        search_settings::set_provider(&pool, SearchProvider::Brave)
            .await
            .unwrap();
        let ctx = ToolContext {
            crypto: Some(Arc::new(Crypto::ephemeral())),
            ..ToolContext::for_test(pool)
        };
        let msg = format!(
            "{}",
            SearchWeb.run(ctx, json!({"query": "x"})).await.unwrap_err()
        );
        assert!(msg.contains("Brave"), "{msg}");
    }

    #[tokio::test]
    async fn freshness_is_forwarded_as_searxng_time_range() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("time_range", "week"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;
        // The mock only matches when `time_range=week` is present, so a
        // successful call is the assertion.
        let out = SearchWeb
            .run(
                ctx_with_searxng(&server.uri()).await,
                json!({"query": "ceph release", "freshness": "week"}),
            )
            .await
            .unwrap();
        assert_eq!(out["freshness"], "week");
    }

    #[tokio::test]
    async fn site_list_becomes_a_site_operator_in_the_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param(
                "q",
                "osd tuning (site:docs.ceph.com OR site:tracker.ceph.com)",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;
        let out = SearchWeb
            .run(
                ctx_with_searxng(&server.uri()).await,
                json!({
                    "query": "osd tuning",
                    // Scheme and path must be stripped before use.
                    "site": ["https://docs.ceph.com/en/latest", "TRACKER.ceph.com"],
                }),
            )
            .await
            .unwrap();
        assert_eq!(out["site"], json!(["docs.ceph.com", "tracker.ceph.com"]));
    }

    #[tokio::test]
    async fn single_site_needs_no_parentheses() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("q", "rgw site:docs.ceph.com"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"results": []})),
            )
            .mount(&server)
            .await;
        SearchWeb
            .run(
                ctx_with_searxng(&server.uri()).await,
                json!({"query": "rgw", "site": ["docs.ceph.com"]}),
            )
            .await
            .unwrap();
    }

    #[test]
    fn normalize_sites_strips_scheme_and_path_and_lowercases() {
        let got = normalize_sites(Some(vec![
            "https://Docs.Example.com/a/b".into(),
            "http://x.example".into(),
            "  y.example  ".into(),
            "".into(),
        ]))
        .unwrap();
        assert_eq!(got, vec!["docs.example.com", "x.example", "y.example"]);
    }

    #[test]
    fn normalize_sites_rejects_an_overlong_list() {
        let many: Vec<String> = (0..MAX_SITES + 1)
            .map(|i| format!("d{i}.example"))
            .collect();
        assert!(matches!(
            normalize_sites(Some(many)).unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[test]
    fn freshness_maps_to_each_provider_spelling() {
        assert_eq!(Freshness::Day.brave(), "pd");
        assert_eq!(Freshness::Week.brave(), "pw");
        assert_eq!(Freshness::Month.brave(), "pm");
        assert_eq!(Freshness::Year.brave(), "py");
        assert_eq!(Freshness::Day.searxng(), "day");
        assert_eq!(Freshness::Year.searxng(), "year");
    }

    #[test]
    fn unknown_freshness_is_rejected_at_the_arg_boundary() {
        assert!(
            serde_json::from_value::<SearchArgs>(json!({"query": "x", "freshness": "decade"}))
                .is_err()
        );
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

    #[test]
    fn schema_advertises_site_and_freshness() {
        let schema = SearchWeb.schema();
        let props = &schema.function.parameters["properties"];
        assert_eq!(props["site"]["type"], "array");
        assert_eq!(props["site"]["maxItems"], MAX_SITES);
        assert_eq!(
            props["freshness"]["enum"],
            json!(["day", "week", "month", "year"])
        );
    }
}
