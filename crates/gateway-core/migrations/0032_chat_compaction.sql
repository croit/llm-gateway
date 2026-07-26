-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Conversation compaction.
--
-- Long chat sessions replay their entire turn history to the model on
-- every turn (see `openai_driver::run_one_turn`), so a session's
-- upstream prompt grows without bound and eventually crowds the
-- model's context window. Compaction folds the oldest prefix of a
-- conversation into a single LLM-generated summary and replays that
-- summary in place of the folded turns, keeping the most recent turns
-- verbatim (the "hybrid" scheme).
--
-- Three pieces of state:
--
-- 1. `chat_compactions` — one overlay row per session recording the
--    current summary and how far it covers (`up_to_seq`). This is an
--    overlay on the session-core-owned `chat_sessions`/`chat_turns`
--    tables (same pattern as `chat_session_settings`), so the generic
--    turn schema stays free of gateway-specific compaction columns.
--    The folded turns are NOT deleted — they stay in `chat_turns` and
--    remain visible/scrollable in the transcript above a divider; they
--    are simply not sent upstream. Re-compaction UPDATEs this row in
--    place, folding the previous summary plus the newly-aged turns into
--    a fresh summary and bumping `up_to_seq`.
--
-- 2. `chat_turns.context_tokens` — the largest `prompt_tokens` the
--    upstream reported across the turn's rounds (from the usage frame).
--    A model-tokenizer-accurate measure of "how big is this session's
--    context right now", used to decide when to auto-compact. NULL when
--    the upstream reported no usage (or metrics were unavailable).
--
-- 3. `model_defaults.context_window` — the model's context window in
--    tokens, so the auto-trigger threshold is per-model. NULL = fall
--    back to the global `[chat.compaction] default_context_window`.

CREATE TABLE chat_compactions (
    session_id    TEXT PRIMARY KEY NOT NULL,
    -- Highest turn `seq` covered by `summary`. Turns with seq <= this
    -- are replaced by the summary on replay; turns with a greater seq
    -- go upstream verbatim.
    up_to_seq     INTEGER NOT NULL,
    -- The LLM-generated summary of turns [0, up_to_seq].
    summary       TEXT NOT NULL,
    -- Context size (prompt_tokens) that triggered this compaction, and
    -- an estimate of the summary's own token cost — bookkeeping for the
    -- admin/telemetry, never load-bearing.
    tokens_before INTEGER,
    tokens_after  INTEGER,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE
);

ALTER TABLE chat_turns ADD COLUMN context_tokens INTEGER;

ALTER TABLE model_defaults ADD COLUMN context_window INTEGER;
