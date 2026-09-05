// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Anthropic `/v1/messages` request → OpenAI `/v1/chat/completions` request.
//!
//! The interesting asymmetries, and what we do about each:
//!
//! | Anthropic | OpenAI | Note |
//! |---|---|---|
//! | top-level `system` (string or block array) | a leading `role:"system"` message | block arrays are joined with a blank line |
//! | a mid-conversation `role: "system"` message | folded into that same leading system message | see below |
//! | `tool_result` blocks inside a **user** message | one `role:"tool"` message per block | they must precede any remaining user content |
//! | `tool_use` blocks inside an **assistant** message | `tool_calls[]` with stringified `arguments` | |
//! | `thinking` / `redacted_thinking` blocks | *(dropped)* | an OpenAI backend rejects them; see below |
//! | `input_schema` on a tool | `function.parameters` | |
//! | `tool_choice: {type:"any"}` | `tool_choice: "required"` | |
//! | `stop_sequences` | `stop` | |
//! | `thinking: {...}` request field | a backend-specific reasoning param | via [`crate::server::reasoning`], never forwarded verbatim |
//! | `cache_control`, `output_config`, `context_management`, `metadata`, `container`, `mcp_servers` | *(dropped)* | bridging these is the gateway's job — forwarding them 400s a vLLM backend |
//!
//! **Thinking blocks are dropped on the way in, deliberately.** We synthesise
//! them on the way out from the backend's `reasoning_content` (see
//! [`super::response`]), which means the `signature` we attach is ours, not a
//! real Anthropic signature. Round-tripping is safe precisely because we throw
//! the block away again when the client echoes it back.
//!
//! **Every system message is hoisted to the front.** Anthropic lets an
//! operator append a `role: "system"` entry *inside* `messages` mid-conversation
//! — Claude Code uses it for the agent-type roster — and the OpenAI schema has
//! a `system` role too, so the naive mapping is one-to-one in place. It doesn't
//! survive contact with a real backend: Qwen's chat template rejects the result
//! outright (`400 System message must be at the beginning.`), which is every
//! Claude Code request failing. So all system content — the top-level `system`
//! field and every mid-conversation entry, in order — is joined into the single
//! leading system message that every chat template accepts. The instruction
//! still applies to the request that carried it, which is what it is for; only
//! its position is lost.
//!
//! **Unknown fields are dropped, not rejected.** Claude Code gains request
//! fields with every release and sends them to any base URL it is pointed at;
//! a gateway that 400s on the first unrecognised field breaks on the next
//! client update. Everything not explicitly mapped here is simply not
//! forwarded.

use serde_json::{Map, Value, json};

use crate::server::reasoning::Effort;

/// A request we could not translate. Surfaces as `400 invalid_request_error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslateError(pub String);

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TranslateError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// The translated request plus the facts about it the handler needs before it
/// can route: which model was asked for, whether the caller wants SSE, and
/// what reasoning effort the `thinking` / `output_config` fields imply.
#[derive(Debug, Clone)]
pub struct TranslatedRequest {
    /// The OpenAI-shaped body to send upstream.
    pub body: Value,
    /// The model id exactly as the client asked for it (may be an alias).
    pub model: String,
    /// `stream: true` in the Anthropic request.
    pub stream: bool,
    /// Reasoning effort implied by `thinking` / `output_config.effort`, or
    /// `None` when the request said nothing about thinking — in which case we
    /// leave the backend's reasoning parameters alone entirely, matching what
    /// `/v1/chat/completions` does for a client that sets none.
    pub effort: Option<Effort>,
}

/// Translate an Anthropic Messages request body.
pub fn to_openai(req: &Value) -> Result<TranslatedRequest, TranslateError> {
    let obj = req
        .as_object()
        .ok_or_else(|| TranslateError::new("request body must be a JSON object"))?;

    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| TranslateError::new("request body is missing a string `model` field"))?;

    let stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);

    // Every piece of system content, in the order it appeared, hoisted into
    // one leading message (see the module docs for why).
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(system) = obj.get("system")
        && let Some(text) = system_text(system)
        && !text.is_empty()
    {
        system_parts.push(text);
    }

    let mut messages: Vec<Value> = Vec::new();
    let input = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| TranslateError::new("request body is missing a `messages` array"))?;
    for (i, message) in input.iter().enumerate() {
        translate_message(message, &mut messages, &mut system_parts)
            .map_err(|e| TranslateError::new(format!("messages[{i}]: {e}")))?;
    }
    if !system_parts.is_empty() {
        messages.insert(
            0,
            json!({"role": "system", "content": system_parts.join("\n\n")}),
        );
    }

    let mut out = Map::new();
    out.insert("model".into(), Value::String(model.clone()));
    out.insert("messages".into(), Value::Array(messages));
    out.insert("stream".into(), Value::Bool(stream));

    // Anthropic requires `max_tokens`; OpenAI treats it as optional. Pass it
    // through when sane and drop a nonsense value rather than 400 on it —
    // the upstream's own ceiling is the real constraint.
    if let Some(max) = obj.get("max_tokens").and_then(Value::as_i64)
        && max > 0
    {
        out.insert("max_tokens".into(), json!(max));
    }
    for key in ["temperature", "top_p"] {
        if let Some(v) = obj.get(key).filter(|v| v.is_number()) {
            out.insert(key.into(), v.clone());
        }
    }
    // `top_k` has no place in the OpenAI schema and a strict backend 400s on
    // it. Claude Code never sends it; drop it if some other client does.
    if let Some(stop) = obj.get("stop_sequences").and_then(Value::as_array)
        && !stop.is_empty()
    {
        out.insert("stop".into(), Value::Array(stop.clone()));
    }
    if let Some(user) = obj
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        out.insert("user".into(), Value::String(user.to_string()));
    }

    let tools = translate_tools(obj.get("tools"))?;
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = translate_tool_choice(obj.get("tool_choice")) {
        out.insert("tool_choice".into(), choice);
    }
    if obj
        .get("tool_choice")
        .and_then(|c| c.get("disable_parallel_tool_use"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        out.insert("parallel_tool_calls".into(), Value::Bool(false));
    }

    Ok(TranslatedRequest {
        body: Value::Object(out),
        model,
        stream,
        effort: effort_for(obj),
    })
}

/// Flatten a top-level `system` field: a bare string, or an array of blocks
/// of which we keep the `text` ones (`cache_control` markers ride along on
/// those blocks and are simply not copied).
///
/// Claude Code puts an attribution block first in this array. We keep it —
/// it is a few tokens of prompt, and rewriting or reordering the array is
/// exactly what the client documentation warns gateways not to do.
fn system_text(system: &Value) -> Option<String> {
    match system {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n\n"))
        }
        _ => None,
    }
}

/// Translate one Anthropic message, appending one *or more* OpenAI messages.
///
/// One-to-many because a single Anthropic user message can carry several
/// `tool_result` blocks, and OpenAI wants one `role:"tool"` message each. A
/// system message appends to `system_parts` instead and produces no message
/// of its own — the caller hoists them all to the front.
fn translate_message(
    message: &Value,
    out: &mut Vec<Value>,
    system_parts: &mut Vec<String>,
) -> Result<(), String> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .ok_or("missing `role`")?;
    let content = message.get("content").unwrap_or(&Value::Null);

    match role {
        "assistant" => {
            out.push(assistant_message(content));
            Ok(())
        }
        // A mid-conversation operator instruction. It keeps the system role
        // — it just moves to the front, where a chat template will accept it.
        "system" => {
            if let Some(text) = system_text(content)
                && !text.is_empty()
            {
                system_parts.push(text);
            }
            Ok(())
        }
        "user" => {
            user_messages(content, out);
            Ok(())
        }
        other => Err(format!("unsupported role `{other}`")),
    }
}

/// Assistant turn: text blocks become the content string, `tool_use` blocks
/// become `tool_calls`, thinking blocks are dropped.
fn assistant_message(content: &Value) -> Value {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    match content {
        Value::String(s) => text.push_str(s),
        Value::Array(blocks) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                    Some("tool_use") => {
                        let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": input.to_string(),
                            }
                        }));
                    }
                    // `thinking`, `redacted_thinking`, and anything a newer
                    // client sends: not representable upstream, dropped.
                    _ => {}
                }
            }
        }
        _ => {}
    }

    // `content: null` alongside tool_calls is the shape the rest of the
    // gateway already replays upstream; an empty string is rejected by some
    // backends when tool_calls are present.
    let content = if text.is_empty() && !tool_calls.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    };
    let mut msg = json!({"role": "assistant", "content": content});
    if !tool_calls.is_empty() {
        msg["tool_calls"] = Value::Array(tool_calls);
    }
    msg
}

/// User turn: `tool_result` blocks become `role:"tool"` messages (emitted
/// first, so they directly follow the assistant message that called them),
/// and whatever is left becomes the user message.
fn user_messages(content: &Value, out: &mut Vec<Value>) {
    match content {
        Value::String(s) => {
            out.push(json!({"role": "user", "content": s}));
        }
        Value::Array(blocks) => {
            let mut parts: Vec<Value> = Vec::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        let id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": id,
                            "content": tool_result_text(block),
                        }));
                    }
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            parts.push(json!({"type": "text", "text": t}));
                        }
                    }
                    Some("image") => {
                        if let Some(url) = image_url(block.get("source")) {
                            parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                    }
                    // A PDF or other file the Anthropic format carries inline.
                    // An OpenAI-compatible backend has nowhere to put it, and
                    // dropping it silently is the worst outcome: the model is
                    // then asked about a document it was never shown and
                    // answers confidently from nothing. Say it is missing.
                    Some("document") => parts.push(json!({
                        "type": "text",
                        "text": omitted_document(block),
                    })),
                    _ => {}
                }
            }
            if parts.is_empty() {
                return;
            }
            // Collapse the common single-text case to a plain string: some
            // backends' chat templates only handle the array form for
            // multimodal input.
            if parts.len() == 1
                && let Some(text) = parts[0].get("text").and_then(Value::as_str)
            {
                out.push(json!({"role": "user", "content": text}));
            } else {
                out.push(json!({"role": "user", "content": parts}));
            }
        }
        _ => {}
    }
}

/// Flatten a `tool_result` block's content to the string an OpenAI `tool`
/// message carries. The content may be a bare string or an array of blocks;
/// images inside a tool result have no OpenAI equivalent and are announced
/// rather than silently dropped, so the model doesn't reason about output it
/// was never shown.
fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                    Some("text") => b.get("text").and_then(Value::as_str).map(str::to_owned),
                    Some("image") => Some(
                        "[image omitted: tool results cannot carry images on this backend]"
                            .to_string(),
                    ),
                    _ => None,
                })
                .collect();
            parts.join("\n")
        }
        // A tool that returned nothing still needs a message, or the upstream
        // rejects the unanswered tool_call.
        _ => String::new(),
    }
}

/// The stand-in text for a `document` block we can't forward, naming the file
/// so the model can say *what* it is missing rather than inventing content.
fn omitted_document(block: &Value) -> String {
    let title = block
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let media = block
        .pointer("/source/media_type")
        .and_then(Value::as_str)
        .unwrap_or("unknown type");
    match title {
        Some(title) => format!(
            "[document omitted: `{title}` ({media}) — this backend accepts text and images only]"
        ),
        None => format!("[document omitted: {media} — this backend accepts text and images only]"),
    }
}

/// `image` block source → an OpenAI `image_url` value (a `data:` URI for
/// base64 sources, the URL itself for URL sources).
fn image_url(source: Option<&Value>) -> Option<String> {
    let source = source?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            let data = source.get("data").and_then(Value::as_str)?;
            Some(format!("data:{media};base64,{data}"))
        }
        Some("url") => source.get("url").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

/// Anthropic tool definitions → OpenAI function definitions.
///
/// Anthropic-hosted server tools (`web_search_*`, `code_execution_*`,
/// `tool_search_tool_*`, …) are identified by a `type` field and cannot run
/// against our upstreams, so they are skipped: the model then simply never
/// sees them. Client-side tools carry `input_schema` and either no `type` or
/// `type: "custom"`.
fn translate_tools(tools: Option<&Value>) -> Result<Vec<Value>, TranslateError> {
    let Some(tools) = tools else {
        return Ok(Vec::new());
    };
    let Some(tools) = tools.as_array() else {
        return Err(TranslateError::new("`tools` must be an array"));
    };
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        match tool.get("type").and_then(Value::as_str) {
            None | Some("custom") => {}
            // A server-tool type we can't serve.
            Some(_) => continue,
        }
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        let mut function = Map::new();
        function.insert("name".into(), Value::String(name.to_string()));
        if let Some(desc) = tool.get("description").and_then(Value::as_str) {
            function.insert("description".into(), Value::String(desc.to_string()));
        }
        function.insert(
            "parameters".into(),
            tool.get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        );
        out.push(json!({"type": "function", "function": function}));
    }
    Ok(out)
}

/// `tool_choice` object → the OpenAI spelling. Returns `None` when there is
/// nothing to say (absent, or a shape we don't recognise).
fn translate_tool_choice(choice: Option<&Value>) -> Option<Value> {
    let choice = choice?;
    match choice.get("type").and_then(Value::as_str)? {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => {
            let name = choice.get("name").and_then(Value::as_str)?;
            Some(json!({"type": "function", "function": {"name": name}}))
        }
        _ => None,
    }
}

/// Map the request's thinking configuration onto the gateway's own effort
/// knob, which [`crate::server::reasoning::apply_effort`] then translates into
/// whatever the serving backend actually understands.
///
/// `output_config.effort` wins when present because it is the more specific
/// statement; otherwise the presence and mode of `thinking` decides. A
/// request that mentions neither returns `None` — meaning "don't touch the
/// backend's reasoning parameters", which is how `/v1/chat/completions`
/// already behaves.
fn effort_for(obj: &Map<String, Value>) -> Option<Effort> {
    if let Some(level) = obj
        .get("output_config")
        .and_then(|c| c.get("effort"))
        .and_then(Value::as_str)
    {
        return Some(match level {
            "low" => Effort::Fast,
            "medium" => Effort::Standard,
            "xhigh" | "max" => Effort::Max,
            // "high" and anything newer.
            _ => Effort::Deep,
        });
    }
    match obj.get("thinking")?.get("type").and_then(Value::as_str) {
        Some("disabled") => Some(Effort::Fast),
        // "adaptive", "enabled", or a mode we don't know yet: thinking on.
        Some(_) => Some(Effort::Standard),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(req: Value) -> TranslatedRequest {
        to_openai(&req).expect("translates")
    }

    #[test]
    fn a_minimal_request_becomes_a_chat_completion() {
        let out = translate(json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(out.model, "claude-sonnet-4-6");
        assert!(!out.stream);
        assert_eq!(out.body["model"], "claude-sonnet-4-6");
        assert_eq!(out.body["max_tokens"], 1024);
        assert_eq!(out.body["messages"][0]["role"], "user");
        assert_eq!(out.body["messages"][0]["content"], "hi");
    }

    #[test]
    fn a_missing_model_is_a_translate_error() {
        let err = to_openai(&json!({"messages": []})).unwrap_err();
        assert!(err.0.contains("model"), "{}", err.0);
    }

    #[test]
    fn system_blocks_join_into_one_leading_system_message() {
        let out = translate(json!({
            "model": "m",
            "max_tokens": 16,
            "system": [
                {"type": "text", "text": "You are Claude Code."},
                {"type": "text", "text": "Be terse.", "cache_control": {"type": "ephemeral"}},
            ],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(out.body["messages"][0]["role"], "system");
        assert_eq!(
            out.body["messages"][0]["content"],
            "You are Claude Code.\n\nBe terse."
        );
        // The cache_control marker never reaches the upstream.
        assert!(!out.body.to_string().contains("cache_control"));
    }

    #[test]
    fn a_string_system_prompt_works_too() {
        let out = translate(json!({
            "model": "m", "max_tokens": 16,
            "system": "be brief",
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(out.body["messages"][0]["content"], "be brief");
    }

    #[test]
    fn tool_use_and_tool_result_round_trip_into_openai_shape() {
        let out = translate(json!({
            "model": "m",
            "max_tokens": 64,
            "messages": [
                {"role": "user", "content": "read foo.rs"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Let me look."},
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "foo.rs"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "fn main() {}"},
                ]},
            ],
        }));
        let msgs = out.body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], "Let me look.");
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "Read");
        // Arguments are the stringified `input` object.
        assert_eq!(
            msgs[1]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"foo.rs"}"#
        );
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "toolu_1");
        assert_eq!(msgs[2]["content"], "fn main() {}");
    }

    #[test]
    fn several_tool_results_precede_the_remaining_user_text() {
        let out = translate(json!({
            "model": "m", "max_tokens": 64,
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": "one"},
                {"type": "tool_result", "tool_use_id": "b", "content": [
                    {"type": "text", "text": "two"},
                ]},
                {"type": "text", "text": "now continue"},
            ]}],
        }));
        let msgs = out.body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["tool_call_id"], "a");
        assert_eq!(msgs[1]["tool_call_id"], "b");
        assert_eq!(msgs[1]["content"], "two");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"], "now continue");
    }

    #[test]
    fn an_empty_tool_result_still_answers_the_call() {
        let out = translate(json!({
            "model": "m", "max_tokens": 8,
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a"},
            ]}],
        }));
        assert_eq!(out.body["messages"][0]["role"], "tool");
        assert_eq!(out.body["messages"][0]["content"], "");
    }

    #[test]
    fn assistant_thinking_blocks_are_dropped_on_the_way_in() {
        let out = translate(json!({
            "model": "m", "max_tokens": 64,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "gw"},
                    {"type": "redacted_thinking", "data": "xx"},
                    {"type": "text", "text": "answer"},
                ]},
            ],
        }));
        assert_eq!(out.body["messages"][0]["content"], "answer");
        assert!(!out.body.to_string().contains("hmm"));
    }

    #[test]
    fn an_assistant_turn_with_only_tool_calls_sends_null_content() {
        let out = translate(json!({
            "model": "m", "max_tokens": 64,
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t", "name": "Bash", "input": {}},
            ]}],
        }));
        assert!(out.body["messages"][0]["content"].is_null());
    }

    /// A document has nowhere to go on an OpenAI-compatible backend. Dropping
    /// it silently leaves the model answering about a file it never saw.
    #[test]
    fn a_document_block_is_announced_rather_than_dropped() {
        let out = translate(json!({
            "model": "m", "max_tokens": 64,
            "messages": [{"role": "user", "content": [
                {"type": "document", "title": "contract.pdf",
                 "source": {"type": "base64", "media_type": "application/pdf", "data": "AAAA"}},
                {"type": "text", "text": "summarise it"},
            ]}],
        }));
        let parts = out.body["messages"][0]["content"].as_array().unwrap();
        let note = parts[0]["text"].as_str().unwrap();
        assert!(note.contains("document omitted"), "{note}");
        assert!(note.contains("contract.pdf"), "{note}");
        assert!(note.contains("application/pdf"), "{note}");
        assert_eq!(parts[1]["text"], "summarise it");
        // The base64 payload itself is not smuggled into the prompt.
        assert!(!out.body.to_string().contains("AAAA"));
    }

    #[test]
    fn an_untitled_document_still_names_its_type() {
        let out = translate(json!({
            "model": "m", "max_tokens": 64,
            "messages": [{"role": "user", "content": [
                {"type": "document", "source": {"type": "text", "media_type": "text/plain"}},
            ]}],
        }));
        let note = out.body["messages"][0]["content"].as_str().unwrap();
        assert!(note.contains("text/plain"), "{note}");
    }

    #[test]
    fn images_become_data_uris() {
        let out = translate(json!({
            "model": "m", "max_tokens": 64,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                {"type": "text", "text": "what is this"},
            ]}],
        }));
        let parts = out.body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(parts[1]["text"], "what is this");
    }

    #[test]
    fn tools_become_openai_functions_and_server_tools_are_skipped() {
        let out = translate(json!({
            "model": "m", "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "Read", "description": "read a file",
                 "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}},
                {"type": "web_search_20260209", "name": "web_search"},
                {"type": "custom", "name": "Write", "input_schema": {"type": "object"}},
            ],
        }));
        let tools = out.body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "Read");
        assert_eq!(tools[0]["function"]["description"], "read a file");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        assert_eq!(tools[1]["function"]["name"], "Write");
    }

    #[test]
    fn tool_choice_spellings_map_across() {
        let choice = |v: Value| {
            translate(json!({
                "model": "m", "max_tokens": 8,
                "messages": [{"role": "user", "content": "hi"}],
                "tool_choice": v,
            }))
            .body
            .get("tool_choice")
            .cloned()
        };
        assert_eq!(choice(json!({"type": "auto"})), Some(json!("auto")));
        assert_eq!(choice(json!({"type": "any"})), Some(json!("required")));
        assert_eq!(choice(json!({"type": "none"})), Some(json!("none")));
        assert_eq!(
            choice(json!({"type": "tool", "name": "Bash"})),
            Some(json!({"type": "function", "function": {"name": "Bash"}}))
        );
    }

    #[test]
    fn disabling_parallel_tool_use_carries_over() {
        let out = translate(json!({
            "model": "m", "max_tokens": 8,
            "messages": [{"role": "user", "content": "hi"}],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
        }));
        assert_eq!(out.body["parallel_tool_calls"], false);
    }

    #[test]
    fn stop_sequences_and_sampling_carry_over() {
        let out = translate(json!({
            "model": "m", "max_tokens": 8,
            "messages": [{"role": "user", "content": "hi"}],
            "stop_sequences": ["END"],
            "temperature": 0.5,
            "top_p": 0.9,
            "top_k": 40,
            "metadata": {"user_id": "u-1"},
        }));
        assert_eq!(out.body["stop"], json!(["END"]));
        assert_eq!(out.body["temperature"], 0.5);
        assert_eq!(out.body["top_p"], 0.9);
        assert_eq!(out.body["user"], "u-1");
        // top_k has no OpenAI equivalent and a strict backend rejects it.
        assert!(out.body.get("top_k").is_none());
    }

    #[test]
    fn fields_only_anthropic_understands_are_dropped_not_rejected() {
        let out = translate(json!({
            "model": "m", "max_tokens": 8,
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"type": "adaptive"},
            "context_management": {"edits": [{"type": "clear_tool_uses_20250919"}]},
            "output_config": {"format": {"type": "json_schema"}},
            "mcp_servers": [{"type": "url", "url": "https://example.com"}],
            "container": {"skills": []},
            "some_field_from_next_year": true,
        }));
        let body = out.body.to_string();
        for gone in [
            "thinking",
            "context_management",
            "output_config",
            "mcp_servers",
            "container",
            "some_field_from_next_year",
        ] {
            assert!(!body.contains(gone), "`{gone}` reached the upstream body");
        }
    }

    #[test]
    fn thinking_and_effort_map_onto_the_gateway_effort_knob() {
        let effort = |v: Value| {
            let mut req = json!({
                "model": "m", "max_tokens": 8,
                "messages": [{"role": "user", "content": "hi"}],
            });
            if let Some(o) = req.as_object_mut()
                && let Some(vo) = v.as_object()
            {
                for (k, val) in vo {
                    o.insert(k.clone(), val.clone());
                }
            }
            translate(req).effort
        };
        assert_eq!(effort(json!({})), None);
        assert_eq!(
            effort(json!({"thinking": {"type": "adaptive"}})),
            Some(Effort::Standard)
        );
        assert_eq!(
            effort(json!({"thinking": {"type": "disabled"}})),
            Some(Effort::Fast)
        );
        assert_eq!(
            effort(json!({"thinking": {"type": "enabled", "budget_tokens": 4096}})),
            Some(Effort::Standard)
        );
        assert_eq!(
            effort(json!({"output_config": {"effort": "low"}})),
            Some(Effort::Fast)
        );
        assert_eq!(
            effort(json!({"output_config": {"effort": "max"}})),
            Some(Effort::Max)
        );
        // output_config wins over thinking: it is the more specific statement.
        assert_eq!(
            effort(json!({"thinking": {"type": "disabled"}, "output_config": {"effort": "high"}})),
            Some(Effort::Deep)
        );
    }

    #[test]
    fn streaming_is_reported_and_forwarded() {
        let out = translate(json!({
            "model": "m", "max_tokens": 8, "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert!(out.stream);
        assert_eq!(out.body["stream"], true);
    }

    /// Claude Code sends exactly this shape — the user turn, then a
    /// `role: "system"` entry carrying the agent-type roster. Left in place it
    /// is `400 System message must be at the beginning.` from the backend's
    /// chat template, on every single request.
    #[test]
    fn a_mid_conversation_system_message_is_hoisted_to_the_front() {
        let out = translate(json!({
            "model": "m", "max_tokens": 8,
            "system": [{"type": "text", "text": "You are Claude Code."}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "system", "content": [{"type": "text", "text": "operator note"}]},
            ],
        }));
        let msgs = out.body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "the system entry must not stay in place");
        assert_eq!(msgs[0]["role"], "system");
        // Top-level system first, then the mid-conversation one, in order.
        assert_eq!(msgs[0]["content"], "You are Claude Code.\n\noperator note");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hi");
    }

    /// No system message may follow a non-system one, whatever the input.
    #[test]
    fn system_is_always_the_only_leading_message() {
        let out = translate(json!({
            "model": "m", "max_tokens": 8,
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "system", "content": "a"},
                {"role": "assistant", "content": "two"},
                {"role": "system", "content": "b"},
                {"role": "user", "content": "three"},
            ],
        }));
        let roles: Vec<&str> = out.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
        assert_eq!(out.body["messages"][0]["content"], "a\n\nb");
    }

    /// A conversation with no system content at all gains no system message.
    #[test]
    fn no_system_content_means_no_system_message() {
        let out = translate(json!({
            "model": "m", "max_tokens": 8,
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(out.body["messages"][0]["role"], "user");
    }

    #[test]
    fn an_unknown_role_is_rejected_with_the_message_index() {
        let err = to_openai(&json!({
            "model": "m", "max_tokens": 8,
            "messages": [{"role": "user", "content": "a"}, {"role": "wizard", "content": "b"}],
        }))
        .unwrap_err();
        assert!(err.0.contains("messages[1]"), "{}", err.0);
        assert!(err.0.contains("wizard"), "{}", err.0);
    }
}
