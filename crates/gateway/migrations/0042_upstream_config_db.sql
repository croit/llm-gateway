-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Move upstream pool/backend topology from config.toml into the database so
-- admins can manage it through the UI at /admin/backends and /admin/pools
-- without a restart. On first boot after this migration the tables are empty;
-- the startup sequence seeds them from config.toml (see main.rs) and after
-- that the DB is the source of truth.
--
-- Also extends model_defaults with a capability system (tri-state: NULL =
-- unknown, 0 = confirmed unsupported, 1 = confirmed supported) and per-
-- capability fallback model references so the gateway can transparently
-- route image/audio/tool content to a capable model when the primary model
-- lacks support.

-- ---------------------------------------------------------------------------
-- Backends: connections to upstream OpenAI-compatible API endpoints.
-- ---------------------------------------------------------------------------
CREATE TABLE backends (
    name          TEXT PRIMARY KEY NOT NULL,
    base_url      TEXT NOT NULL,
    api_key_env   TEXT,                         -- env-var NAME, never the key value
    weight        INTEGER NOT NULL DEFAULT 1,
    max_inflight  INTEGER NOT NULL DEFAULT 16,
    health_path   TEXT NOT NULL DEFAULT '/models',
    probe_models  INTEGER NOT NULL DEFAULT 1,   -- 0/1
    supports_edit INTEGER NOT NULL DEFAULT 0,   -- 0/1 (image pools only)
    created_at    TEXT NOT NULL,                -- RFC3339 UTC
    updated_at    TEXT NOT NULL
) STRICT;

-- Static fallback model IDs for backends that don't self-report via /models.
-- Probe results always win; then these; then pool_models.
CREATE TABLE backend_models (
    backend_name TEXT NOT NULL REFERENCES backends(name) ON DELETE CASCADE,
    model_id     TEXT NOT NULL,
    sort_order   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (backend_name, model_id)
) STRICT;

-- Aliases: client-facing names that resolve to a real model on this backend.
-- target NULL = bare-list alias (resolve to the backend's sole model at
-- request time); target = explicit real model id.
CREATE TABLE backend_aliases (
    backend_name TEXT NOT NULL REFERENCES backends(name) ON DELETE CASCADE,
    alias        TEXT NOT NULL,
    target       TEXT,                          -- NULL = sole-model binding
    PRIMARY KEY (backend_name, alias)
) STRICT;

-- ---------------------------------------------------------------------------
-- Pools: routing groups that bind backends to a request kind.
-- ---------------------------------------------------------------------------
CREATE TABLE pools (
    name             TEXT PRIMARY KEY NOT NULL,
    kind             TEXT NOT NULL,              -- chat|transcription|embedding|image|speech
    strategy         TEXT NOT NULL DEFAULT 'least_inflight', -- least_inflight|round_robin
    fallback_offline TEXT,                       -- model id for known-but-down spill
    compliance_gdpr  INTEGER NOT NULL DEFAULT 1, -- 0/1 (advisory UI warning)
    compliance_nda   INTEGER NOT NULL DEFAULT 1, -- 0/1
    enforce_limits   INTEGER NOT NULL DEFAULT 1, -- 0/1 (count toward quotas?)
    sort_order       INTEGER NOT NULL DEFAULT 0, -- deterministic iteration order
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL
) STRICT;

-- Many-to-many: which backends belong to which pool, in display order.
CREATE TABLE pool_backends (
    pool_name    TEXT NOT NULL REFERENCES pools(name) ON DELETE CASCADE,
    backend_name TEXT NOT NULL REFERENCES backends(name) ON DELETE CASCADE,
    sort_order   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (pool_name, backend_name)
) STRICT;

-- Pool-level fallback model IDs (lowest-priority source after probe + backend).
CREATE TABLE pool_models (
    pool_name TEXT NOT NULL REFERENCES pools(name) ON DELETE CASCADE,
    model_id  TEXT NOT NULL,
    sort_order INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (pool_name, model_id)
) STRICT;

-- Language → voice-id map for speech pools.
CREATE TABLE pool_voices (
    pool_name TEXT NOT NULL REFERENCES pools(name) ON DELETE CASCADE,
    lang_code TEXT NOT NULL,                     -- lowercase ISO-639-1, '' = default
    voice_id  TEXT NOT NULL,
    PRIMARY KEY (pool_name, lang_code)
) STRICT;

-- ---------------------------------------------------------------------------
-- Unknown-model fallback (formerly [fallback] in config.toml).
-- Per-kind model id substituted when a request names a model nobody knows.
-- ---------------------------------------------------------------------------
CREATE TABLE fallback_models (
    kind     TEXT PRIMARY KEY NOT NULL,          -- chat|embedding|transcription|image
    model_id TEXT NOT NULL
) STRICT;

-- ---------------------------------------------------------------------------
-- Model capabilities: tri-state per capability + fallback model references.
-- Extends model_defaults (one row per model_name).
--
-- Tri-state convention: NULL = unknown (try and learn), 1 = supported,
-- 0 = unsupported. 'auto' source = learned from an upstream error;
-- 'admin' source = explicitly set by an operator via /admin/models.
-- ---------------------------------------------------------------------------
ALTER TABLE model_defaults ADD COLUMN cap_vision          INTEGER; -- NULL|0|1
ALTER TABLE model_defaults ADD COLUMN cap_audio_input     INTEGER;
ALTER TABLE model_defaults ADD COLUMN cap_pdf_input       INTEGER;
ALTER TABLE model_defaults ADD COLUMN cap_tools           INTEGER;
ALTER TABLE model_defaults ADD COLUMN cap_parallel_tools  INTEGER;
ALTER TABLE model_defaults ADD COLUMN cap_structured_output INTEGER;
ALTER TABLE model_defaults ADD COLUMN fallback_vision     TEXT;    -- model id
ALTER TABLE model_defaults ADD COLUMN fallback_tools      TEXT;    -- model id
ALTER TABLE model_defaults ADD COLUMN cap_updated_at      TEXT;    -- RFC3339 UTC
