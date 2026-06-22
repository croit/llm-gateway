// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Tool injection + tool-call loop for `/v1/chat/completions`.
//!
//! Algorithm (also described in docs/tools-rbac.md):
//! 1. Compute the user's allowed-tool set (intersect role grants with the
//!    tool registry).
//! 2. If empty, skip everything — let the proxy stream the body through.
//! 3. Otherwise:
//!    - Parse the request body; **union** the user's tool defs with any
//!      `tools` the client sent (de-dupe by `function.name`).
//!    - Forward the modified body to the upstream **non-streaming**, so we
//!      can inspect `tool_calls`.
//!    - If the response carries `tool_calls` for gateway-registered tools
//!      *and only those*: execute them concurrently (bounded), append
//!      `role: "tool"` messages, and loop.
//!    - If the turn carries any client-owned tool_call (a tool the client
//!      supplied, or any name we don't own), return the final response
//!      as-is so the client executes its tools and re-submits. This holds
//!      even when the same turn also called a gateway tool: the client
//!      owns the message history on this path, so we can't run ours and
//!      yield mid-turn without dropping or orphaning the client's calls.
//!    - If no tool_calls at all, return the final assistant message.
//! 4. Hard bound: [`MAX_TOOL_ROUNDS`].
//!
//! Streaming caveat: this path always returns non-streaming. If the client
//! requested `stream: true` and the user has any allowed tools, we still
//! produce a JSON response. Re-issuing the final round with `stream: true` is
//! a follow-up. Documented in roadmap.md.

use std::sync::Arc;

use rama::bytes::Bytes;
use serde_json::{Value, json};

use crate::server::tools::{Tool, ToolContext, ToolError, ToolSource};

/// Hard cap on tool-call rounds per turn — the single source of truth shared
/// by every tool loop (this buffered runner, the chat-UI driver, and the `/v1`
/// streaming proxy) so the caps can't silently diverge again.
pub const MAX_TOOL_ROUNDS: u32 = 16;
const PER_REQUEST_TOOL_CONCURRENCY: usize = 4;
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Tool-result context budget (mirrors Anthropic's tool-result clearing:
/// trigger on size, keep the recent few, stub older ones re-callably).
/// Only kicks in once the cumulative `role:"tool"` content exceeds this, so
/// short conversations keep the full history and the prompt cache intact
/// (clearing invalidates the cached prefix).
const TOOL_OUTPUT_BUDGET: usize = 128 * 1024;
/// When evicting, keep the last N tool results verbatim.
const TOOL_OUTPUT_KEEP_FULL: usize = 3;
/// Only stub older tool results bigger than this. Set above the sandbox
/// preview size so a small `{preview, full_output_ref}` result is never
/// stubbed (which would drop the ref it carries).
const TOOL_OUTPUT_STUB_THRESHOLD: usize = 8192;

#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error("malformed chat-completion request body: {0}")]
    MalformedRequest(String),
    #[error("upstream returned malformed JSON: {0}")]
    MalformedUpstream(String),
    #[error("upstream HTTP error: {0}")]
    Upstream(String),
    #[error("tool-call loop exhausted after {0} rounds")]
    LoopExhausted(u32),
}

/// Outcome of [`run_with_tools`].
#[derive(Debug)]
pub struct LoopOutput {
    /// The final JSON body to send to the client. Already serialised.
    pub body: Bytes,
    /// HTTP status to relay. Always 200 in the happy path.
    pub status: u16,
    /// Number of gateway-tool rounds executed (0 when the model returned no
    /// gateway tool_calls). Useful for audit logs + tests.
    pub rounds: u32,
}

/// Runs the chat-completion request with tool injection + the gateway-tool
/// execution loop. `upstream` is a callback that forwards one round to the
/// LLM and returns the response body bytes + status. It accepts an opaque
/// model string so the caller can rotate backends per round via the
/// `UpstreamRegistry`.
pub async fn run_with_tools<F, Fut>(
    tools: &dyn ToolSource,
    allowed_tools: &[String],
    ctx: &ToolContext,
    mut request_body: Value,
    upstream: F,
) -> Result<LoopOutput, LoopError>
where
    F: Fn(Value) -> Fut,
    Fut: std::future::Future<Output = Result<(u16, Bytes), LoopError>>,
{
    inject_tools(&mut request_body, tools, allowed_tools)?;
    // Tool-call inspection requires the full response body — force
    // non-streaming on the wire even if the client asked for
    // stream:true. `stream_options` only makes sense with stream:true
    // and vLLM hard-rejects the combination otherwise, so drop it
    // here in lockstep with the override.
    let obj = request_body
        .as_object_mut()
        .ok_or_else(|| LoopError::MalformedRequest("body is not a JSON object".into()))?;
    obj.insert("stream".into(), Value::Bool(false));
    obj.remove("stream_options");

    let mut rounds = 0u32;
    loop {
        if rounds > MAX_TOOL_ROUNDS {
            return Err(LoopError::LoopExhausted(MAX_TOOL_ROUNDS));
        }

        let (status, body_bytes) = upstream(request_body.clone()).await?;
        if status >= 400 {
            // Upstream error: just relay.
            return Ok(LoopOutput {
                body: body_bytes,
                status,
                rounds,
            });
        }

        let response: Value = serde_json::from_slice(&body_bytes)
            .map_err(|e| LoopError::MalformedUpstream(e.to_string()))?;

        // Split the response's tool_calls into "owned by us" vs "owned by the
        // client". Only the first choice is considered — multi-choice with
        // tools is vanishingly rare and complicates the loop pointlessly.
        let split = split_tool_calls(&response, tools);

        // Stop the loop when there's nothing of ours to run, OR when the
        // turn also calls client-supplied tools. The client owns the
        // conversation history on the proxy path: it re-sends every
        // message each request. We can therefore run a turn entirely
        // server-side (looping until the model produces a final answer)
        // *only* when that turn calls our tools and ours alone. The moment
        // a turn mixes in a client-owned call we must hand the whole
        // assistant message back so the client executes its tools and
        // re-submits — running ours and yielding mid-turn would either
        // drop the client's calls or leave them unanswered in the next
        // upstream round (which the upstream rejects). Mixed turns are
        // rare; this keeps the wire valid at the cost of not executing our
        // tool in that one turn (the model re-emits it on the next).
        if split.gateway_owned.is_empty() || split.has_client_tool_calls {
            // Either the model returned a normal assistant message, the
            // tool_calls belong to client-supplied tools, or the turn
            // mixes both. Hand it back to the client.
            return Ok(LoopOutput {
                body: serde_json::to_vec(&response)
                    .map(Bytes::from)
                    .map_err(|e| LoopError::MalformedUpstream(e.to_string()))?,
                status,
                rounds,
            });
        }

        // Execute gateway-owned tool calls concurrently.
        let tool_results = execute_tool_calls(tools, ctx, &split.gateway_owned).await;

        // Append the assistant's tool-call message + the tool results to the
        // request's messages for the next round.
        append_round_to_messages(
            &mut request_body,
            &split.assistant_message,
            &split.gateway_owned,
            &tool_results,
        )?;

        // Cap the cumulative context cost of accumulated tool results across
        // rounds: once over budget, keep the last few verbatim and stub older
        // large ones.
        enforce_tool_output_budget(
            &mut request_body,
            TOOL_OUTPUT_BUDGET,
            TOOL_OUTPUT_KEEP_FULL,
            TOOL_OUTPUT_STUB_THRESHOLD,
        );

        rounds += 1;
    }
}

pub(crate) fn inject_tools(
    body: &mut Value,
    tools: &dyn ToolSource,
    allowed_tools: &[String],
) -> Result<(), LoopError> {
    if allowed_tools.is_empty() {
        return Ok(());
    }
    let defs = tools.defs_for(allowed_tools);
    if defs.is_empty() {
        return Ok(());
    }
    let obj = body
        .as_object_mut()
        .ok_or_else(|| LoopError::MalformedRequest("body is not a JSON object".into()))?;

    let mut tools: Vec<Value> = match obj.get("tools") {
        Some(Value::Array(existing)) => existing.clone(),
        Some(_) => {
            return Err(LoopError::MalformedRequest(
                "`tools` field is present but not an array".into(),
            ));
        }
        None => Vec::new(),
    };
    let mut existing_names: std::collections::HashSet<String> = tools
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_owned)
        })
        .collect();
    for def in defs {
        if existing_names.insert(def.function.name.clone()) {
            tools.push(serde_json::to_value(def).expect("ToolDef serializes"));
        }
    }
    // Visibility for the tool-context-optimization work: how big is the tool
    // block we're actually sending, and which tools. The byte count is a
    // direct proxy for the token cost the model pays per turn.
    let tools_bytes = serde_json::to_vec(&tools).map(|v| v.len()).unwrap_or(0);
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
        .collect();
    tracing::info!(
        tool_count = tools.len(),
        tools_bytes,
        ?names,
        "inject_tools: tool block sent upstream"
    );
    obj.insert("tools".into(), Value::Array(tools));
    Ok(())
}

struct ToolCallSplit {
    /// The full assistant message that triggered the tool calls (we append
    /// it verbatim to the message history for the next round).
    assistant_message: Value,
    /// Tool calls whose `function.name` is in the registry — we run these.
    gateway_owned: Vec<ToolCallRef>,
    /// True when the turn also carries a tool_call for a tool the gateway
    /// does *not* own (a client-supplied tool). Signals `run_with_tools`
    /// to stop the loop and hand the turn back to the client — see the
    /// loop body for why we can't both run our tools and yield mid-turn.
    has_client_tool_calls: bool,
}

#[derive(Clone)]
pub(crate) struct ToolCallRef {
    pub id: String,
    pub name: String,
    pub arguments_raw: String,
}

fn split_tool_calls(response: &Value, tools: &dyn ToolSource) -> ToolCallSplit {
    let mut gateway_owned = Vec::new();
    let mut has_client_tool_calls = false;
    let assistant_message = response
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("message"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    if let Some(tool_calls) = assistant_message
        .get("tool_calls")
        .and_then(|v| v.as_array())
    {
        for tc in tool_calls {
            let Some(function) = tc.get("function") else {
                continue;
            };
            let Some(name) = function.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            if !tools.contains(name) {
                // A tool_call we don't own. On the proxy merge path this is
                // the client's own tool (it brought a `tools` array we
                // unioned ours into); flag it so the loop yields the turn
                // back to the client. It can also be a hallucinated /
                // parser-munged name (dots → underscores and similar) —
                // we register tool IDs in OpenAI's function-name regex
                // (`^[a-zA-Z0-9_-]{1,64}$`), but logging here keeps future
                // parser divergences diagnosable. Either way it isn't ours
                // to run, and either way the safe move is to stop looping
                // and let the client deal with it.
                has_client_tool_calls = true;
                tracing::debug!(
                    wire_name = %name,
                    known = ?tools.ids(),
                    "upstream emitted tool_call we don't own; yielding turn to client"
                );
                continue;
            }
            let id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // OpenAI's spec says `arguments` is a JSON-encoded string,
            // but several real parsers (and at least one vLLM tool-call
            // template) emit it as a structured JSON object instead.
            // Accept both — if it's a string we use it directly; if
            // it's any other JSON value we re-serialise to a string so
            // the downstream `serde_json::from_str` in
            // `execute_tool_calls` still parses cleanly.
            let arguments_raw = match function.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "{}".to_string(),
            };
            gateway_owned.push(ToolCallRef {
                id,
                name: name.to_string(),
                arguments_raw,
            });
        }
    }

    ToolCallSplit {
        assistant_message,
        gateway_owned,
        has_client_tool_calls,
    }
}

pub(crate) async fn execute_tool_calls(
    tools: &dyn ToolSource,
    ctx: &ToolContext,
    calls: &[ToolCallRef],
) -> Vec<ToolResultRecord> {
    let sem = Arc::new(tokio::sync::Semaphore::new(PER_REQUEST_TOOL_CONCURRENCY));
    let futs = calls.iter().map(|call| {
        let sem = sem.clone();
        let call = call.clone();
        let tool: Option<Arc<dyn Tool>> = tools.get(&call.name);
        let ctx = ctx.clone();
        async move {
            let _permit = sem.acquire().await.expect("semaphore not closed");
            let Some(tool) = tool else {
                return ToolResultRecord {
                    call_id: call.id,
                    body: error_to_tool_message(&format!(
                        "tool `{name}` is no longer registered",
                        name = call.name
                    )),
                };
            };
            let args: Value = serde_json::from_str(&call.arguments_raw)
                .unwrap_or(Value::Object(Default::default()));
            // Trace each tool call with timing + the args we sent. Lets
            // operators grep the journal when a specific tool (e.g.
            // search_web against the brave API) hangs — the
            // `started`/`completed`/`timed out` triplet bounds the
            // wall-clock cost server-side.
            let started = std::time::Instant::now();
            tracing::info!(
                tool = %call.name,
                user = %ctx.user_id,
                args = %truncate_for_log(&call.arguments_raw),
                "tool call started"
            );
            // Most tools finish well within TOOL_TIMEOUT; a few (the sandbox
            // family) declare a longer ceiling via `max_duration`.
            let tool_timeout = tool.max_duration().unwrap_or(TOOL_TIMEOUT);
            let outcome = tokio::time::timeout(tool_timeout, tool.run(ctx, args)).await;
            let elapsed_ms = started.elapsed().as_millis();
            let body = match outcome {
                Ok(Ok(value)) => {
                    tracing::info!(
                        tool = %call.name,
                        elapsed_ms,
                        "tool call completed"
                    );
                    value
                }
                Ok(Err(ToolError::InvalidArgs(m))) => {
                    tracing::warn!(
                        tool = %call.name,
                        elapsed_ms,
                        error = %m,
                        "tool rejected arguments"
                    );
                    error_to_tool_message(&format!("invalid arguments: {m}"))
                }
                Ok(Err(ToolError::Failed(m))) => {
                    tracing::warn!(
                        tool = %call.name,
                        elapsed_ms,
                        error = %m,
                        "tool failed"
                    );
                    error_to_tool_message(&m)
                }
                Err(_) => {
                    tracing::warn!(
                        tool = %call.name,
                        elapsed_ms,
                        timeout_secs = tool_timeout.as_secs(),
                        "tool timed out"
                    );
                    error_to_tool_message(&format!(
                        "tool execution timed out after {tool_timeout:?}"
                    ))
                }
            };
            ToolResultRecord {
                call_id: call.id,
                body,
            }
        }
    });
    rama::futures::future::join_all(futs).await
}

/// Clip a raw-JSON args string for safe inclusion in a tracing line.
/// Keeps the head readable, drops anything past 200 bytes. We don't
/// strip newlines — `tracing`'s structured-output handles them.
/// Slice boundary is rolled back to the previous char boundary so
/// non-ASCII input (e.g. UTF-8 letters with an `ß` straddling the
/// cut point) doesn't panic on `str::index`.
fn truncate_for_log(s: &str) -> String {
    const MAX_BYTES: usize = 200;
    if s.len() <= MAX_BYTES {
        return s.to_string();
    }
    let mut end = MAX_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} chars)", &s[..end], s.len())
}

fn error_to_tool_message(message: &str) -> Value {
    json!({ "error": message })
}

pub(crate) struct ToolResultRecord {
    pub call_id: String,
    pub body: Value,
}

fn append_round_to_messages(
    request: &mut Value,
    assistant_message: &Value,
    calls: &[ToolCallRef],
    results: &[ToolResultRecord],
) -> Result<(), LoopError> {
    let obj = request
        .as_object_mut()
        .ok_or_else(|| LoopError::MalformedRequest("body is not a JSON object".into()))?;
    let messages = obj
        .get_mut("messages")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| LoopError::MalformedRequest("missing `messages` array".into()))?;

    messages.push(assistant_message.clone());

    // For each gateway tool_call we executed, emit a matching role:"tool"
    // message. OpenAI's contract: each tool_call_id must be answered.
    // If the result body is a `tool_content_parts(...)` envelope, the
    // content goes upstream as an array of typed parts (so a tool can
    // return e.g. an image_url back to the model); otherwise we
    // stringify the JSON into a plain content string.
    for call in calls {
        let body = results
            .iter()
            .find(|r| r.call_id == call.id)
            .map(|r| &r.body);
        let content = match body {
            Some(b) => match super::extract_content_parts(b) {
                Some(parts) => Value::Array(parts.clone()),
                None => Value::String(serde_json::to_string(b).unwrap_or_else(|_| "{}".into())),
            },
            None => Value::String("{}".into()),
        };
        messages.push(json!({
            "role": "tool",
            "tool_call_id": call.id,
            "content": content,
        }));
    }
    Ok(())
}

/// Once the cumulative `role:"tool"` content exceeds `budget`, keep the last
/// `keep_full` results verbatim and replace the *content* of older, large ones
/// with a short re-callable stub — preserving each message and its
/// `tool_call_id` so the tool_call ↔ result pairing is never orphaned
/// (upstreams reject an orphaned tool result). Bounds prompt growth without
/// touching short conversations (which keeps the prompt cache warm). A
/// stubbed result's `full_output_ref` (if any) is carried into the stub so the
/// model can still `read_sandbox_output` it. Only string contents are stubbed
/// (array/`tool_content_parts` results, e.g. inline images, are left intact).
fn enforce_tool_output_budget(
    request: &mut Value,
    budget: usize,
    keep_full: usize,
    stub_threshold: usize,
) {
    let Some(messages) = request.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return;
    };
    let content_len = |m: &Value| -> usize {
        m.get("content")
            .and_then(|c| c.as_str())
            .map(str::len)
            .unwrap_or(0)
    };
    let tool_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .map(|(i, _)| i)
        .collect();
    if tool_idxs.len() <= keep_full {
        return;
    }
    // Trigger only when we're actually over budget.
    let total: usize = tool_idxs.iter().map(|&i| content_len(&messages[i])).sum();
    if total <= budget {
        return;
    }
    let stub_until = tool_idxs.len() - keep_full;
    for &i in &tool_idxs[..stub_until] {
        let len = content_len(&messages[i]);
        if len <= stub_threshold {
            continue;
        }
        let ref_hint = messages[i]
            .get("content")
            .and_then(|c| c.as_str())
            .map(output_ref_hint)
            .unwrap_or_default();
        messages[i]["content"] = Value::String(format!(
            "[earlier tool output cleared to save context ({len} chars). Re-run the tool to \
             regenerate it{ref_hint}.]"
        ));
    }
}

/// If a stubbed tool result carried `full_output_ref`(s) (the sandbox preview
/// shape), surface them so the model can still retrieve the output after
/// eviction. Returns e.g. ` — or read it with read_sandbox_output id="t/x.txt"`.
fn output_ref_hint(content: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(content) else {
        return String::new();
    };
    let mut refs: Vec<&str> = ["stdout", "stderr"]
        .iter()
        .filter_map(|k| {
            v.get(k)
                .and_then(|s| s.get("full_output_ref"))
                .and_then(|r| r.as_str())
        })
        .collect();
    refs.dedup();
    match refs.as_slice() {
        [] => String::new(),
        ids => format!(
            " — or read it with read_sandbox_output ({})",
            ids.iter()
                .map(|id| format!("id=\"{id}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tools::ToolRegistry;
    use crate::server::tools::echo::Echo;
    use crate::server::tools::time::CurrentTimestamp;

    async fn ctx() -> ToolContext {
        let pool = crate::server::db::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
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

    fn registry() -> ToolRegistry {
        ToolRegistry::new().with(Echo).with(CurrentTimestamp)
    }

    #[test]
    fn truncate_for_log_handles_multibyte_boundary() {
        // Crafted so the 200-byte cut lands inside a UTF-8 ß
        // (`ß` = 2 bytes). Earlier slicing version panicked here
        // when the model sent a German letter through typst_letter.
        let mut s = "a".repeat(199);
        s.push('ß'); // bytes 199..201
        s.push_str(&"b".repeat(50));
        let out = truncate_for_log(&s);
        assert!(out.starts_with(&"a".repeat(199)));
        assert!(out.contains("…"));
    }

    #[test]
    fn truncate_for_log_passthrough_under_cap() {
        let s = "short and sweet";
        assert_eq!(truncate_for_log(s), s);
    }

    #[test]
    fn inject_tools_appends_to_empty_array() {
        let reg = registry();
        let mut body = json!({"model": "x", "messages": []});
        inject_tools(&mut body, &reg, &["company_echo".into()]).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "company_echo");
    }

    #[test]
    fn inject_tools_unions_with_client_supplied() {
        let reg = registry();
        let mut body = json!({
            "model": "x",
            "messages": [],
            "tools": [
                {"type": "function", "function": {"name": "client.tool", "description": "x", "parameters": {}}}
            ]
        });
        inject_tools(&mut body, &reg, &["company_echo".into()]).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"client.tool"));
        assert!(names.contains(&"company_echo"));
    }

    #[test]
    fn inject_tools_dedupes_when_client_supplied_same_name() {
        let reg = registry();
        let mut body = json!({
            "model": "x",
            "messages": [],
            "tools": [
                {"type": "function", "function": {"name": "company_echo", "description": "x", "parameters": {}}}
            ]
        });
        inject_tools(&mut body, &reg, &["company_echo".into()]).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn inject_tools_noop_when_no_allowed_tools() {
        let reg = registry();
        let mut body = json!({"model": "x", "messages": []});
        inject_tools(&mut body, &reg, &[]).unwrap();
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn split_tool_calls_separates_gateway_owned() {
        let reg = registry();
        let response = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [
                        {"id": "c1", "type": "function", "function": {"name": "company_echo", "arguments": "{\"message\":\"hi\"}"}},
                        {"id": "c2", "type": "function", "function": {"name": "client.tool", "arguments": "{}"}}
                    ]
                }
            }]
        });
        let split = split_tool_calls(&response, &reg);
        assert_eq!(split.gateway_owned.len(), 1);
        assert_eq!(split.gateway_owned[0].name, "company_echo");
        assert_eq!(split.gateway_owned[0].id, "c1");
        // The turn also called a tool we don't own → flagged so the loop
        // yields back to the client.
        assert!(split.has_client_tool_calls);
    }

    #[test]
    fn split_tool_calls_no_client_flag_when_all_gateway_owned() {
        let reg = registry();
        let response = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [
                        {"id": "c1", "type": "function", "function": {"name": "company_echo", "arguments": "{\"message\":\"hi\"}"}},
                        {"id": "c2", "type": "function", "function": {"name": "get_current_timestamp", "arguments": "{}"}}
                    ]
                }
            }]
        });
        let split = split_tool_calls(&response, &reg);
        assert_eq!(split.gateway_owned.len(), 2);
        assert!(!split.has_client_tool_calls);
    }

    #[test]
    fn split_tool_calls_empty_when_response_has_none() {
        let reg = registry();
        let response = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "hello"}
            }]
        });
        let split = split_tool_calls(&response, &reg);
        assert!(split.gateway_owned.is_empty());
    }

    #[tokio::test]
    async fn execute_tool_calls_runs_echo() {
        let reg = registry();
        let calls = vec![ToolCallRef {
            id: "c1".into(),
            name: "company_echo".into(),
            arguments_raw: "{\"message\":\"yo\"}".into(),
        }];
        let results = execute_tool_calls(&reg, &ctx().await, &calls).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].call_id, "c1");
        assert_eq!(results[0].body["message"], "yo");
    }

    #[tokio::test]
    async fn execute_tool_calls_captures_invalid_args() {
        let reg = registry();
        let calls = vec![ToolCallRef {
            id: "c1".into(),
            name: "company_echo".into(),
            arguments_raw: "not json".into(),
        }];
        let results = execute_tool_calls(&reg, &ctx().await, &calls).await;
        // serde_json::from_str fails → we fall back to {} args → Echo rejects
        // the missing `message`. Tool error appears as "error" in body.
        assert!(results[0].body.get("error").is_some());
    }

    #[tokio::test]
    async fn run_with_tools_loops_until_no_gateway_tool_calls() {
        let reg = registry();
        let ctx = ctx().await;
        let request = json!({
            "model": "x",
            "messages": [{"role": "user", "content": "what's the time?"}],
            "stream": true
        });

        // Upstream: round 0 returns a tool_call for get_current_timestamp;
        // round 1 returns a final assistant message.
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = counter.clone();
        let upstream = move |body: Value| {
            let counter = counter_clone.clone();
            async move {
                // Body is forced to non-streaming inside the loop:
                assert_eq!(body["stream"], false);
                let round = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = match round {
                    0 => json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "tool_calls": [{
                                    "id": "call_0",
                                    "type": "function",
                                    "function": {"name": "get_current_timestamp", "arguments": "{}"}
                                }]
                            }
                        }]
                    }),
                    _ => json!({
                        "choices": [{
                            "message": {"role": "assistant", "content": "it is now"}
                        }]
                    }),
                };
                Ok::<_, LoopError>((200, Bytes::from(serde_json::to_vec(&response).unwrap())))
            }
        };

        let out = run_with_tools(
            &reg,
            &["get_current_timestamp".into()],
            &ctx,
            request,
            upstream,
        )
        .await
        .unwrap();
        assert_eq!(out.rounds, 1);
        assert_eq!(out.status, 200);
        let body: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "it is now");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn run_with_tools_yields_to_client_on_mixed_turn() {
        // A single turn that calls BOTH a gateway tool and a client-owned
        // tool must NOT loop server-side: the client owns the history and
        // has to run its own tool. We hand the whole assistant turn back
        // unchanged (rounds == 0) so the client sees both tool_calls.
        let reg = registry();
        let ctx = ctx().await;
        let request = json!({
            "model": "x",
            "messages": [{"role": "user", "content": "search then call my tool"}],
            "tools": [{"type": "function", "function": {"name": "client_tool", "description": "x", "parameters": {}}}]
        });
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_clone = calls.clone();
        let upstream = move |_body: Value| {
            let calls = calls_clone.clone();
            async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "tool_calls": [
                                {"id": "g1", "type": "function", "function": {"name": "company_echo", "arguments": "{\"message\":\"hi\"}"}},
                                {"id": "c1", "type": "function", "function": {"name": "client_tool", "arguments": "{}"}}
                            ]
                        }
                    }]
                });
                Ok::<_, LoopError>((200, Bytes::from(serde_json::to_vec(&response).unwrap())))
            }
        };
        let out = run_with_tools(&reg, &["company_echo".into()], &ctx, request, upstream)
            .await
            .unwrap();
        // No tool round ran, and the upstream was hit exactly once.
        assert_eq!(out.rounds, 0);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Both tool_calls survive in the body we return to the client.
        let body: Value = serde_json::from_slice(&out.body).unwrap();
        let tcs = body["choices"][0]["message"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(tcs.len(), 2);
    }

    #[tokio::test]
    async fn run_with_tools_returns_passthrough_when_no_gateway_tool_calls() {
        let reg = registry();
        let ctx = ctx().await;
        let request = json!({
            "model": "x",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let upstream = |_body: Value| async {
            let response = json!({
                "choices": [{"message": {"role": "assistant", "content": "hello"}}]
            });
            Ok::<_, LoopError>((200, Bytes::from(serde_json::to_vec(&response).unwrap())))
        };
        let out = run_with_tools(&reg, &["company_echo".into()], &ctx, request, upstream)
            .await
            .unwrap();
        assert_eq!(out.rounds, 0);
    }

    #[tokio::test]
    async fn run_with_tools_relays_upstream_4xx_without_looping() {
        let reg = registry();
        let ctx = ctx().await;
        let request = json!({"model": "x", "messages": []});
        let upstream = |_body: Value| async {
            Ok::<_, LoopError>((429, Bytes::from(r#"{"error":{"message":"rate limit"}}"#)))
        };
        let out = run_with_tools(&reg, &["company_echo".into()], &ctx, request, upstream)
            .await
            .unwrap();
        assert_eq!(out.status, 429);
        assert_eq!(out.rounds, 0);
    }

    #[tokio::test]
    async fn run_with_tools_loop_exhausted_after_max_rounds() {
        let reg = registry();
        let ctx = ctx().await;
        let request = json!({"model": "x", "messages": []});
        // Always return a tool_call → guaranteed infinite loop, MAX_ROUNDS
        // breaks it.
        let upstream = |_body: Value| async {
            let response = json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "x",
                            "type": "function",
                            "function": {"name": "company_echo", "arguments": "{\"message\":\"loop\"}"}
                        }]
                    }
                }]
            });
            Ok::<_, LoopError>((200, Bytes::from(serde_json::to_vec(&response).unwrap())))
        };
        let err = run_with_tools(&reg, &["company_echo".into()], &ctx, request, upstream)
            .await
            .unwrap_err();
        assert!(matches!(err, LoopError::LoopExhausted(_)), "{err:?}");
    }

    fn body_with_tool_results(contents: &[(&str, String)]) -> Value {
        let mut messages = vec![json!({"role": "user", "content": "hi"})];
        for (id, content) in contents {
            messages.push(json!({"role": "assistant", "tool_calls": [{"id": id}]}));
            messages.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
        }
        json!({ "messages": messages })
    }

    #[test]
    fn budget_stubs_old_tool_results_keeps_recent_and_pairing() {
        let big = "x".repeat(10_000);
        let mut body = body_with_tool_results(&[
            ("a", big.clone()),
            ("b", big.clone()),
            ("c", big.clone()),
            ("d", big.clone()),
        ]);
        // total 40 KB > budget 4 KB → triggers; keep last 3 → only "a" stubbed.
        enforce_tool_output_budget(&mut body, 4096, 3, 4096);
        let msgs = body["messages"].as_array().unwrap();
        let tool = |id: &str| {
            msgs.iter()
                .find(|m| m["tool_call_id"] == json!(id))
                .unwrap()
        };
        assert!(
            tool("a")["content"]
                .as_str()
                .unwrap()
                .contains("cleared to save context")
        );
        assert_eq!(tool("a")["tool_call_id"], json!("a")); // pairing intact
        for id in ["b", "c", "d"] {
            assert_eq!(tool(id)["content"], json!(big), "{id} kept verbatim");
        }
    }

    #[test]
    fn budget_noop_under_budget_even_with_many_results() {
        // 4 small results (8 KB total) under a 128 KB budget → no eviction, so
        // the prompt cache stays intact.
        let small = "y".repeat(2_000);
        let mut body = body_with_tool_results(&[
            ("a", small.clone()),
            ("b", small.clone()),
            ("c", small.clone()),
            ("d", small.clone()),
        ]);
        enforce_tool_output_budget(&mut body, 128 * 1024, 3, 4096);
        let msgs = body["messages"].as_array().unwrap();
        let a = msgs
            .iter()
            .find(|m| m["tool_call_id"] == json!("a"))
            .unwrap();
        assert_eq!(a["content"], json!(small), "not stubbed under budget");
    }

    #[test]
    fn budget_stub_preserves_output_ref() {
        // A sandbox preview result carries full_output_ref; eviction must keep
        // the ref so the model can still read it.
        let with_ref = json!({
            "stdout": {"preview": "x".repeat(9_000), "full_output_ref": "t-1/stdout.txt"}
        })
        .to_string();
        let other = "z".repeat(9_000);
        let mut body = body_with_tool_results(&[
            ("a", with_ref),
            ("b", other.clone()),
            ("c", other.clone()),
            ("d", other.clone()),
        ]);
        enforce_tool_output_budget(&mut body, 4096, 3, 4096);
        let stub = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["tool_call_id"] == json!("a"))
            .unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(stub.contains("read_sandbox_output"), "{stub}");
        assert!(stub.contains("t-1/stdout.txt"), "{stub}");
    }
}
