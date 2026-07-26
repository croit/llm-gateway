-- Per-user tool preferences — the rows behind the /tools page.
--
-- Each user can turn the individual AI tools the assistant may call
-- on or off for their own account. This is a personal layer *on top
-- of* RBAC: a row here can only ever take away a tool the user's
-- roles already grant, never add one.
--
-- Default is "enabled": a tool with no row is on. We only persist an
-- explicit choice, so newly added tools light up for everyone without
-- a backfill. `enabled = 0` is the disable; `enabled = 1` records an
-- explicit re-enable (so a future default flip wouldn't surprise the
-- user).
--
-- `tool_key` is the toggle key shown in the UI, NOT necessarily a
-- registered tool id: the per-template `typst_<id>` tools collapse to
-- a single `typst` key, so one row governs the whole family.
--
-- One row per (user, tool_key).

CREATE TABLE user_tool_prefs (
    user_id    TEXT NOT NULL,
    tool_key   TEXT NOT NULL,
    enabled    INTEGER NOT NULL,                          -- 0 = off, 1 = on
    updated_at TEXT NOT NULL,                             -- RFC3339
    PRIMARY KEY (user_id, tool_key)
) STRICT;
