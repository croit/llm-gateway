-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Per-connector audit logging for MCP tool calls.
--
-- With a shared/global connector (e.g. a Discord bot) every action Discord
-- sees comes from one bot identity — so the gateway is the ONLY place the
-- human who triggered it is known. This adds an opt-in, per-connector audit
-- trail so an operator can answer "who ran which tool, when, and what
-- happened" for the connectors that matter, without logging noise from
-- low-risk read-only ones.
--
--   * `mcp_catalog_connectors.audit` — admin toggle, per connector (default
--     off). When on, every one of that connector's tool calls is recorded.
--   * `mcp_tool_audit` — append-only trail. Like `impersonation_audit`, the
--     acting user's email is denormalised and the table has NO foreign keys,
--     so the record survives (and can't be erased by) user/connector deletion.

ALTER TABLE mcp_catalog_connectors
    ADD COLUMN audit INTEGER NOT NULL DEFAULT 0;

CREATE TABLE mcp_tool_audit (
    id            TEXT PRIMARY KEY NOT NULL,
    user_id       TEXT NOT NULL,
    user_email    TEXT NOT NULL,          -- denormalised; survives user deletion
    connector_key TEXT NOT NULL,
    tool_id       TEXT NOT NULL,          -- namespaced tool id (mcp__<key>__<tool>)
    arguments     TEXT,                   -- truncated JSON of the call arguments
    outcome       TEXT NOT NULL,          -- 'ok' | 'error'
    error         TEXT,                   -- error detail when outcome = 'error'
    session_id    TEXT,                   -- chat session context, when present
    created_at    TEXT NOT NULL
) STRICT;

CREATE INDEX idx_mcp_tool_audit_created ON mcp_tool_audit (created_at DESC);
CREATE INDEX idx_mcp_tool_audit_connector ON mcp_tool_audit (connector_key, created_at DESC);
