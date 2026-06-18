// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Gateway-executed tools.
//!
//! Tools are async functions the gateway exposes to the LLM through the OpenAI
//! `tools` array on a chat completion. When the model emits `tool_calls`, the
//! gateway runs the corresponding tool, appends the result as a `role: "tool"`
//! message, and re-prompts the model. The loop runs until the model returns a
//! plain assistant message (or we hit the round bound — see `runner`).
//!
//! Adding a tool means writing code, registering it in `ToolRegistry`, and
//! granting it to one or more roles in `[rbac]`. We do **not** discover tools
//! at runtime.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;
use shared::api::ToolDef;
use thiserror::Error;

pub mod catalog;
pub mod currency;
pub mod echo;
pub mod enable_tools;
pub mod feedback;
pub mod fetch_attachment;
pub mod fetch_url;
pub mod location;
pub mod lookup_ip;
pub mod mcp;
pub mod memory;
pub mod netcheck;
pub mod rag;
pub mod registry;
pub mod runner;
pub mod search_web;
pub mod time;
pub mod typst_render;
pub mod upload_attachment;
pub mod wikipedia;

pub use registry::ToolRegistry;

/// Carried into each `Tool::run` invocation. Lets a tool read the
/// caller's identity + roles and reach the gateway's datastore
/// without us threading specific scalars (timezone, locale,
/// preferences) through the trait signature each time we add one.
#[derive(Clone)]
pub struct ToolContext {
    pub user_id: String,
    pub roles: Vec<String>,
    /// Handle to the gateway's SQLite pool. Tools that need anything
    /// from `users` / `sessions` / etc. query it here keyed by
    /// `user_id` (e.g. the time tool reads `users.timezone`).
    pub db: crate::server::db::Pool,
    /// S3 config for chat attachments. Threaded through so the
    /// `fetch_attachment` tool can resolve opaque attachment ids
    /// (`<turn_id>/<filename>`) back to the live bucket. `None` when
    /// the deployment hasn't configured `[chat.s3]` — the tool errs
    /// with `NotConfigured` in that case.
    pub s3: Option<std::sync::Arc<crate::server::config::S3Config>>,
    /// Id of the assistant turn the gateway is currently driving.
    /// `Some` only on the chat-page path (where a persistent
    /// `chat_turns` row exists); `None` on the proxy paths, which
    /// have no chat session to attach to. Tools that produce
    /// in-conversation side effects (`upload_attachment`) refuse to
    /// run when this is `None`.
    pub assistant_turn_id: Option<String>,
    /// Id of the chat session the current turn belongs to. `Some` only on
    /// the chat-page path; `None` on the proxy paths (no session there).
    /// Required by `enable_tools` to write per-conversation rows; tools
    /// that don't need it ignore the field.
    pub session_id: Option<String>,
    /// The caller's source IP (from `X-Forwarded-For` / `X-Real-IP`),
    /// when the request carried one. Used by `get_user_location` to
    /// resolve a coarse location via [`geoip`]. `None` for callers we
    /// can't attribute an IP to.
    pub client_ip: Option<String>,
    /// Client-IP → location resolver, cloned from `AppState`. `None`
    /// when `[geoip]` isn't configured. Carried here (rather than reached
    /// via global state) so tools stay a pure function of their context.
    pub geoip: Option<crate::server::geoip::GeoIp>,
    /// Handles for an interactive mid-turn browser prompt. `Some` only on
    /// the chat-page path (a live SSE turn the user is watching); `None`
    /// on proxy / bearer paths. `get_user_location` uses it to ask the
    /// browser for a precise position and wait for the reply.
    pub chat_feedback: Option<ChatFeedback>,
    /// Per-turn reservation set of filenames already claimed by an
    /// in-flight upload (typst, `upload_attachment`). Tool calls in
    /// one round run concurrently via `join_all`, so two parallel
    /// callers would both read the same pre-upload `content` and
    /// pick the same filename — the second `put_object` then
    /// overwrites the first. Holding the set behind a mutex inside
    /// `ToolContext` lets every uploader take the lock, see the
    /// committed *and* in-flight names together, and pick a unique
    /// suffix before releasing. `Some` only on the chat-page path
    /// (the only path with an `assistant_turn_id` to attach to);
    /// `None` on proxy / bearer paths.
    pub attachment_reservations:
        Option<std::sync::Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>>,
    /// The RAG indexer handle, cloned from `AppState`. Tools that hit
    /// the vector index (`rag_search`, `rag_list_collections`) reach it
    /// here. `None` on paths that never wired one up (notably the
    /// in-memory test scaffolding); the tools degrade to a clear
    /// `Failed("RAG not configured")` error in that case.
    pub indexer: Option<crate::server::rag::worker::Indexer>,
}

/// Chat-only handles a tool needs to run an interactive prompt while a
/// turn is streaming: push UI onto the live SSE stream (`broadcast`,
/// via `TurnUpdate::Inject`) and await the browser's reply (`hub`). Only
/// populated on the chat-page path — proxy / bearer callers have no
/// browser and no live turn to prompt against.
#[derive(Clone)]
pub struct ChatFeedback {
    pub broadcast: tokio::sync::broadcast::Sender<session_core::workers::TurnUpdate>,
    pub hub: std::sync::Arc<feedback::FeedbackHub>,
    /// Whether the browser is on a secure context (so `navigator.
    /// geolocation` is actually allowed). When false, the tool skips the
    /// futile precise-location prompt, warns inline, and falls back to
    /// GeoIP. Computed per-request from `X-Forwarded-Proto` / `Host` —
    /// see `geoip::transport_is_secure`.
    pub secure: bool,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Pool isn't `Debug`. Print the user/roles and elide the rest
        // so test failures stay readable.
        f.debug_struct("ToolContext")
            .field("user_id", &self.user_id)
            .field("roles", &self.roles)
            .field("db", &"<Pool>")
            .field("s3", &self.s3.as_ref().map(|_| "<S3Config>"))
            .field("assistant_turn_id", &self.assistant_turn_id)
            .field("session_id", &self.session_id)
            .field("client_ip", &self.client_ip)
            .field("geoip", &self.geoip.as_ref().map(|_| "<GeoIp>"))
            .field(
                "chat_feedback",
                &self.chat_feedback.as_ref().map(|_| "<ChatFeedback>"),
            )
            .field(
                "attachment_reservations",
                &self
                    .attachment_reservations
                    .as_ref()
                    .map(|_| "<Reservations>"),
            )
            .field("indexer", &self.indexer.as_ref().map(|_| "<Indexer>"))
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool arguments did not match the declared schema: {0}")]
    InvalidArgs(String),
    #[error("tool execution failed: {0}")]
    Failed(String),
}

pub type ToolResult = Result<Value, ToolError>;

/// `BoxFuture` alias — keeps each tool's `run` signature legible while staying
/// object-safe. Hand-rolled so we don't pull in `async_trait` for one trait.
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

/// Sentinel key that signals "the body of this tool result is not a
/// JSON object to stringify into `content`, but an array of OpenAI
/// content parts to splice in as-is". Used by `fetch_attachment` so
/// it can hand the model back an `image_url` part for binary image
/// attachments — OpenAI's Chat Completions API has supported tool
/// message `content` as an array of parts for a while now, and that's
/// the only path that actually gets image bytes back into the
/// conversation (tool results have no other channel for them).
///
/// Tools opt in by returning `Value::Object` of shape
/// `{ TOOL_CONTENT_PARTS_KEY: [ ...parts ] }`. The driver detects this
/// shape and emits `content: [..parts..]` instead of `content: "<json>"`.
pub const TOOL_CONTENT_PARTS_KEY: &str = "__gateway_tool_content_parts";

/// Helper for tools that want to return mixed text + image content
/// back to the model. The driver renders the parts as the tool
/// message's `content` array; the parts themselves are also what the
/// operator UI sees in the tool-call log (we persist the envelope
/// verbatim).
pub fn tool_content_parts(parts: Vec<Value>) -> Value {
    serde_json::json!({ TOOL_CONTENT_PARTS_KEY: parts })
}

/// Read the parts back out of a tool result body if the tool used
/// `tool_content_parts(...)`. Returns `None` for the JSON-stringified
/// path so the existing behavior stays unchanged.
pub fn extract_content_parts(body: &Value) -> Option<&Vec<Value>> {
    body.as_object()
        .filter(|m| m.len() == 1)
        .and_then(|m| m.get(TOOL_CONTENT_PARTS_KEY))
        .and_then(|v| v.as_array())
}

/// A registered, server-side tool.
///
/// Implementations are typically zero-sized or hold an `Arc<Service>`; cloning
/// them must be cheap because they're held in `Arc` by the registry.
pub trait Tool: Send + Sync + 'static {
    /// Stable identifier. Used as the OpenAI function name and as the lookup
    /// key in `ToolRegistry`. Must be unique across all registered tools.
    /// Convention: `"company.<area>.<verb>"`, e.g. `"company_wiki_search"`.
    fn id(&self) -> &str;

    /// The OpenAI-shaped definition the model sees. Generated fresh each call
    /// so the schema can interpolate runtime context if needed (most don't).
    fn schema(&self) -> ToolDef;

    /// Executes the tool. Returns a JSON value that's stringified into the
    /// `role: "tool"` message we send back to the model in the next round.
    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parts_envelope_round_trips_through_extract() {
        let body = tool_content_parts(vec![
            json!({"type": "text", "text": "hi"}),
            json!({"type": "image_url", "image_url": {"url": "https://e/x.png"}}),
        ]);
        let parts = extract_content_parts(&body).expect("parts envelope detected");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
    }

    #[test]
    fn extract_returns_none_for_plain_json_body() {
        // A regular tool result must not be misinterpreted as parts —
        // the existing JSON-string path stays the default.
        let body = json!({"foo": "bar", "count": 3});
        assert!(extract_content_parts(&body).is_none());
    }

    #[test]
    fn extract_returns_none_when_sentinel_key_is_one_of_many() {
        // Only an envelope where the sentinel is the SOLE key counts;
        // protects against a tool accidentally namespace-colliding by
        // returning a struct that happens to include the same field.
        let mut obj = serde_json::Map::new();
        obj.insert(
            TOOL_CONTENT_PARTS_KEY.into(),
            json!([{"type": "text", "text": "x"}]),
        );
        obj.insert("other_field".into(), json!("hello"));
        let body = Value::Object(obj);
        assert!(extract_content_parts(&body).is_none());
    }
}
