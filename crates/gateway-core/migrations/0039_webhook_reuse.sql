-- Conversation reuse for webhooks (0037), mirroring scheduled actions (0029).
-- By default each fire opens a fresh chat; with reuse on, a fire instead
-- appends into the previous fire's chat, so the model sees prior fires as
-- history (a running incident log, a rolling digest, …). `reuse_rounds` caps
-- how many recent rounds are replayed so the context can't grow unbounded.
-- Reruns are always fresh regardless (they're ad-hoc experiments).
ALTER TABLE webhooks ADD COLUMN reuse_conversation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE webhooks ADD COLUMN reuse_rounds INTEGER NOT NULL DEFAULT 5;
