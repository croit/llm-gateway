// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! MCP (Model Context Protocol) client bridge.
//!
//! Lets the gateway act as an MCP *host*: connect to external MCP servers
//! declared in `[mcp]` config (over stdio — a spawned subprocess — or
//! streamable HTTP), enumerate their tools, and surface each one to the
//! model as an ordinary [`Tool`] in the registry. Tool calls the model
//! emits are routed back over MCP via [`rmcp`]; everything downstream (the
//! tool runner, RBAC, the `/tools` toggles, result feedback) treats them
//! exactly like a built-in tool.
//!
//! Adding an integration is then config, not code: point the gateway at any
//! MCP server and its tools show up in chat. The one piece of plumbing that
//! lives here is the adapter — id namespacing, schema mapping, and turning
//! an MCP `CallToolResult` (text / image / structured content) back into the
//! gateway's tool-result shape.
//!
//! Each tool's name is namespaced `mcp__<server>__<tool>` so two servers
//! can't collide and MCP tools stay visually distinct from built-ins. The
//! original (un-namespaced) name is what we actually call on the server.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::serve_client;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Value, json};
use shared::api::ToolDef;

use super::{Tool, ToolContext, ToolError, ToolFuture, ToolResult, tool_content_parts};
use crate::server::config::{McpConfig, McpServerConfig};

pub mod manager;
pub mod worker;

/// Prefix every bridged tool id carries, so the catalog can group them and
/// the registry can't confuse them with a built-in. Matches the parsing in
/// `catalog::entry_key_for`.
pub const MCP_ID_PREFIX: &str = "mcp__";

/// How long to wait for one server to connect + enumerate its tools at
/// startup before giving up on it. Non-fatal: boot continues without that
/// server (mirrors the typst-discovery / OIDC-retry "never block boot" rule).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-call ceiling. Kept under the tool runner's own per-tool timeout so a
/// wedged MCP server surfaces as a tool error the model can see, rather than
/// being force-cancelled mid-round.
const CALL_TIMEOUT: Duration = Duration::from_secs(25);

/// A live session to one MCP server. The `Arc` each [`McpTool`] holds keeps
/// it (and its background transport task) alive for the life of the registry
/// — i.e. the process. Dropping the last reference closes the connection.
pub(crate) struct McpConnection {
    name: String,
    service: RunningService<RoleClient, ()>,
}

/// One MCP-server tool, adapted to the gateway's [`Tool`] trait.
pub struct McpTool {
    /// Namespaced id = registry key = OpenAI function name the model calls.
    registry_id: String,
    /// The server's own tool name — what we pass to `tools/call`.
    remote_name: String,
    schema: ToolDef,
    conn: Arc<McpConnection>,
    /// MCP `readOnlyHint` annotation, when the server provided one. Drives the
    /// default permission tier in the per-user connector store.
    read_only: bool,
    /// MCP `destructiveHint` annotation. Together with `read_only` it sets the
    /// default tier: destructive (and not read-only) → `ask`, everything else
    /// → `always` (so a connected connector's read/query tools work in chat
    /// without the user pre-authorizing each one).
    destructive: bool,
}

impl McpTool {
    /// The server's own (un-namespaced) tool name.
    pub fn remote_name(&self) -> &str {
        &self.remote_name
    }

    /// The model-facing definition.
    pub fn def(&self) -> &ToolDef {
        &self.schema
    }

    /// Whether the server marked this tool read-only.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Whether the server marked this tool destructive.
    pub fn destructive(&self) -> bool {
        self.destructive
    }
}

impl Tool for McpTool {
    fn id(&self) -> &str {
        &self.registry_id
    }

    fn schema(&self) -> ToolDef {
        self.schema.clone()
    }

    fn run<'a>(&'a self, _ctx: ToolContext, args: Value) -> ToolFuture<'a> {
        // Own everything the call needs so the future doesn't borrow `self`
        // across the await (and the `Arc` keeps the session alive regardless).
        let conn = self.conn.clone();
        let remote = self.remote_name.clone();
        Box::pin(async move {
            let mut params = CallToolRequestParams::new(remote.clone());
            // The model's tool-call arguments are already a JSON object;
            // a non-object (or absent) becomes "no arguments".
            params.arguments = args.as_object().cloned();

            let res = tokio::time::timeout(CALL_TIMEOUT, conn.service.call_tool(params))
                .await
                .map_err(|_| {
                    ToolError::Failed(format!(
                        "MCP tool `{remote}` on `{}` timed out after {}s",
                        conn.name,
                        CALL_TIMEOUT.as_secs()
                    ))
                })?
                .map_err(|e| {
                    ToolError::Failed(format!(
                        "MCP call `{remote}` on `{}` failed: {e}",
                        conn.name
                    ))
                })?;
            map_call_result(res)
        })
    }
}

/// Connect to every enabled server in `cfg`, concurrently and non-fatally,
/// and return the flattened set of bridged tools. A server that fails to
/// connect (or times out) is logged and skipped — it never blocks boot.
pub async fn connect_all(cfg: &McpConfig) -> Vec<McpTool> {
    let attempts = cfg.servers.iter().filter_map(|server| {
        if !server.enabled {
            tracing::info!(server = %server.name, "MCP server disabled in config — skipping");
            return None;
        }
        Some(connect_server(server))
    });

    let results = rama::futures::future::join_all(attempts).await;
    let mut tools = Vec::new();
    for r in results {
        match r {
            Ok(mut t) => tools.append(&mut t),
            Err((name, err)) => tracing::warn!(
                server = %name, error = %err,
                "MCP server connect failed — its tools won't be available this run"
            ),
        }
    }
    tools
}

/// Connect one server with a bounded timeout. `Err((name, reason))` on any
/// failure so the caller can log which server dropped out.
async fn connect_server(server: &McpServerConfig) -> Result<Vec<McpTool>, (String, String)> {
    match tokio::time::timeout(CONNECT_TIMEOUT, connect_and_list(server)).await {
        Ok(Ok(tools)) => Ok(tools),
        Ok(Err(reason)) => Err((server.name.clone(), reason)),
        Err(_) => Err((
            server.name.clone(),
            format!("timed out after {}s", CONNECT_TIMEOUT.as_secs()),
        )),
    }
}

/// Establish the session, list its tools, and build one [`McpTool`] each.
async fn connect_and_list(server: &McpServerConfig) -> Result<Vec<McpTool>, String> {
    let service = connect_transport(server).await?;
    let conn = Arc::new(McpConnection {
        name: server.name.clone(),
        service,
    });

    let remote_tools = conn
        .service
        .list_all_tools()
        .await
        .map_err(|e| format!("tools/list failed: {e}"))?;

    let out = build_tools(&server.name, &conn, remote_tools, Some(server));
    tracing::info!(server = %server.name, tools = out.len(), "connected MCP server");
    Ok(out)
}

/// Build [`McpTool`]s from a connection's listed tools. `server` (when set)
/// supplies the description fallback for the boot/config path; per-user
/// connections pass `None` and fall back to a generic line.
fn build_tools(
    name: &str,
    conn: &Arc<McpConnection>,
    remote_tools: Vec<rmcp::model::Tool>,
    server: Option<&McpServerConfig>,
) -> Vec<McpTool> {
    let mut out = Vec::with_capacity(remote_tools.len());
    let mut seen = HashSet::new();
    for t in remote_tools {
        let registry_id = sanitize_tool_id(name, &t.name);
        if !seen.insert(registry_id.clone()) {
            tracing::warn!(
                server = %name, tool = %t.name, id = %registry_id,
                "MCP tool id collides after sanitization — skipping"
            );
            continue;
        }
        let description = match server {
            Some(s) => tool_description(s, &t),
            None => t
                .description
                .as_deref()
                .filter(|d| !d.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    t.title
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| format!("`{}` tool from the `{name}` MCP server.", t.name)),
        };
        let read_only = t
            .annotations
            .as_ref()
            .and_then(|a| a.read_only_hint)
            .unwrap_or(false);
        let destructive = t
            .annotations
            .as_ref()
            .and_then(|a| a.destructive_hint)
            .unwrap_or(false);
        let parameters = Value::Object((*t.input_schema).clone());
        let schema = ToolDef::function(registry_id.clone(), description, parameters);
        out.push(McpTool {
            registry_id,
            remote_name: t.name.to_string(),
            schema,
            conn: conn.clone(),
            read_only,
            destructive,
        });
    }
    out
}

/// A live per-user connection to a remote (HTTP) MCP server, plus its tools.
pub(crate) struct ConnectedServer {
    pub conn: Arc<McpConnection>,
    pub tools: Vec<McpTool>,
}

/// Connect to a remote streamable-HTTP MCP server with an optional bearer
/// token (the user's OAuth access token), and list its tools. Used by the
/// per-user [`manager::McpConnectionManager`]; the bearer is the only thing
/// that differs from the operator/boot path.
pub(crate) async fn connect_http_server(
    name: &str,
    url: &str,
    bearer: Option<&str>,
) -> Result<ConnectedServer, String> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    if let Some(token) = bearer {
        config = config.auth_header(token.to_string());
    }
    let transport = StreamableHttpClientTransport::from_config(config);
    let service = tokio::time::timeout(CONNECT_TIMEOUT, serve_client((), transport))
        .await
        .map_err(|_| format!("MCP handshake to `{name}` timed out"))?
        .map_err(|e| format!("MCP handshake to `{name}` failed: {e}"))?;
    let conn = Arc::new(McpConnection {
        name: name.to_string(),
        service,
    });
    let remote_tools = tokio::time::timeout(CONNECT_TIMEOUT, conn.service.list_all_tools())
        .await
        .map_err(|_| format!("tools/list on `{name}` timed out"))?
        .map_err(|e| format!("tools/list on `{name}` failed: {e}"))?;
    let tools = build_tools(name, &conn, remote_tools, None);
    Ok(ConnectedServer { conn, tools })
}

/// Open the transport the config asks for and run the MCP handshake. Exactly
/// one of `command` (stdio) / `url` (http) must be set.
async fn connect_transport(
    server: &McpServerConfig,
) -> Result<RunningService<RoleClient, ()>, String> {
    match (server.command.as_deref(), server.url.as_deref()) {
        (Some(command), None) => {
            // The child inherits the gateway's environment (so secrets already
            // in the process env reach it); `env` adds/overrides on top.
            let args = server.args.clone();
            let env = server.env.clone();
            let transport =
                TokioChildProcess::new(tokio::process::Command::new(command).configure(|cmd| {
                    cmd.args(&args);
                    for (k, v) in &env {
                        cmd.env(k, v);
                    }
                }))
                .map_err(|e| format!("spawning `{command}`: {e}"))?;
            serve_client((), transport)
                .await
                .map_err(|e| format!("MCP handshake over stdio failed: {e}"))
        }
        (None, Some(url)) => {
            // Build the config with optional auth, then let rmcp's default
            // client drive it (`from_config` uses a reqwest client with idle
            // pooling disabled — suited to long-lived SSE). We don't build the
            // client ourselves: rmcp pins a different reqwest major than the
            // gateway, so its `from_config` is the seam that stays version-safe.
            let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
            // rmcp sends this via reqwest's `bearer_auth`, i.e. as
            // `Authorization: Bearer <value>` — so the env holds the raw token,
            // not the scheme.
            if let Some(token) = server.bearer_token() {
                config = config.auth_header(token);
            }
            if !server.headers.is_empty() {
                config = config.custom_headers(parse_headers(&server.headers)?);
            }
            let transport = StreamableHttpClientTransport::from_config(config);
            serve_client((), transport)
                .await
                .map_err(|e| format!("MCP handshake over http failed: {e}"))
        }
        (Some(_), Some(_)) => {
            Err("set either `command` (stdio) or `url` (http), not both".to_string())
        }
        (None, None) => Err("set `command` (stdio) or `url` (http)".to_string()),
    }
}

/// Validate + convert the configured `headers` map into typed HTTP headers.
/// A malformed name or value fails the whole server (logged + skipped) rather
/// than silently dropping a credential header.
fn parse_headers(
    headers: &HashMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, String> {
    let mut out = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        let header = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| format!("invalid header name `{name}`: {e}"))?;
        let val = HeaderValue::from_str(value)
            .map_err(|e| format!("invalid value for header `{name}`: {e}"))?;
        out.insert(header, val);
    }
    Ok(out)
}

/// Model-facing description for a bridged tool. MCP servers should ship one;
/// fall back to the title, then a generic line, so the schema is never blank.
fn tool_description(server: &McpServerConfig, t: &rmcp::model::Tool) -> String {
    if let Some(d) = t.description.as_deref().filter(|d| !d.is_empty()) {
        return d.to_string();
    }
    if let Some(title) = t.title.as_deref().filter(|s| !s.is_empty()) {
        return title.to_string();
    }
    format!("`{}` tool from the `{}` MCP server.", t.name, server.name)
}

/// Build the namespaced, OpenAI-function-name-safe id for a bridged tool.
/// `mcp__<server>__<tool>` with any out-of-charset byte replaced by `_`, and
/// truncated to the 64-char function-name limit (post-sanitize the string is
/// pure ASCII, so the cut is always on a char boundary).
fn sanitize_tool_id(server: &str, tool: &str) -> String {
    // Same charset the registry validates against, so a sanitized id can
    // never fail `ToolRegistry::with`'s assertion.
    let mut id: String = format!("{MCP_ID_PREFIX}{server}__{tool}")
        .chars()
        .map(|c| {
            if super::registry::is_openai_function_name_char(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    id.truncate(64);
    id
}

/// Turn an MCP `CallToolResult` into the gateway's tool-result `Value`.
///
/// - `is_error` → [`ToolError::Failed`] carrying any text the server returned.
/// - Any image content → the `tool_content_parts` envelope (text parts +
///   `image_url` data-URIs) so vision models actually see the image, the
///   same path `fetch_url` uses.
/// - Otherwise prefer `structured_content` (already JSON); else the joined
///   text, wrapped as a single text part so it lands unquoted.
///
/// Non-text/-image blocks (resources, audio, links) are serialised to JSON
/// text so nothing the server returned is silently dropped.
fn map_call_result(res: CallToolResult) -> ToolResult {
    let mut texts: Vec<String> = Vec::new();
    let mut images: Vec<Value> = Vec::new();
    for c in &res.content {
        // Match the content enum directly (via `Content`'s deref to
        // `RawContent`) rather than serialising every block just to read a
        // tag. Image data is already base64 (no re-encode).
        if let Some(text) = c.as_text() {
            texts.push(text.text.clone());
        } else if let Some(image) = c.as_image() {
            images.push(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{}", image.mime_type, image.data) },
            }));
        } else {
            // resource / audio / link / anything else → hand the model the
            // raw JSON so nothing the server returned is silently dropped.
            texts.push(serde_json::to_value(c).unwrap_or(Value::Null).to_string());
        }
    }

    if res.is_error == Some(true) {
        let msg = if texts.is_empty() {
            "MCP tool reported an error".to_string()
        } else {
            texts.join("\n")
        };
        return Err(ToolError::Failed(msg));
    }

    if !images.is_empty() {
        let mut parts: Vec<Value> = texts
            .into_iter()
            .map(|t| json!({ "type": "text", "text": t }))
            .collect();
        parts.extend(images);
        return Ok(tool_content_parts(parts));
    }

    if let Some(structured) = res.structured_content {
        return Ok(structured);
    }

    if !texts.is_empty() {
        return Ok(tool_content_parts(vec![json!({
            "type": "text",
            "text": texts.join("\n"),
        })]));
    }

    Ok(json!({ "ok": true, "note": "the tool returned no content" }))
}

#[cfg(test)]
mod tests {
    use super::{map_call_result, parse_headers, sanitize_tool_id};
    use crate::server::tools::{ToolError, extract_content_parts};
    use rmcp::model::{CallToolResult, Content};

    fn result(content: Vec<Content>, is_error: bool) -> CallToolResult {
        // `CallToolResult` is `#[non_exhaustive]` but derives `Default`, so
        // build via default + field assignment rather than a struct literal.
        let mut r = CallToolResult::default();
        r.content = content;
        r.is_error = Some(is_error);
        r
    }

    #[test]
    fn sanitize_namespaces_and_scrubs() {
        assert_eq!(
            sanitize_tool_id("gcal", "list_events"),
            "mcp__gcal__list_events"
        );
        // dots / slashes / spaces collapse to `_`.
        assert_eq!(
            sanitize_tool_id("g.cal", "list events"),
            "mcp__g_cal__list_events"
        );
    }

    #[test]
    fn sanitize_caps_at_64_chars() {
        let id = sanitize_tool_id("server", &"x".repeat(200));
        assert!(id.len() <= 64, "len was {}", id.len());
        assert!(id.starts_with("mcp__server__x"));
    }

    #[test]
    fn text_result_lands_as_unquoted_text_part() {
        let out = map_call_result(result(vec![Content::text("hello world")], false)).unwrap();
        let parts = extract_content_parts(&out).expect("text wrapped as content-parts");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "hello world");
    }

    #[test]
    fn image_result_becomes_image_url_part() {
        let out = map_call_result(result(
            vec![Content::image(
                "BASE64DATA".to_string(),
                "image/png".to_string(),
            )],
            false,
        ))
        .unwrap();
        let parts = extract_content_parts(&out).expect("image wrapped as content-parts");
        let url = parts.last().unwrap()["image_url"]["url"].as_str().unwrap();
        assert_eq!(url, "data:image/png;base64,BASE64DATA");
    }

    #[test]
    fn error_result_maps_to_tool_error_with_text() {
        let err = map_call_result(result(vec![Content::text("boom")], true)).unwrap_err();
        match err {
            ToolError::Failed(msg) => assert!(msg.contains("boom"), "{msg}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn parse_headers_accepts_valid_and_rejects_malformed() {
        let ok =
            std::collections::HashMap::from([("X-Api-Key".to_string(), "secret-123".to_string())]);
        let parsed = parse_headers(&ok).expect("valid header parses");
        assert_eq!(parsed.len(), 1);

        // A space isn't allowed in a header name — must error, not silently
        // drop a (possibly credential-bearing) header.
        let bad = std::collections::HashMap::from([("bad name".to_string(), "v".to_string())]);
        assert!(parse_headers(&bad).is_err());
    }
}
