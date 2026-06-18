// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! `wikipedia` — look up an encyclopedic summary from Wikipedia. Read-only,
//! keyless, public data; better factual grounding than a raw web search for
//! "who/what is X" questions. One request to the MediaWiki action API:
//! search for the best-matching article and return its intro extract + URL.

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture};

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
// Wikipedia's API policy asks for a descriptive User-Agent.
const USER_AGENT: &str = concat!(
    "llm-gateway/",
    env!("CARGO_PKG_VERSION"),
    " (wikipedia tool)"
);
/// Cap the returned extract so a long lead section doesn't blow up the
/// model's context; the model can ask a follow-up or fetch the URL for more.
const MAX_EXTRACT: usize = 1500;

pub struct Wikipedia;

#[derive(Deserialize)]
struct Args {
    query: String,
    /// Wikipedia language edition (e.g. "en", "de"). Defaults to "en".
    #[serde(default)]
    lang: Option<String>,
}

impl Tool for Wikipedia {
    fn id(&self) -> &str {
        "wikipedia"
    }

    fn schema(&self) -> ToolDef {
        ToolDef::function(
            self.id(),
            "Look up a topic on Wikipedia and return the best-matching article's title, \
             intro summary, and URL. Read-only public reference data — prefer this over a \
             web search for encyclopedic 'who/what/where is X' questions.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "What to look up, e.g. \"Ceph (software)\" or \"erasure coding\"." },
                    "lang": { "type": "string", "description": "Wikipedia language edition (ISO code like \"en\", \"de\"). Defaults to \"en\"." }
                }
            }),
        )
    }

    fn run<'a>(&'a self, _ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{query, lang?}}: {e}")))?;
            let query = args.query.trim();
            if query.is_empty() {
                return Err(ToolError::InvalidArgs("`query` must not be empty".into()));
            }
            // Keep the language code to a sane shape (it's interpolated into a host).
            let lang = match args
                .lang
                .as_deref()
                .map(str::trim)
                .filter(|l| !l.is_empty())
            {
                Some(l)
                    if l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                        && l.len() <= 12 =>
                {
                    l
                }
                Some(_) => {
                    return Err(ToolError::InvalidArgs(
                        "`lang` is not a valid language code".into(),
                    ));
                }
                None => "en",
            };

            let client = reqwest::Client::builder()
                .timeout(TIMEOUT)
                .user_agent(USER_AGENT)
                .build()
                .map_err(|e| ToolError::Failed(format!("HTTP client build: {e}")))?;

            // One round-trip: `generator=search` finds the best article and
            // `prop=extracts|info` returns its plain-text intro + canonical URL.
            let url = format!("https://{lang}.wikipedia.org/w/api.php");
            let resp = client
                .get(&url)
                .query(&[
                    ("action", "query"),
                    ("format", "json"),
                    ("generator", "search"),
                    ("gsrsearch", query),
                    ("gsrlimit", "1"),
                    ("prop", "extracts|info"),
                    ("exintro", "1"),
                    ("explaintext", "1"),
                    ("inprop", "url"),
                    ("redirects", "1"),
                ])
                .send()
                .await
                .map_err(|e| ToolError::Failed(format!("Wikipedia request failed: {e}")))?;
            if !resp.status().is_success() {
                return Err(ToolError::Failed(format!(
                    "Wikipedia API returned {}",
                    resp.status()
                )));
            }
            let body: Value = resp
                .json()
                .await
                .map_err(|e| ToolError::Failed(format!("Wikipedia response parse: {e}")))?;

            // `query.pages` is an object keyed by pageid; take the first page.
            let page = body
                .get("query")
                .and_then(|q| q.get("pages"))
                .and_then(Value::as_object)
                .and_then(|pages| pages.values().next());
            let Some(page) = page else {
                return Ok(json!({
                    "found": false, "query": query, "lang": lang,
                    "note": "No matching Wikipedia article.",
                }));
            };

            let title = page.get("title").and_then(Value::as_str).unwrap_or("");
            let mut extract = page
                .get("extract")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let truncated = extract.chars().count() > MAX_EXTRACT;
            if truncated {
                extract = extract.chars().take(MAX_EXTRACT).collect::<String>() + "…";
            }
            let page_url = page
                .get("canonicalurl")
                .or_else(|| page.get("fullurl"))
                .and_then(Value::as_str);

            Ok(json!({
                "found": true,
                "title": title,
                "summary": extract,
                "url": page_url,
                "lang": lang,
                "truncated": truncated,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_name_matches_id() {
        assert_eq!(Wikipedia.id(), Wikipedia.schema().function.name);
    }
}
