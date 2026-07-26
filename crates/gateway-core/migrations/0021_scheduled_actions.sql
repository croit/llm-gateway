-- Scheduled actions: per-user prompts that run automatically on a cron
-- schedule. Each fire opens a fresh chat session driven headlessly by the
-- same OpenAiDriver the interactive /chat path uses, so a scheduled run is
-- an ordinary conversation in the UI once it lands.
--
-- The background loop (`server::scheduled::worker`) polls this table every
-- ~30s: it selects rows whose `next_run_at` has passed, advances
-- `next_run_at` to the next occurrence *before* running (so a slow run or a
-- crash can't double-fire), then runs the prompt and records the outcome.
--
-- `cron` is a standard 5-field expression (the source of truth; the UI's
-- friendly builder and its "advanced" field both write it). It is evaluated
-- in `timezone` (IANA), so a daily job keeps its wall-clock time across DST.
-- `next_run_at` is the precomputed UTC fire time, NULL when paused
-- (`enabled = 0`) or when the expression has no future occurrence.
CREATE TABLE scheduled_actions (
    id              TEXT PRIMARY KEY NOT NULL,    -- UUID v4
    user_id         TEXT NOT NULL,                -- owner; rows are scoped to it everywhere
    name            TEXT NOT NULL,                -- sidebar/list label + the run's chat title
    prompt          TEXT NOT NULL,                -- the message sent to the model each run
    model           TEXT NOT NULL,                -- upstream model id
    cron            TEXT NOT NULL,                -- 5-field cron expression
    timezone        TEXT NOT NULL,                -- IANA tz the cron is evaluated in
    tools_enabled   INTEGER NOT NULL DEFAULT 1,   -- 1 = run with the user's normal tools
    enabled         INTEGER NOT NULL DEFAULT 1,   -- 0 = paused (worker skips it)
    next_run_at     TEXT,                         -- precomputed UTC RFC 3339; NULL when paused
    last_run_at     TEXT,                         -- when the most recent run started
    last_status     TEXT,                         -- 'ok' | 'error' for the most recent run
    last_session_id TEXT,                         -- chat session the most recent run opened
    last_error      TEXT,                         -- error detail when last_status = 'error'
    created_at      TEXT NOT NULL,                -- RFC 3339
    updated_at      TEXT NOT NULL,                -- RFC 3339
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

-- The worker's hot path: "which enabled actions are due now?"
CREATE INDEX scheduled_actions_due ON scheduled_actions(enabled, next_run_at);

-- The list page reads a user's actions newest-first.
CREATE INDEX scheduled_actions_user ON scheduled_actions(user_id, created_at DESC);
