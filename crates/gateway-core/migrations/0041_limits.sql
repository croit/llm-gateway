-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Rate limits & quotas (Phase 2 of the accounting feature).
--
-- One flexible table holds every limit rule, at every attachment level. A
-- rule caps one (dimension) over one sliding (window_kind), optionally scoped
-- to a single model, for one subject:
--
--   * subject_type = 'global' — the deployment-wide default (subject_id '').
--   * subject_type = 'role'   — a role id (from `[[roles]]`); subject_id = id.
--   * subject_type = 'user'   — a specific user; subject_id = users.id.
--
-- Resolution (see `db::limits::effective_limits`) is per
-- (model-scope, dimension, window_kind) cell: a matching user rule wins; else
-- the most-generous matching role rule (a user may hold several roles); else
-- the global default. A user's whole budget is shared across all their API
-- tokens + chat + scheduled traffic — rules key on the user, never the token.
--
-- `model` NULL = the aggregate over all *enforce_limits* models; a value scopes the
-- rule to that one model id. `dimension` is 'requests' | 'tokens' | 'cost'
-- (cost in the `[usage] currency`). `window_kind` is a sliding window snapped
-- to the top of the hour: 'hour' | 'day' | 'week' | 'month'. `value` is the
-- threshold (whole for requests/tokens, fractional for cost) — REAL holds all
-- three. Enforcement is post-hoc (a request is allowed while under limit, its
-- usage settled after, the next request blocked once over) with a hard 429.

CREATE TABLE limits (
    id           TEXT PRIMARY KEY NOT NULL,  -- uuid v4
    subject_type TEXT NOT NULL,              -- 'global' | 'role' | 'user'
    subject_id   TEXT NOT NULL,              -- '' (global) | role id | users.id
    model        TEXT,                       -- NULL = all enforce_limits models
    dimension    TEXT NOT NULL,              -- 'requests' | 'tokens' | 'cost'
    window_kind  TEXT NOT NULL,              -- 'hour' | 'day' | 'week' | 'month'
    value        REAL NOT NULL,              -- threshold (>= 0)
    created_at   TEXT NOT NULL,              -- RFC3339 UTC
    updated_at   TEXT NOT NULL
);

-- At most one rule per (subject, model-scope, dimension, window); re-saving
-- the same coordinates updates the value in place. IFNULL folds the nullable
-- model into the uniqueness key ('' = the all-models aggregate slot).
CREATE UNIQUE INDEX limits_unique
    ON limits (subject_type, subject_id, IFNULL(model, ''), dimension, window_kind);

CREATE INDEX limits_subject ON limits (subject_type, subject_id);

-- Whether this call counts toward limits. Set at settle time from the serving
-- pool's `enforce_limits` flag (self-hosted GPU pools set `enforce_limits = false`; cloud
-- pools default true). Enforcement sums only `enforce_limits = 1` rows, so exempt
-- traffic is recorded for the dashboards but never consumes a budget. Stored
-- on the row so re-flagging a pool later doesn't rewrite history.
ALTER TABLE usage_events ADD COLUMN enforce_limits INTEGER NOT NULL DEFAULT 1;
