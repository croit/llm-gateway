-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Usage accounting must support modalities that are not priced by tokens.
-- Values are settled into the immutable cost column together with the
-- configured unit at flush time.

ALTER TABLE model_defaults ADD COLUMN pricing_unit TEXT NOT NULL DEFAULT 'tokens';

ALTER TABLE usage_events ADD COLUMN input_units REAL NOT NULL DEFAULT 0;
ALTER TABLE usage_events ADD COLUMN output_units REAL NOT NULL DEFAULT 0;

ALTER TABLE usage_daily ADD COLUMN input_units REAL NOT NULL DEFAULT 0;
ALTER TABLE usage_daily ADD COLUMN output_units REAL NOT NULL DEFAULT 0;
