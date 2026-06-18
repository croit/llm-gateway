-- Per-user durable memories — the store behind the `remember` / `recall`
-- tools.
--
-- The assistant can persist short, free-text facts about a user
-- (preferences, ongoing projects, names they ask it to keep) and pull
-- them back in later conversations. Strictly per-user: every row is
-- scoped by `user_id` and the tools only ever read/write the caller's
-- own rows — there is no cross-user path.
--
-- `content` is a single fact in plain text. We keep it deliberately
-- simple (no embeddings / vectors): recall is recency + substring
-- match, which is cheap, dependency-free, and good enough for the
-- handful of facts a user accumulates.

CREATE TABLE user_memories (
    id         TEXT PRIMARY KEY NOT NULL,                -- uuid v4
    user_id    TEXT NOT NULL,
    content    TEXT NOT NULL,
    created_at TEXT NOT NULL,                            -- RFC3339
    updated_at TEXT NOT NULL                             -- RFC3339
) STRICT;

-- Recall lists a user's memories newest-first; this index serves both
-- the scoping filter and the ordering.
CREATE INDEX idx_user_memories_user_created
    ON user_memories (user_id, created_at DESC);
