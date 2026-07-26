-- Per-model default sampling parameters.
--
-- The admin UI at `/admin/models` lets operators set defaults for
-- things like temperature / top_p / top_k / min_p / repeat_penalty
-- / frequency_penalty / presence_penalty / max_tokens for each
-- chat model. At request time the gateway parses this row's TOML
-- and fills in any top-level keys the client request didn't
-- already set — client values always win.
--
-- TOML rather than JSON for the storage shape because the admin UI
-- exposes it as a key=value textarea: operators read/write TOML
-- directly, and we don't want a serialize→parse round-trip to
-- silently reformat their input. The row stores the raw text + a
-- best-effort parse happens at request time; a save-time parse
-- gates obviously-broken submissions (non-table top-level, nested
-- arrays of tables, etc.).
--
-- One row per model_name. Models that aren't listed here use the
-- empty default (forward the client's body verbatim).

CREATE TABLE model_defaults (
    model_name    TEXT PRIMARY KEY NOT NULL,
    defaults_toml TEXT NOT NULL,
    updated_at    TEXT NOT NULL                          -- RFC3339
) STRICT;
