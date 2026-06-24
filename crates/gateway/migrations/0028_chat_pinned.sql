-- Chat pinning: a per-conversation favorite flag, owner-only.
-- When `pinned = 1`, the conversation floats to the top of the sidebar
-- list (sorted ahead of the recency order, see `list_sessions`). It's a
-- pure UI affordance — pinning never changes who can read a session.
-- Toggled by the owner via `set_pinned`. Default 0 leaves every existing
-- conversation unpinned.
ALTER TABLE chat_sessions ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
