-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Per-request usage accounting across every access method (the /v1 proxy,
-- the chat UI, and scheduled actions). One row per upstream backend call.
--
-- Two tables, written together by the batched usage writer
-- (`server::usage`):
--
--   * `usage_events` — raw rows, kept for a configurable recent window
--     ([usage].retention_days, default 90) and pruned by the maintenance
--     task. Serves every period the UI offers (today … last month all fall
--     inside the window) with time-zone-correct boundaries and full detail.
--
--   * `usage_daily` — daily rollups (UTC-day granularity) kept forever as
--     the permanent historical record and the read source for ranges older
--     than the raw window. Accumulated in place via UPSERT as events arrive.
--
-- Emails / token names are denormalised onto both tables (the
-- `0018_impersonation.sql` pattern) so the stats page needs no joins and
-- the record survives a user/token deletion. No foreign keys for the same
-- reason — usage history must not be erased by an ON DELETE CASCADE.

CREATE TABLE usage_events (
    id                TEXT PRIMARY KEY NOT NULL,  -- uuid v4
    created_at        TEXT NOT NULL,              -- RFC3339 UTC (request time)
    user_id           TEXT NOT NULL,
    user_email        TEXT,                       -- denormalised for display
    token_id          TEXT,                       -- /v1 only; NULL for chat/scheduled
    token_name        TEXT,                       -- denormalised for display
    source            TEXT NOT NULL,              -- 'v1_api' | 'chat' | 'scheduled'
    kind              TEXT NOT NULL,              -- 'chat' | 'embedding' | 'transcription'
    backend           TEXT NOT NULL,              -- Backend.name
    model             TEXT NOT NULL,
    status            INTEGER NOT NULL,           -- upstream HTTP status
    duration_ms       INTEGER NOT NULL,
    prompt_tokens     INTEGER,                    -- NULL when unavailable
    completion_tokens INTEGER,
    total_tokens      INTEGER
);

CREATE INDEX usage_events_created_at      ON usage_events (created_at);
CREATE INDEX usage_events_user_created    ON usage_events (user_id, created_at);
CREATE INDEX usage_events_backend_created ON usage_events (backend, created_at);

CREATE TABLE usage_daily (
    day               TEXT NOT NULL,              -- 'YYYY-MM-DD' (UTC)
    user_id           TEXT NOT NULL,
    user_email        TEXT,
    source            TEXT NOT NULL,
    kind              TEXT NOT NULL,
    backend           TEXT NOT NULL,
    model             TEXT NOT NULL,
    req_count         INTEGER NOT NULL DEFAULT 0,
    error_count       INTEGER NOT NULL DEFAULT 0, -- rows whose status >= 400
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, user_id, source, kind, backend, model)
);

CREATE INDEX usage_daily_day  ON usage_daily (day);
CREATE INDEX usage_daily_user ON usage_daily (user_id, day);
