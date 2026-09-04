-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- An operator write path for a token's model allowlist.
--
-- 0060 gave quotas two authors (`limits.managed_by`) precisely because an
-- owner must not be able to undo an admin's cap. The model allowlist has the
-- same two authors and the higher stake — its stated purpose is bounding a
-- credential handed to a third party — but shipped with only the owner's
-- path, so an operator could pin a token's spend and not its reach, and any
-- owner could clear their own allowlist at will.
--
-- ## Two lists, not one owner
--
-- A quota is one row per (dimension, window) coordinate, so "who owns this
-- row" settles it. An allowlist is a *set*, and one set cannot have two
-- editors without one of them silently overwriting the other. So each author
-- keeps their own list and the effective allowlist is the intersection:
--
--   neither has rows  → unrestricted (the default every token still has)
--   one has rows      → that list
--   both have rows    → only models on BOTH
--
-- Which is the same rule the quotas use, stated for sets: each side may only
-- narrow. An admin cannot be widened by the owner adding models, and an owner
-- keeps a meaningful say over a token they issued. `token_models::for_token`
-- resolves it; nothing else needs to know there are two lists.
--
-- 'owner' for every pre-existing row: /tokens was the only writer that
-- existed, so that is what they are.

CREATE TABLE token_models_new (
    token_id   TEXT NOT NULL,
    model      TEXT NOT NULL,   -- model id as advertised by /v1/models
    -- 'owner' (set at /tokens) | 'admin' (set at /admin/tokens). Part of the
    -- key: both authors may list the same model, and each list is edited as a
    -- whole by whoever owns it.
    managed_by TEXT NOT NULL DEFAULT 'owner',
    created_at TEXT NOT NULL,   -- RFC3339 UTC
    PRIMARY KEY (token_id, managed_by, model),
    FOREIGN KEY (token_id) REFERENCES tokens(id) ON DELETE CASCADE
);

INSERT INTO token_models_new (token_id, model, managed_by, created_at)
SELECT token_id, model, 'owner', created_at FROM token_models;

DROP TABLE token_models;
ALTER TABLE token_models_new RENAME TO token_models;

CREATE INDEX token_models_token ON token_models (token_id);
