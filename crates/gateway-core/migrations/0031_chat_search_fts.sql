-- Full-text search over chat conversations.
--
-- Enables searching within conversation content (user prompts and assistant
-- responses) without indexing reasoning/thinking blocks. Attachment markers
-- ([gw-attachment file="..."]) are indexed, so searching for a filename finds
-- the conversation where it was attached.
--
-- External-content FTS5: the virtual table shadows `chat_turns` rather than
-- storing a second copy of the text. Triggers keep it in sync with
-- inserts/deletes/updates (the worker appends to content incrementally as
-- the stream arrives).
--
-- NOTE on the rowid: `chat_turns.id` is a TEXT UUID, but FTS5's external-
-- content rowid MUST be an integer. So we key the index on `chat_turns`'s
-- implicit integer `rowid` (a TEXT-PK table still has one) via the default
-- `content_rowid='rowid'`, and the triggers/backfill reference `new.rowid`.
-- The search query joins `chat_turns t ON t.rowid = fts.rowid`.
--
-- Only `user_content` and `content` are indexed — `reasoning` is excluded
-- so searches don't match internal thinking blocks. `unicode61` tokenizer
-- treats `_` as a separator (like the RAG index), so identifiers like
-- `osd_op_timeout` tokenize to [osd, op, timeout] and "osd timeout" matches.

CREATE VIRTUAL TABLE chat_turns_fts USING fts5(
    user_content,
    content,
    content='chat_turns',
    tokenize='unicode61'
);

-- Keep the FTS index in sync with the source table.
--
-- Invariant: a chat_turns row is present in the FTS index IFF its status is
-- terminal (anything other than 'in_progress'). This is the key to keeping
-- indexing off the streaming hot path:
--
--   * An assistant turn is inserted `in_progress` with empty content, then
--     the worker fires `append_content` (UPDATE of `content`) every ~100ms
--     as tokens arrive. If the index tracked the row live, every one of
--     those updates would delete + re-tokenize the whole (growing) content —
--     O(n²) tokenization per turn on the busiest write path. Instead the row
--     is indexed exactly ONCE, when `finalize_turn` flips the status to a
--     terminal value; the streaming UPDATEs touch a not-yet-indexed row and
--     the triggers below no-op on them.
--   * A user turn is inserted already-terminal (`completed`), so it is
--     indexed at INSERT.
--   * A crashed turn swept to `errored` at startup transitions
--     in_progress → errored, which indexes its partial content (searchable).
--   * Editing a terminal turn's text (message edit) re-indexes it.
--
-- The `WHEN` clauses skip the trigger body entirely on the hot path
-- (in_progress → in_progress), and `OF user_content, content, status`
-- means metadata-only updates (reasoning, reasoning_elapsed_ms,
-- completed_at) never fire it at all. The external-content `'delete'`
-- command is passed the OLD column values it was indexed with, as FTS5
-- requires.

CREATE TRIGGER chat_turns_fts_ai AFTER INSERT ON chat_turns
WHEN new.status != 'in_progress'
BEGIN
    INSERT INTO chat_turns_fts(rowid, user_content, content)
    VALUES (new.rowid, new.user_content, new.content);
END;

CREATE TRIGGER chat_turns_fts_ad AFTER DELETE ON chat_turns
WHEN old.status != 'in_progress'
BEGIN
    INSERT INTO chat_turns_fts(chat_turns_fts, rowid, user_content, content)
    VALUES ('delete', old.rowid, old.user_content, old.content);
END;

CREATE TRIGGER chat_turns_fts_au AFTER UPDATE OF user_content, content, status ON chat_turns
WHEN old.status != 'in_progress' OR new.status != 'in_progress'
BEGIN
    -- Drop the previously-indexed copy, but only if it was indexed
    -- (terminal). A freshly-finalized turn (in_progress → terminal) was
    -- never indexed, so there is nothing to delete.
    INSERT INTO chat_turns_fts(chat_turns_fts, rowid, user_content, content)
        SELECT 'delete', old.rowid, old.user_content, old.content
        WHERE old.status != 'in_progress';
    -- Index the new copy once it is terminal.
    INSERT INTO chat_turns_fts(rowid, user_content, content)
        SELECT new.rowid, new.user_content, new.content
        WHERE new.status != 'in_progress';
END;

-- Backfill existing terminal turns. Any still-`in_progress` rows are crash
-- orphans; `sweep_in_progress_at_startup` flips them to `errored` right
-- after migrations, which indexes them via the UPDATE trigger.
INSERT INTO chat_turns_fts(rowid, user_content, content)
    SELECT rowid, user_content, content FROM chat_turns
    WHERE status != 'in_progress';
