-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Operator-configurable per-feature default models.
--
-- Until now the model pre-selected in the chat UI (and the fallback used
-- when a tool/API call omits a model) was simply the alphabetically-first
-- model advertised by the relevant pool — so "glm-4.5" won the chat picker
-- purely by sort order, with no way to override it.
--
-- `app_settings` is a tiny global key/value store for exactly this kind of
-- deployment-wide operator preference. The first keys it carries are the
-- per-feature default model ids, written from `/admin/models`:
--
--   default_model.chat           — pre-selected chat model
--   default_model.transcription  — pre-selected voice/transcription model
--   default_model.image          — fallback image-generation model
--
-- Values are model ids exactly as advertised by the pool. A key that is
-- absent, empty, or names a model no longer being served falls back to the
-- old "first advertised" behaviour (see `server::feature_defaults`), so the
-- setting degrades gracefully across redeploys and backend changes.
--
-- The table is intentionally generic (not a fixed set of columns) so future
-- singleton operator settings can reuse it without another migration.

CREATE TABLE app_settings (
    key        TEXT PRIMARY KEY NOT NULL,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
