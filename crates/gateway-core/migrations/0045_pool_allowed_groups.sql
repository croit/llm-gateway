-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Per-pool access control by gateway group (Phase 2 of the RBAC work).
--
-- A JSON array of gateway-group names allowed to see + route to this pool.
-- Empty (`[]`, the default) = unrestricted: every user sees the pool's models,
-- exactly as before this migration — so restriction is an opt-in that leaves
-- existing deployments unchanged. When non-empty, only users holding one of the
-- listed groups (or an admin) may list or call the pool's models; to everyone
-- else the pool's models are invisible AND unroutable (`404 model_not_found`),
-- so filtering the listing can't be bypassed by calling the id directly.
--
-- Access is per pool, not per model or per backend: a model becomes visible to a
-- user as soon as ONE pool they may access serves it.
ALTER TABLE pools
    ADD COLUMN allowed_groups TEXT NOT NULL DEFAULT '[]';
