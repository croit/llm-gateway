// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Lets the model turn on additional tools mid-conversation.
//!
//! Every tool except this one is lazy: `allowed_tools_for_session`
//! narrows the per-turn `tools` block to `enable_tools` plus whatever the
//! conversation has explicitly turned on via `chat_session_tools`. This
//! tool is the bootstrap — the model sees the full catalog of available
//! tool groups in its description, calls
//! `enable_tools(["fetch_url", "wikipedia"])`, and the *next* round of
//! `runner::inject_tools` surfaces the real schemas. Sticky — once on, a
//! group stays enabled for the rest of the conversation.
//!
//! Belt-and-braces: the chat driver also auto-enables on a direct
//! tool_call (`source = "auto-call"`), so a model that calls `fetch_url`
//! without going through `enable_tools` first still works — it just pays
//! the same one-round retry cost if the hallucinated args don't match
//! the real schema. See `openai_driver::run_one_turn`.
//!
//! On the proxy paths (no chat session) it refuses cleanly: there's
//! nothing to write the enablement to.
//!
//! Snapshots its catalog at registration time from the live `ToolRegistry`.
//! The schema description carries the keys → titles → one-liners the model
//! needs to choose; the `keys[].enum` constrains valid calls. RBAC narrowing
//! happens at runtime in `run`, not in the schema — the model occasionally
//! seeing a key it can't use is cheaper than re-rendering the schema per
//! user (which would blow the upstream prefix cache).

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::catalog::{BOOTSTRAP_TOOL_ID, entry_key_for, is_hidden};
use super::mcp::MCP_ID_PREFIX;
use super::{Tool, ToolContext, ToolError, ToolFuture, ToolRegistry};

/// One enableable group as advertised in this tool's schema. Key matches
/// `entry_key_for` output, so `chat_session_tools` writes line up with the
/// `allowed_tools_for_session` overlay.
#[derive(Clone, Debug)]
pub struct EnableTarget {
    pub key: String,
    pub title: String,
    pub one_liner: String,
}

pub struct EnableTools {
    catalog: Arc<Vec<EnableTarget>>,
}

impl EnableTools {
    /// Take a snapshot of the registry's catalog so the schema description
    /// can list every group the model could turn on. Should be built *after*
    /// every other tool (static + MCP + typst) is registered.
    pub fn from_registry(registry: &ToolRegistry) -> Self {
        let mut seen = std::collections::HashSet::new();
        let mut catalog: Vec<EnableTarget> = registry
            .ids()
            .filter_map(|id| {
                // Smoke-test / internal-plumbing tools (e.g. `company_echo`)
                // stay granted via RBAC but are never advertised — same gate
                // the `/tools` page uses, so the two surfaces agree.
                if is_hidden(id) {
                    return None;
                }
                let key = entry_key_for(id).to_string();
                // enable_tools itself is the bootstrap — it's already on, so
                // never advertise it as something to turn on (defence in depth:
                // at snapshot time it's not yet in the registry anyway).
                if key == BOOTSTRAP_TOOL_ID {
                    return None;
                }
                if !seen.insert(key.clone()) {
                    return None;
                }
                Some(target_for_key(&key))
            })
            .collect();
        // Byte-stable order across boots so the schema description (which
        // ends up in the prefix cache) doesn't churn.
        catalog.sort_by(|a, b| a.key.cmp(&b.key));
        Self {
            catalog: Arc::new(catalog),
        }
    }
}

#[derive(Deserialize)]
struct EnableArgs {
    keys: Vec<String>,
}

impl Tool for EnableTools {
    fn id(&self) -> &str {
        "enable_tools"
    }

    fn schema(&self) -> ToolDef {
        let mut description = String::from(
            "Turn on one or more additional tools for the rest of this conversation. \
             Use this whenever the user's request needs a capability that isn't already \
             in your current tools list — call it BEFORE you say you can't do something. \
             Enablement is sticky: once a key is on it stays on for the remaining turns. \
             The real tool schemas appear in your tools list on the next turn after the \
             call succeeds.\n\nAvailable keys:\n",
        );
        for t in self.catalog.iter() {
            description.push_str(&format!("- {} — {} ({})\n", t.key, t.title, t.one_liner));
        }
        if self.catalog.is_empty() {
            description
                .push_str("(none built in — every available capability is already active)\n");
        }
        description.push_str(
            "\nThe signed-in user may also have connected MCP integrations (keys like \
             `mcp__github`). Those are listed in the system context message, not here — \
             pass such a key to turn the integration on.\n",
        );
        // NB: deliberately *no* `enum` constraint on the items. The static
        // catalog above can't include the user's per-account MCP connector keys
        // (they're resolved per request), and a guided-decoding backend would
        // reject any key outside an enum — so the model could never enable a
        // connected integration. Validation happens at runtime in `run`
        // instead: unknown keys are skipped, and enabling a key that surfaces
        // no tools is harmless (the MCP layer is the authoritative gate).
        ToolDef::function(
            self.id(),
            description,
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["keys"],
                "properties": {
                    "keys": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 8,
                        "items": { "type": "string" },
                        "description": "Toggle keys to enable for this conversation \
                                        — pick from the list in this tool's description, \
                                        or an `mcp__*` integration key from the system context."
                    }
                }
            }),
        )
    }

    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: EnableArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArgs(format!("expected {{keys: [string]}}: {e}")))?;
            let Some(session_id) = ctx.session_id.as_deref() else {
                return Err(ToolError::Failed(
                    "enable_tools only works inside a chat session — this request has no \
                     persisted conversation to enable tools for"
                        .into(),
                ));
            };
            // Snapshot the keys we advertise so we can reject typos before
            // writing rows.
            let known: std::collections::HashSet<&str> =
                self.catalog.iter().map(|t| t.key.as_str()).collect();

            let mut enabled: Vec<String> = Vec::new();
            let mut skipped: Vec<Value> = Vec::new();
            for key in &args.keys {
                // Accept the static catalog keys, plus any `mcp__*` connector
                // key (the user's connected integrations aren't in the static
                // snapshot — they're advertised in the system context). An
                // `mcp__*` key that doesn't match a connected connector just
                // surfaces no tools; the MCP layer is the real gate, so writing
                // the row is harmless.
                if !known.contains(key.as_str()) && !key.starts_with(MCP_ID_PREFIX) {
                    skipped.push(json!({ "key": key, "reason": "unknown key" }));
                    continue;
                }
                if let Err(err) = crate::server::db::chat_session_tools::set(
                    &ctx.db, session_id, key, true, "model",
                )
                .await
                {
                    skipped.push(
                        json!({ "key": key, "reason": format!("persisting enablement: {err}") }),
                    );
                    continue;
                }
                enabled.push(key.clone());
            }
            tracing::info!(
                session = %session_id,
                ?enabled,
                "enable_tools: model enabled groups for this conversation"
            );
            Ok(json!({
                "status": "ok",
                "enabled": enabled,
                "skipped": skipped,
                "note": "the enabled tools' real schemas appear in your tools list on the \
                        next turn",
            }))
        })
    }
}

/// Pick a human-readable `(title, one_liner)` for `key`. Reuses the
/// `/tools` page's hand-written copy where it exists (so the model and the
/// settings UI tell the same story), with sensible per-prefix fallbacks for
/// the variadic groups (`typst_*`, `mcp__*`) and one-off unknowns.
fn target_for_key(key: &str) -> EnableTarget {
    // The Web/Network/Documents/etc routing groups in the old embedding
    // router collapse here into one row per *toggle* key (so e.g. `fetch_url`
    // and `search_web` are two separate keys the model can pick from).
    // Copy is written for a capable model: the *key* already names the
    // concept (the model knows what DNS or a TLS cert is), so each one-liner
    // states the tool's specific affordance — what action, on what subject —
    // rather than re-teaching the concept. Product-specific tools (RAG,
    // attachments, sandbox) get fuller copy because the name alone doesn't
    // convey what's behind it.
    let (title, one_liner) = match key {
        "memory" => (
            "Memory",
            "remember durable facts about the user and recall them in later turns",
        ),
        "get_current_timestamp" => (
            "Current date & time",
            "the user's current local date and time (the system context carries none)",
        ),
        "get_user_location" => (
            "Precise user location",
            "prompt the browser for a precise GPS fix (coarse IP location is already in \
             the system context)",
        ),
        "search_web" => (
            "Web search",
            "search the web for current information, news, or recent events",
        ),
        "fetch_url" => (
            "Fetch a web page",
            "fetch a specific http(s) URL and read its contents (text, JSON, images)",
        ),
        "wikipedia" => (
            "Wikipedia lookup",
            "fetch the summary of a Wikipedia article",
        ),
        "typst" => (
            "Document rendering",
            "render a PDF from a corporate template (letter, invoice, report)",
        ),
        "document" => (
            "Document canvas",
            "build up a long document (guide, spec, config) in a live side panel and edit it \
             one passage at a time across turns — without rewriting the whole thing",
        ),
        "fetch_attachment" => (
            "Read an attachment",
            "read a file the user attached to this chat (text, images, PDFs incl. scanned)",
        ),
        "upload_attachment" => (
            "Attach a file to your reply",
            "attach a generated file (PDF/PNG/text) for the user to download",
        ),
        "rag_search" => (
            "Knowledge-base search",
            "semantic search over this gateway's indexed repositories & documents — \
             use it for questions about the user's own codebase, docs, or data",
        ),
        "rag_list_collections" => (
            "List knowledge bases",
            "list the indexed collections available to rag_search (call this first)",
        ),
        "run_in_sandbox" => (
            "Code sandbox",
            "run Python or shell in an isolated, throwaway sandbox",
        ),
        "generate_document" => (
            "Document generation",
            "turn Markdown into a downloadable PDF, Word, or PowerPoint file",
        ),
        "capture_webpage" => (
            "Web page capture",
            "screenshot, PDF, or extract the text of a web page via a headless browser",
        ),
        "convert_document" => (
            "Convert uploaded file",
            "convert an uploaded file (PowerPoint/Word/Excel/PDF) to PDF, Word, text, \
             HTML, or per-slide images",
        ),
        "edit_presentation" => (
            "Edit PowerPoint",
            "modify an uploaded PowerPoint (.pptx) deck with python-pptx",
        ),
        "read_sandbox_output" => (
            "Read large sandbox output",
            "grep or page through a previous sandbox run's large output",
        ),
        "dns_lookup" => ("DNS lookup", "resolve a hostname's A/AAAA/MX/TXT records"),
        "whois_lookup" => (
            "Domain WHOIS",
            "a domain's registration owner, dates & status (RDAP)",
        ),
        "tls_cert" => (
            "TLS certificate check",
            "inspect a remote site's TLS certificate: issuer & expiry",
        ),
        "lookup_ip" => (
            "IP / host location",
            "geolocate an IP or hostname via offline GeoIP",
        ),
        "convert_currency" => (
            "Currency conversion",
            "convert between currencies at daily ECB rates",
        ),
        _ if key.starts_with(MCP_ID_PREFIX) => {
            let server = key.strip_prefix(MCP_ID_PREFIX).unwrap_or(key);
            return EnableTarget {
                key: key.to_string(),
                title: format!("{server} (MCP)"),
                one_liner: format!("bridged tools from the '{server}' MCP server"),
            };
        }
        _ => ("Tool group", "see your /tools page for details"),
    };
    EnableTarget {
        key: key.to_string(),
        title: title.to_string(),
        one_liner: one_liner.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;
    use crate::server::tools::fetch_url::FetchUrl;
    use crate::server::tools::search_web::SearchWeb;
    use crate::server::tools::time::CurrentTimestamp;

    async fn ctx(pool: db::Pool, session_id: Option<String>) -> ToolContext {
        ToolContext {
            user_id: "u1".into(),
            roles: vec!["user".into()],
            db: pool,
            s3: None,
            assistant_turn_id: None,
            session_id,
            client_ip: None,
            geoip: None,
            chat_feedback: None,
            attachment_reservations: None,
            indexer: None,
        }
    }

    async fn seed_session(pool: &db::Pool, id: &str) {
        sqlx::query(
            r#"INSERT INTO users (id, email, created_at, updated_at)
               VALUES ('u1', 'u1@example.com', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')
               ON CONFLICT(id) DO NOTHING"#,
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r#"INSERT INTO chat_sessions (id, user_id, created_at, updated_at)
               VALUES (?, 'u1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"#,
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[test]
    fn snapshot_lists_every_registered_tool() {
        // Everything except `enable_tools` itself is now lazy — including
        // previously-core tools like get_current_timestamp.
        let reg = ToolRegistry::new()
            .with(CurrentTimestamp)
            .with(FetchUrl)
            .with(SearchWeb);
        let et = EnableTools::from_registry(&reg);
        let keys: Vec<&str> = et.catalog.iter().map(|t| t.key.as_str()).collect();
        assert!(keys.contains(&"get_current_timestamp"));
        assert!(keys.contains(&"fetch_url"));
        assert!(keys.contains(&"search_web"));
    }

    #[test]
    fn hidden_tools_are_not_advertised() {
        // `company_echo` is a smoke-test tool listed in `catalog::HIDDEN`; it
        // stays granted via RBAC but must never appear as an enableable key
        // (same gate the `/tools` page applies).
        use crate::server::tools::echo::Echo;
        let reg = ToolRegistry::new().with(Echo).with(FetchUrl);
        let et = EnableTools::from_registry(&reg);
        let keys: Vec<&str> = et.catalog.iter().map(|t| t.key.as_str()).collect();
        assert!(keys.contains(&"fetch_url"));
        assert!(
            !keys.contains(&"company_echo"),
            "hidden smoke-test tool must not be advertised: {keys:?}"
        );
    }

    #[test]
    fn rag_tools_get_real_copy_not_the_fallback() {
        // Regression: rag_* used to fall through to the "see your /tools page"
        // default one-liner, so the model couldn't tell it could search the
        // indexed codebase/docs.
        use crate::server::tools::rag::{RagListCollections, RagSearch};
        let reg = ToolRegistry::new().with(RagListCollections).with(RagSearch);
        let et = EnableTools::from_registry(&reg);
        let rag = et
            .catalog
            .iter()
            .find(|t| t.key == "rag_search")
            .expect("rag_search should be advertised");
        assert!(
            !rag.one_liner.contains("see your /tools page"),
            "rag_search fell through to the default copy: {:?}",
            rag.one_liner
        );
        assert!(rag.one_liner.to_lowercase().contains("search"));
    }

    #[test]
    fn snapshot_collapses_memory_ids_to_one_key() {
        use crate::server::tools::memory::{Recall, Remember};
        let reg = ToolRegistry::new().with(Remember).with(Recall);
        let et = EnableTools::from_registry(&reg);
        let keys: Vec<&str> = et.catalog.iter().map(|t| t.key.as_str()).collect();
        // `remember` + `recall` share the `memory` toggle key.
        assert_eq!(keys, vec!["memory"]);
    }

    #[test]
    fn schema_lists_keys_in_description_without_an_enum() {
        let reg = ToolRegistry::new().with(FetchUrl);
        let et = EnableTools::from_registry(&reg);
        let def = et.schema();
        assert!(def.function.description.contains("fetch_url"));
        // No `enum` constraint: a guided-decoding backend must be able to emit
        // a per-user `mcp__*` key that isn't in the static catalog.
        assert!(
            def.function.parameters["properties"]["keys"]["items"]
                .get("enum")
                .is_none(),
            "items must not constrain to an enum"
        );
    }

    #[tokio::test]
    async fn accepts_an_mcp_connector_key_not_in_the_static_catalog() {
        // Per-user MCP connector keys aren't in the registry snapshot, but the
        // model must still be able to enable a connected integration.
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        let reg = ToolRegistry::new().with(FetchUrl);
        let et = EnableTools::from_registry(&reg);
        let out = et
            .run(
                ctx(pool.clone(), Some("s1".into())).await,
                json!({"keys": ["mcp__gitlab"]}),
            )
            .await
            .unwrap();
        let enabled: Vec<&str> = out["enabled"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(enabled, vec!["mcp__gitlab"]);
        let on = crate::server::db::chat_session_tools::enabled_keys_for_session(&pool, "s1")
            .await
            .unwrap();
        assert!(on.contains("mcp__gitlab"));
    }

    #[tokio::test]
    async fn run_writes_a_row_for_each_known_key() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        let reg = ToolRegistry::new().with(FetchUrl).with(SearchWeb);
        let et = EnableTools::from_registry(&reg);
        let out = et
            .run(
                ctx(pool.clone(), Some("s1".into())).await,
                json!({"keys": ["fetch_url", "search_web"]}),
            )
            .await
            .unwrap();
        let enabled = out["enabled"].as_array().unwrap();
        assert_eq!(enabled.len(), 2);
        let on = crate::server::db::chat_session_tools::enabled_keys_for_session(&pool, "s1")
            .await
            .unwrap();
        assert!(on.contains("fetch_url"));
        assert!(on.contains("search_web"));
    }

    #[tokio::test]
    async fn run_refuses_without_a_session() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        let reg = ToolRegistry::new().with(FetchUrl);
        let et = EnableTools::from_registry(&reg);
        let err = et
            .run(ctx(pool, None).await, json!({"keys": ["fetch_url"]}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)), "{err:?}");
    }

    #[tokio::test]
    async fn unknown_keys_land_in_skipped_not_enabled() {
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        let reg = ToolRegistry::new().with(FetchUrl);
        let et = EnableTools::from_registry(&reg);
        let out = et
            .run(
                ctx(pool, Some("s1".into())).await,
                json!({"keys": ["fetch_url", "bogus"]}),
            )
            .await
            .unwrap();
        let enabled: Vec<&str> = out["enabled"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(enabled, vec!["fetch_url"]);
        let skipped = out["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["key"], "bogus");
    }

    #[test]
    fn schema_name_matches_id() {
        let reg = ToolRegistry::new();
        let et = EnableTools::from_registry(&reg);
        assert_eq!(et.id(), et.schema().function.name);
    }

    #[tokio::test]
    async fn allowed_tools_for_session_yields_only_bootstrap_before_any_enable() {
        // The "everything is lazy" invariant: a fresh session sees just
        // `enable_tools` in its tools array, until the model (or the
        // driver's auto-enable path) writes a chat_session_tools row.
        use crate::server::config::Config;
        use crate::server::rbac::Resolver;
        use crate::server::rbac::config::{RbacConfig, RoleConfig};
        use crate::server::state::AppState;
        use crate::server::upstreams::UpstreamRegistry;
        let pool = db::open(std::path::Path::new(":memory:")).await.unwrap();
        seed_session(&pool, "s1").await;
        let reg =
            ToolRegistry::new()
                .with(FetchUrl)
                .with(SearchWeb)
                .with(EnableTools::from_registry(
                    &ToolRegistry::new().with(FetchUrl).with(SearchWeb),
                ));
        let config = Config {
            rbac: RbacConfig {
                default_role: Some("user".into()),
                mappings: vec![],
            },
            roles: vec![RoleConfig {
                id: "user".into(),
                admin: false,
                tools: vec!["*".into()],
                models: vec!["*".into()],
                skills: vec![],
            }],
            ..Config::default()
        };
        let rbac = Resolver::build(config.rbac.clone(), config.roles.clone()).unwrap();
        let upstreams = UpstreamRegistry::new(&config.upstream_pools).unwrap();
        let state = AppState::new(
            config,
            pool.clone(),
            upstreams,
            std::sync::Arc::new(reg),
            std::sync::Arc::new(rbac),
        );

        // Fresh session — only the bootstrap should be allowed.
        let allowed = state
            .allowed_tools_for_session(&["user".into()], "u1", "s1")
            .await;
        assert_eq!(allowed, vec!["enable_tools".to_string()]);

        // After the model enables `fetch_url`, both bootstrap and the
        // newly-enabled tool surface — bootstrap first (cache-stable
        // prefix).
        crate::server::db::chat_session_tools::set(&pool, "s1", "fetch_url", true, "model")
            .await
            .unwrap();
        let allowed = state
            .allowed_tools_for_session(&["user".into()], "u1", "s1")
            .await;
        assert_eq!(
            allowed,
            vec!["enable_tools".to_string(), "fetch_url".into()]
        );
    }
}
