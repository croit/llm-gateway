-- Per-token tool gating — the rows + flag behind the per-token tool
-- controls on the /tokens page (and the `/api/v0/tokens` JSON API).
--
-- Two layers, both narrowing-only (a token can never grant a tool the
-- owning user's roles don't already grant):
--
--   1. `tokens.tools_enabled` — the master "tool use" switch for one
--      token. Default 0 (OFF): a token sees gateway tools only after its
--      owner explicitly turns tool use on. Every pre-existing token
--      inherits 0, so behaviour stays "no gateway tools" until opted in.
--
--   2. `token_tool_prefs` — once tool use is on, this subtracts
--      individual capabilities. Mirrors `user_tool_prefs` exactly: default
--      is enabled (a key with no row is on), `enabled = 0` is the disable,
--      `enabled = 1` records an explicit re-enable. `tool_key` is the UI
--      toggle key (the per-template `typst_<id>` tools collapse to a single
--      `typst` key — see `server::tools::catalog`), NOT necessarily a
--      registered tool id.
--
-- One row per (token, tool_key). ON DELETE CASCADE keeps the prefs in
-- lockstep with the owning token (a deleted token drops its prefs).

ALTER TABLE tokens ADD COLUMN tools_enabled INTEGER NOT NULL DEFAULT 0;

CREATE TABLE token_tool_prefs (
    token_id   TEXT NOT NULL,
    tool_key   TEXT NOT NULL,
    enabled    INTEGER NOT NULL,                          -- 0 = off, 1 = on
    updated_at TEXT NOT NULL,                             -- RFC3339
    PRIMARY KEY (token_id, tool_key),
    FOREIGN KEY (token_id) REFERENCES tokens(id) ON DELETE CASCADE
) STRICT;
