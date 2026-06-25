-- Conversation continuity for scheduled actions.
--
-- By default each fire opens a brand-new chat session (see migration
-- 0021). When `reuse_conversation = 1`, a fire instead reuses the session
-- the previous run opened (`last_session_id`), so the model sees the prior
-- runs as conversation history — useful for digests/trackers that should
-- build on what they reported last time. The first run (or a run whose
-- prior session was deleted) falls back to creating a fresh session.
--
-- Replayed history is capped to the most recent `reuse_rounds` rounds
-- (one round = the run's user prompt + the assistant reply, i.e. 2 turns)
-- so a long-lived action can't grow its context window without bound.
ALTER TABLE scheduled_actions
    ADD COLUMN reuse_conversation INTEGER NOT NULL DEFAULT 0;  -- 1 = reuse the previous run's session

ALTER TABLE scheduled_actions
    ADD COLUMN reuse_rounds INTEGER NOT NULL DEFAULT 5;        -- recent rounds replayed when reusing
