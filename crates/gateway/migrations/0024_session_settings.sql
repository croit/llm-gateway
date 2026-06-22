-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Per-conversation reasoning/effort setting + per-model reasoning style.
--
-- `chat_session_settings` is a gateway-owned overlay on the shared
-- `chat_sessions` table (which session-core owns): it keeps gateway-specific
-- knobs out of the generic session row. Today it carries one column, `effort`
-- ("fast" | "standard" | "deep" | "max"), the user-chosen "Denkaufwand" that
-- drives both the upstream reasoning budget and the per-turn tool-round cap.
-- NULL / missing row = the "standard" default.
--
-- `model_defaults.reasoning_style` records how a model wants its reasoning
-- budget expressed on the wire ("qwen" | "openai" | "glm" | "anthropic" |
-- "none"), so `server::reasoning::apply_effort` can translate one effort level
-- into the right backend-specific parameter. NULL = auto-detect from the model
-- name at request time.

CREATE TABLE chat_session_settings (
    session_id  TEXT PRIMARY KEY NOT NULL,
    effort      TEXT,
    updated_at  TEXT NOT NULL,
    FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE
);

ALTER TABLE model_defaults ADD COLUMN reasoning_style TEXT;
