-- Add structure to per-user memories: each row is now classified as a
-- `preference`, `project` context note, or a plain `fact`. This makes
-- the store explicit (the model picks a kind when remembering),
-- groupable on the /memory page, and filterable by `recall`.
--
-- Additive ALTER so it applies cleanly on top of an already-migrated
-- 0008 database. Existing rows default to `fact` — the safe, generic
-- bucket.

ALTER TABLE user_memories ADD COLUMN kind TEXT NOT NULL DEFAULT 'fact';
