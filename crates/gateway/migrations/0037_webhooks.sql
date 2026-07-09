-- Webhooks: per-user prompts that run when an external caller POSTs to a
-- secret trigger URL. The event-driven twin of scheduled_actions (0021):
-- instead of a cron tick, an inbound HTTP request to `/hooks/{secret}` fires
-- the run. Each fire opens a fresh chat session driven headlessly by the same
-- engine as the interactive /chat path (see `server::headless`), so the result
-- lands as an ordinary conversation the owner can open afterwards.
--
-- The incoming request body (any JSON or text the caller sends) is appended to
-- the stored `prompt` as a clearly delimited *untrusted* block. Tools default
-- OFF (`tools_enabled = 0`): a webhook is triggered by an anonymous external
-- party, so running with the owner's tools is a deliberate, opt-in choice.
--
-- `synchronous` picks the response shape: 0 = fire-and-forget (respond 202
-- immediately, run in the background); 1 = wait for the model and return its
-- output in the HTTP response.
--
-- The trigger secret is a `gwh_<64 hex>` string (see `server::auth::token`).
-- Only its SHA-256 hash is stored, so a DB leak never exposes a live URL; the
-- plaintext is shown to the owner once on create and once on rotate.
CREATE TABLE webhooks (
    id              TEXT PRIMARY KEY NOT NULL,   -- UUID v4
    user_id         TEXT NOT NULL,               -- owner; rows are scoped to it everywhere
    name            TEXT NOT NULL,               -- list label + the run's chat title
    prompt          TEXT NOT NULL,               -- instruction sent to the model each fire
    model           TEXT NOT NULL,               -- upstream model id
    tools_enabled   INTEGER NOT NULL DEFAULT 0,  -- 1 = run with the owner's normal tools
    synchronous     INTEGER NOT NULL DEFAULT 0,  -- 1 = caller waits for the model output
    secret_hash     TEXT NOT NULL,               -- SHA-256 hex of the gwh_ trigger secret
    enabled         INTEGER NOT NULL DEFAULT 1,  -- 0 = paused (the trigger 404s)
    last_fired_at   TEXT,                         -- when the most recent fire started
    last_status     TEXT,                         -- 'ok' | 'error' for the most recent fire
    last_session_id TEXT,                         -- chat session the most recent fire opened
    last_error      TEXT,                         -- error detail when last_status = 'error'
    last_payload    TEXT,                         -- raw body of the most recent fire, kept so the
                                                  -- owner can "rerun with a different prompt" without
                                                  -- the external caller having to re-send it

    created_at      TEXT NOT NULL,                -- RFC 3339
    updated_at      TEXT NOT NULL,                -- RFC 3339
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

-- The trigger's hot path: "which enabled webhook owns this secret?"
CREATE UNIQUE INDEX webhooks_secret ON webhooks(secret_hash);

-- The list page reads a user's webhooks newest-first.
CREATE INDEX webhooks_user ON webhooks(user_id, created_at DESC);
