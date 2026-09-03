-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Per-token accounting: usage attribution, quotas, and a model allowlist.
--
-- `usage_events` has carried `token_id` / `token_name` since 0022, but
-- nothing ever read them: the rollup dropped the attribution, no query
-- grouped by it, and `limits` keyed on the user alone. This migration makes
-- the token a first-class accounting subject.
--
-- ## Why the rollup change cannot wait
--
-- `usage_events` is pruned at `[usage] retention_days` (90 by default);
-- `usage_daily` is kept forever. Every day this column was missing, another
-- day of per-token history aged out of the raw table and was rolled up as
-- anonymous. The backfill below recovers whatever is still in the raw table.

-- ---------------------------------------------------------------- rollups --

-- `token_id` is part of the rollup's identity, so it must never be NULL:
-- SQLite permits NULLs in a non-INTEGER PRIMARY KEY and treats each one as
-- distinct, which would stop `ON CONFLICT` from firing and turn the daily
-- upsert into an insert-per-request. '' is the "no token" subject (chat and
-- scheduled traffic), matching how `limits.subject_id` spells global.
CREATE TABLE usage_daily_new (
    day               TEXT NOT NULL,              -- 'YYYY-MM-DD' (UTC)
    user_id           TEXT NOT NULL,
    user_email        TEXT,
    token_id          TEXT NOT NULL DEFAULT '',   -- '' = chat / scheduled
    token_name        TEXT,                       -- denormalised for display
    source            TEXT NOT NULL,
    kind              TEXT NOT NULL,
    backend           TEXT NOT NULL,
    model             TEXT NOT NULL,
    req_count         INTEGER NOT NULL DEFAULT 0,
    error_count       INTEGER NOT NULL DEFAULT 0,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens      INTEGER NOT NULL DEFAULT 0,
    input_units       REAL NOT NULL DEFAULT 0,
    output_units      REAL NOT NULL DEFAULT 0,
    cost              REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (day, user_id, token_id, source, kind, backend, model)
);

-- Everything that already exists carries over as unattributed. For days the
-- raw table still covers, the next statements replace these rows with a
-- token-attributed rebuild.
INSERT INTO usage_daily_new
    (day, user_id, user_email, token_id, token_name, source, kind, backend,
     model, req_count, error_count, prompt_tokens, completion_tokens,
     total_tokens, input_units, output_units, cost)
SELECT day, user_id, user_email, '', NULL, source, kind, backend,
       model, req_count, error_count, prompt_tokens, completion_tokens,
       total_tokens, input_units, output_units, cost
FROM usage_daily;

-- Backfill from the retained raw events, which do know the token.
--
-- The oldest retained day is excluded: pruning cuts at an instant, not a day
-- boundary, so that day's events are a partial record and rebuilding from
-- them would silently undercount. It keeps its carried-over rollup instead.
-- With an empty `usage_events` every comparison against the MIN is NULL, so
-- both statements are no-ops and the carried-over rows stand.
DELETE FROM usage_daily_new
WHERE day > (SELECT MIN(substr(created_at, 1, 10)) FROM usage_events);

INSERT INTO usage_daily_new
    (day, user_id, user_email, token_id, token_name, source, kind, backend,
     model, req_count, error_count, prompt_tokens, completion_tokens,
     total_tokens, input_units, output_units, cost)
SELECT substr(created_at, 1, 10),
       user_id,
       MAX(user_email),
       COALESCE(token_id, ''),
       MAX(token_name),
       source,
       kind,
       backend,
       model,
       COUNT(*),
       COALESCE(SUM(CASE WHEN status >= 400 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(prompt_tokens), 0),
       COALESCE(SUM(completion_tokens), 0),
       COALESCE(SUM(total_tokens), 0),
       COALESCE(SUM(input_units), 0),
       COALESCE(SUM(output_units), 0),
       COALESCE(SUM(cost), 0)
FROM usage_events
WHERE substr(created_at, 1, 10)
      > (SELECT MIN(substr(created_at, 1, 10)) FROM usage_events)
GROUP BY substr(created_at, 1, 10), user_id, COALESCE(token_id, ''),
         source, kind, backend, model;

DROP TABLE usage_daily;
ALTER TABLE usage_daily_new RENAME TO usage_daily;

CREATE INDEX usage_daily_day   ON usage_daily (day);
CREATE INDEX usage_daily_user  ON usage_daily (user_id, day);
CREATE INDEX usage_daily_token ON usage_daily (token_id, day);

-- Serves both the per-token breakdown and the quota window read, which
-- filters `token_id` + `created_at` on every metered API call.
CREATE INDEX usage_events_token_created ON usage_events (token_id, created_at);

-- ----------------------------------------------------------------- limits --

-- No schema change: `limits.subject_type` gains a fourth value, 'token',
-- whose `subject_id` is a `tokens.id`. The existing unique index already
-- keys on (subject_type, subject_id, model, dimension, window_kind).
--
-- 0041 says a budget "keys on the user, never the token". That is no longer
-- true, and its text stands as written because an applied migration's
-- checksum is load-bearing. The rule now: a token rule is an *additional*
-- ceiling, not an override. Both the owner's user/role/global budget and the
-- token's own rule are checked, and whichever trips first refuses the call —
-- so minting a token can never widen a user's quota, only narrow it.

-- Who set a rule, and therefore who may change it. Token quotas are the only
-- ones with two possible authors: the token's owner (self-service, on
-- /tokens) and an operator (on /admin/limits). Without this the two are
-- indistinguishable, and since a save at the same coordinates updates in
-- place, an owner could raise — or delete — the very cap an admin put on
-- their token, which is the whole point of the admin being able to set one.
--
-- 'admin' for every pre-existing row: global/role/user rules have always been
-- admin-only, and an admin-owned default is the safe way to be wrong.
ALTER TABLE limits ADD COLUMN managed_by TEXT NOT NULL DEFAULT 'admin';

-- ----------------------------------------------------------------- models --

-- A token's model allowlist. **No rows = unrestricted** (the default, and
-- what every existing token gets). One or more rows makes the token a strict
-- allowlist: only the listed ids, and a model added to the gateway later is
-- denied until it is added here too. That is the safer default for a
-- credential handed to a third party — the alternative (store denials, allow
-- everything new) silently widens a token's reach every time the operator
-- adds a pool.
--
-- This can only ever *narrow* what the owning user may already reach: pool
-- `allowed_groups` are resolved first, so listing a model the user's groups
-- cannot see grants nothing.
CREATE TABLE token_models (
    token_id   TEXT NOT NULL,
    model      TEXT NOT NULL,   -- model id as advertised by /v1/models
    created_at TEXT NOT NULL,   -- RFC3339 UTC
    PRIMARY KEY (token_id, model),
    FOREIGN KEY (token_id) REFERENCES tokens(id) ON DELETE CASCADE
);

CREATE INDEX token_models_token ON token_models (token_id);
