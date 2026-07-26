-- Per-fire history for webhooks (0037). Each time a webhook fires — whether
-- from the live trigger or a manual rerun — we record one row here, so the
-- owner can browse the last N runs, open each run's generated chat, and replay
-- any past run's exact payload with a tweaked prompt.
--
-- The `webhooks.last_*` columns still hold a denormalized summary of the most
-- recent fire (for the list row); this table is the full log. `prompt` and
-- `payload` are captured per run so a historical run is fully reproducible.
--
-- `status` is NULL while a run is in flight (an async fire responds 202 before
-- its background run finishes), then set to 'ok' | 'error' on completion.
CREATE TABLE webhook_runs (
    id          TEXT PRIMARY KEY NOT NULL,   -- UUID v4
    webhook_id  TEXT NOT NULL,               -- owning webhook
    fired_at    TEXT NOT NULL,               -- RFC 3339, when the run started
    status      TEXT,                        -- NULL (pending) | 'ok' | 'error'
    session_id  TEXT,                        -- the chat session this run opened
    error       TEXT,                        -- error detail when status = 'error'
    prompt      TEXT NOT NULL,               -- the prompt used for this run
    payload     TEXT NOT NULL,               -- the request body replayed for this run
    source      TEXT NOT NULL,               -- 'fire' (external trigger) | 'rerun'
    created_at  TEXT NOT NULL,               -- RFC 3339
    FOREIGN KEY (webhook_id) REFERENCES webhooks(id) ON DELETE CASCADE
);

-- The runs list reads one webhook's history newest-first.
CREATE INDEX webhook_runs_by_hook ON webhook_runs(webhook_id, fired_at DESC);
