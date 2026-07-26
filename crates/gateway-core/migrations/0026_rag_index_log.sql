-- Per-ref indexing event log.
--
-- Until now a ref carried a single `last_error` column that the indexer
-- overwrote on every run. That made failures hard to diagnose: a branch
-- that doesn't exist, a rotated PAT, an unreachable embedding model — each
-- left at most one terse line, and a *successful* run wiped it. The admin
-- had no history of what the indexer actually did or why a build failed.
--
-- This table records one row per notable indexing event (build started,
-- finished, failed, or an advisory like "0 files matched"), so the /rag
-- page can show a per-ref timeline instead of a single overwritten string.
-- It is intentionally append-only from the worker's side; the worker prunes
-- old rows per ref to bound growth (see `prune_log_entries`).
CREATE TABLE rag_index_log (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    ref_id        INTEGER NOT NULL,
    collection_id INTEGER NOT NULL,
    created_at    TEXT NOT NULL,            -- RFC 3339
    level         TEXT NOT NULL,            -- info | warn | error
    phase         TEXT NOT NULL,            -- queued | cloning | indexing | ready | error
    message       TEXT NOT NULL,
    commit_sha    TEXT,                     -- resolved HEAD sha, when known
    files         INTEGER,                  -- indexed-file count, on success
    chunks        INTEGER,                  -- embedded-chunk count, on success
    duration_ms   INTEGER,                  -- wall-clock of the build, when known
    FOREIGN KEY (ref_id) REFERENCES rag_collection_refs(id) ON DELETE CASCADE,
    FOREIGN KEY (collection_id) REFERENCES rag_collections(id) ON DELETE CASCADE
) STRICT;

-- Newest-first lookup per ref (the only read pattern: render a ref's log).
CREATE INDEX idx_rag_log_ref ON rag_index_log (ref_id, id DESC);
