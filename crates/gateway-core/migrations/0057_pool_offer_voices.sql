-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Which voices a speech pool offers its users to choose from.
--
-- `pool_voices` cannot answer this. Its primary key is (pool_name, lang_code),
-- one voice per language, because its job is *resolution*: given a spoken
-- language, which voice do we send. A menu is a different question with a
-- different shape -- a German deployment wants to offer several German voices,
-- which that table cannot represent at all.
--
-- Empty is the norm and means "no menu": the per-user picker then falls back to
-- offering whatever `pool_voices` resolves to, which for a single-voice
-- deployment is one entry and hides the picker entirely. Nothing here changes
-- how a voice is *resolved* -- `pool_voices` still decides the default, and a
-- user's pick is only honoured while it is on offer.

CREATE TABLE pool_offer_voices (
    pool_name  TEXT    NOT NULL REFERENCES pools(name) ON DELETE CASCADE,
    voice_id   TEXT    NOT NULL,
    -- Menu order, so an operator can put the house voice first rather than
    -- having the UI sort alphabetically.
    sort_order INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (pool_name, voice_id)
) STRICT;
