-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Cost accounting: per-model prices + a monetary `cost` on every usage row.
--
-- Phase 1 of the rate-limit / quota / accounting feature: turn the token
-- counts already recorded in `usage_events` / `usage_daily` into money, so
-- the /usage dashboards can show spend. Enforcement (rate limits, quotas)
-- and the per-model `enforce_limits` exemption flag come in later phases.
--
-- Prices live on `model_defaults` (the existing per-model metadata row that
-- already carries `context_window`), expressed as a price per 1,000,000
-- tokens so the stored numbers stay human-readable (e.g. 3.0 = $3 / 1M).
-- Both NULL = "no price set" = the model contributes 0 cost (the default for
-- self-hosted models the operator never prices).
--
-- `cost` is computed once by the batched usage writer (`server::usage`) at
-- flush time from these prices and stored immutably on the row, so a later
-- price change never rewrites historical spend. `usage_daily.cost`
-- accumulates in place alongside the token rollups.

ALTER TABLE model_defaults ADD COLUMN input_price  REAL;  -- price per 1M prompt tokens
ALTER TABLE model_defaults ADD COLUMN output_price REAL;  -- price per 1M completion tokens

ALTER TABLE usage_events ADD COLUMN cost REAL NOT NULL DEFAULT 0;
ALTER TABLE usage_daily  ADD COLUMN cost REAL NOT NULL DEFAULT 0;
