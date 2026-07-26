-- Hybrid RAG retrieval: add a lexical (BM25) index alongside the dense
-- usearch vectors.
--
-- Pure vector search blurs exact identifiers — a query for
-- "osd op timeout" drifts toward semantically-adjacent code instead of
-- surfacing the literal `osd_op_timeout` option. An FTS5 index over the
-- same chunk text gives us a lexical signal we fuse with the vector hits
-- (reciprocal rank fusion in `worker::search_chunks`), so exact-symbol
-- recall and semantic recall reinforce each other.
--
-- External-content FTS5: the virtual table indexes `rag_chunks.content`
-- by `rowid = rag_chunks.id` rather than storing a second copy of the
-- text. Triggers keep it in lock-step with inserts/deletes/updates the
-- indexer makes. `unicode61` (the default tokenizer) treats `_` as a
-- separator, so `osd_op_timeout` tokenizes to [osd, op, timeout] and a
-- "osd timeout" query matches it — exactly the recall we want for code.

CREATE VIRTUAL TABLE rag_chunks_fts USING fts5(
    content,
    content='rag_chunks',
    content_rowid='id',
    tokenize='unicode61'
);

-- Keep the FTS index in sync with the content table. rag_chunks is
-- insert/delete-heavy (re-index drops + re-adds a file's chunks); the
-- update trigger is belt-and-braces in case a row is ever mutated.
CREATE TRIGGER rag_chunks_fts_ai AFTER INSERT ON rag_chunks BEGIN
    INSERT INTO rag_chunks_fts(rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER rag_chunks_fts_ad AFTER DELETE ON rag_chunks BEGIN
    INSERT INTO rag_chunks_fts(rag_chunks_fts, rowid, content)
    VALUES ('delete', old.id, old.content);
END;

CREATE TRIGGER rag_chunks_fts_au AFTER UPDATE ON rag_chunks BEGIN
    INSERT INTO rag_chunks_fts(rag_chunks_fts, rowid, content)
    VALUES ('delete', old.id, old.content);
    INSERT INTO rag_chunks_fts(rowid, content) VALUES (new.id, new.content);
END;

-- Backfill any chunks indexed before this migration (existing
-- collections keep working without a full re-index for the lexical side).
INSERT INTO rag_chunks_fts(rowid, content)
    SELECT id, content FROM rag_chunks;
