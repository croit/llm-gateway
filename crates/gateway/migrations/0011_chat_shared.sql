-- Chat sharing: a single read-only capability flag on a conversation.
-- When `shared = 1`, any signed-in user who knows the session's UUID may
-- *read* it (the unguessable id is the capability). Mutations stay owner-only
-- in the application layer regardless of this flag. Toggled by the owner.
-- Default 0 keeps every existing conversation private.
ALTER TABLE chat_sessions ADD COLUMN shared INTEGER NOT NULL DEFAULT 0;
