-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Cached document-OCR derivatives.
--
-- OCR is the most expensive derived artefact the gateway produces: a scanned
-- 40-page PDF is 40 vision-model calls. Without a cache the same upload is
-- re-recognised on every round of every turn that references it, and a
-- gateway restart throws the result away entirely.
--
-- The row is a *derivative*: the original upload stays in S3, untouched and
-- downloadable. Nothing here is authoritative content — a lost row only costs
-- one re-run.
--
-- Cache identity is the four things that change the output:
--   * doc_sha256     -- the document bytes (same file re-uploaded in another
--                       turn, or by another user, hits the same row)
--   * model          -- the OCR model / revision
--   * prompt_version -- bumped in code when the parsing prompt changes
--   * settings_key   -- fingerprint of the inference + rasterisation settings
--                       (prompt text, max_tokens, ngram_window, max_pages,
--                       dpi, output cap)
-- Keying on the hash rather than the attachment id is deliberate: the same
-- document uploaded twice costs one OCR run, and re-uploading after an edit
-- correctly misses.
--
-- `status` carries the lifecycle the chat UI surfaces:
-- queued -> running -> completed | failed. A `failed` row is kept (operators
-- need the error) but read as a cache MISS, so a transient upstream failure
-- retries on the next turn instead of poisoning the document forever.
--
-- `pages_total` vs `pages_processed` is how a caller learns whether the whole
-- document was read: a page-limited or partially-failed run reports fewer
-- processed pages, and the injected context block says so.

CREATE TABLE IF NOT EXISTS ocr_derivatives (
    doc_sha256      TEXT    NOT NULL,
    model           TEXT    NOT NULL,
    prompt_version  TEXT    NOT NULL,
    settings_key    TEXT    NOT NULL,
    -- Provenance of the run, for operator debugging. Not part of the identity:
    -- the same bytes under a different filename are the same document.
    filename        TEXT    NOT NULL,
    mime            TEXT    NOT NULL,
    doc_bytes       INTEGER NOT NULL,
    status          TEXT    NOT NULL CHECK (status IN ('queued', 'running', 'completed', 'failed')),
    -- Recognised text. NULL until the run completes; capped by the configured
    -- output ceiling with `truncated` set when it bit.
    markdown        TEXT,
    pages_total     INTEGER,
    pages_processed INTEGER,
    truncated       INTEGER NOT NULL DEFAULT 0,
    -- Operator-facing failure reason for a `failed` row; NULL otherwise.
    error           TEXT,
    created_at      TEXT    NOT NULL,
    updated_at      TEXT    NOT NULL,
    PRIMARY KEY (doc_sha256, model, prompt_version, settings_key)
);

-- Operators prune the cache by age ("drop everything older than a month");
-- the primary key can't answer that.
CREATE INDEX IF NOT EXISTS idx_ocr_derivatives_updated
    ON ocr_derivatives (updated_at);
