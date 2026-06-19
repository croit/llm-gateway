// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Wire-format types for the gateway's session- and bearer-authenticated APIs.
//!
//! Keeping these in `shared` (and using only stdlib + serde + jiff) means the
//! server, the CLI, and the WASM web UI all deserialise them identically.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Me {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
    /// Raw OIDC claim values for this user (e.g. groups: ["engineering"]).
    pub roles: Vec<String>,
    /// Internal RBAC role IDs the user resolves to after `[rbac.mapping]`
    /// (and `default_role`) is applied.
    #[serde(default)]
    pub role_ids: Vec<String>,
    /// Tools granted to this user (union over their roles). UI/CLI uses this
    /// to render the "what can I do" surface.
    #[serde(default)]
    pub allowed_tools: Vec<ToolSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenSummary {
    pub id: String,
    pub name: String,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
    pub expires_at: Timestamp,
    pub revoked: bool,
    /// Master "tool use" switch — `false` (the default) means this token
    /// gets no gateway tools (pure passthrough).
    #[serde(default)]
    pub tools_enabled: bool,
    /// Toggle keys this token has explicitly disabled (only meaningful
    /// when `tools_enabled`). A capability not listed is on by default.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTokenRequest {
    pub name: String,
    /// Token lifetime in days. Falls back to the server's default if missing.
    pub ttl_days: Option<i64>,
    /// Whether the new token may use gateway tools at all. Defaults to
    /// `false` (off) — a token is born without tool access until opted in.
    #[serde(default)]
    pub tools_enabled: Option<bool>,
    /// Toggle keys to disable up-front (only meaningful when
    /// `tools_enabled`). Lets a caller mint a locked-down token in one
    /// call, e.g. `["rag_search"]` for a "no RAG" token.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

/// Set a token's tool configuration wholesale — the master switch plus
/// the full set of disabled toggle keys. Replaces any previous per-token
/// tool prefs. Backs `PUT /api/v0/tokens/{id}/tools`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateTokenToolsRequest {
    pub tools_enabled: bool,
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTokenResponse {
    pub token: TokenSummary,
    /// Plaintext token. Shown to the user **exactly once.**
    pub plaintext: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeResponse {
    /// True if this call flipped the token from active to revoked. False if
    /// it was already revoked, never existed, or belongs to a different user.
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteResponse {
    /// True if this call hard-deleted the row. False if the token didn't
    /// exist, didn't belong to the caller, or was still active (active
    /// tokens must be revoked first — see `tokens::delete_if_revoked`).
    pub deleted: bool,
}

/// OpenAI tool definition — the JSON shape the model sees in the `tools`
/// array of a chat completion request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    /// JSON Schema for the function arguments.
    pub parameters: serde_json::Value,
}

impl ToolDef {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function".into(),
            function: ToolFunction {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// What a UI / CLI sees about a single tool registered on the gateway.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSummary {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// A single chat message in the request the WASM client sends to
/// `/api/v0/chat`. Matches the OpenAI wire shape — `role` ∈ {system, user,
/// assistant, tool} and `content` is plain text. The browser composer only
/// emits `user`; the gateway adds `assistant` on the way back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// `POST /api/v0/chat` request body. The handler forwards a JSON shape
/// compatible with `/v1/chat/completions` — keeping the wire format
/// explicit makes the contract easier for the browser to reason about than
/// "send whatever OpenAI takes".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
}

/// The error envelope every gateway endpoint emits when something goes wrong
/// on the gateway side. Mirrors OpenAI's `{"error": {…}}` shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    pub code: String,
}
