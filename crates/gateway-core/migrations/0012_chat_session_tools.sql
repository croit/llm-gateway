-- Per-conversation tool enablement (tool-context-optimization, Phase 1 + 3).
--
-- Today tool access is RBAC (role grants) minus `user_tool_prefs` (global
-- per-user off switches). Both are conversation-agnostic, so every chat turn
-- injects the user's whole allowed set. This table adds a per-conversation
-- overlay: rows here turn an opt-in tool group *on* for one conversation, on
-- top of the always-on core. It only ever narrows from / adds within what RBAC
-- already grants — it can't widen access.
--
-- `tool_key` is the catalog toggle key (`server::tools::catalog::entry_key_for`)
-- — the per-template `typst_<id>` tools collapse to a single `typst` key, an
-- MCP server's tools to `mcp__<server>`, etc. — so one row governs a whole
-- group, matching the `/tools` page semantics.
--
-- `source` records *why* a group is on:
--   'manual'    — user toggled it for this conversation
--   'suggested' — model proposed it and the user confirmed
--   'auto'      — the embedding router enabled it from the user's message
-- Kept for auditing/tuning the router, and to distinguish user intent from
-- automation. Enablement is sticky for the conversation (protects the upstream
-- prefix cache), so we never delete auto rows on a later miss.
CREATE TABLE chat_session_tools (
    session_id TEXT NOT NULL,
    tool_key   TEXT NOT NULL,
    enabled    INTEGER NOT NULL,
    source     TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (session_id, tool_key),
    FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE
);

-- The hot path reads "which groups are enabled for this conversation" on every
-- turn; index the lookup.
CREATE INDEX idx_chat_session_tools_session ON chat_session_tools (session_id);
