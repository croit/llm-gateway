-- Multi-source collections (stage 2 of the multi-ref work).
--
-- Two changes:
--
--   1. A ref may now carry its OWN git_url (NULL = the collection's url).
--      That turns a "ref" (a branch/tag of one repo) into a "source" — a
--      (repo, ref) pair — so a single collection can aggregate many repos.
--      e.g. one "proxmox" collection spanning pve-manager, qemu-server,
--      pve-docs, ... each indexed independently but searched as one corpus.
--
--   2. rag_collections gains `search_mode`:
--        'versioned' (default) — refs are versions of one repo; rag_search
--                     with no `ref` uses the primary.
--        'aggregate'           — sources are different repos forming one
--                     body of knowledge; rag_search with no `ref` fans out
--                     across all searchable sources.
--
-- The old UNIQUE(collection_id, git_ref) can no longer hold: an aggregate
-- collection has many sources all on `master`. SQLite can't drop a table
-- constraint in place, so we rebuild the refs table without it and add a
-- url-aware unique index instead. NULL git_url collapses to '' so versioned
-- collections keep their (collection, ref) uniqueness. Nothing in this DB
-- references the refs table (chunks/files live in per-collection stores), so
-- the drop+rename is referentially safe.

ALTER TABLE rag_collections ADD COLUMN search_mode TEXT NOT NULL DEFAULT 'versioned';

CREATE TABLE rag_collection_refs_new (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    collection_id       INTEGER NOT NULL,
    git_ref             TEXT NOT NULL,            -- branch, tag, or commit sha
    git_url             TEXT,                     -- NULL = the collection's git_url
    is_primary          INTEGER NOT NULL DEFAULT 0,
    data_uuid           TEXT NOT NULL,            -- names <data_dir>/<uuid>/
    status              TEXT NOT NULL DEFAULT 'pending',
    last_indexed_at     TEXT,
    last_indexed_commit TEXT,
    last_error          TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    FOREIGN KEY (collection_id) REFERENCES rag_collections(id) ON DELETE CASCADE
) STRICT;

-- Preserve ids (the worker and UI reference refs by id) and default the new
-- git_url column to NULL (existing refs are versions of the collection's url).
INSERT INTO rag_collection_refs_new
    (id, collection_id, git_ref, git_url, is_primary, data_uuid, status,
     last_indexed_at, last_indexed_commit, last_error, created_at, updated_at)
SELECT
    id, collection_id, git_ref, NULL, is_primary, data_uuid, status,
    last_indexed_at, last_indexed_commit, last_error, created_at, updated_at
FROM rag_collection_refs;

DROP TABLE rag_collection_refs;
ALTER TABLE rag_collection_refs_new RENAME TO rag_collection_refs;

CREATE INDEX idx_rag_refs_collection ON rag_collection_refs (collection_id);
-- At most one primary ref per collection (versioned mode's search default).
CREATE UNIQUE INDEX idx_rag_refs_one_primary
    ON rag_collection_refs (collection_id) WHERE is_primary = 1;
-- Uniqueness per source: versioned refs (git_url NULL → '') stay unique on
-- (collection, ref); aggregate sources are unique on (collection, url, ref).
CREATE UNIQUE INDEX idx_rag_refs_unique_source
    ON rag_collection_refs (collection_id, COALESCE(git_url, ''), git_ref);
