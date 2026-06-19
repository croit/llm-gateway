-- Multi-ref collections (stage 1: additive).
--
-- A collection used to *be* a single git ref (the `git_ref` column on
-- `rag_collections`). To let an operator configure a source once (e.g.
-- Ceph) and index many branches / tags / commits under it — without a
-- separate collection per version — the ref-level state moves into this
-- child table. Each indexed ref gets its own independently-built store
-- (`data_uuid` folder), status, and last-indexed commit.
--
-- This migration is deliberately ADDITIVE: it creates the refs table and
-- seeds one primary ref per existing collection from that collection's
-- current columns, but it does NOT drop the now-duplicated columns from
-- `rag_collections`. That keeps the change safe to land in stages — the
-- old code keeps working against the old columns while the indexer /
-- tools / UI are migrated to the refs table. A later cleanup migration
-- removes the dead columns once nothing reads them.

CREATE TABLE rag_collection_refs (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    collection_id       INTEGER NOT NULL,
    git_ref             TEXT NOT NULL,            -- branch, tag, or commit sha
    is_primary          INTEGER NOT NULL DEFAULT 0,
    data_uuid           TEXT NOT NULL,            -- names <data_dir>/<uuid>/
    status              TEXT NOT NULL DEFAULT 'pending',
    last_indexed_at     TEXT,
    last_indexed_commit TEXT,
    last_error          TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    UNIQUE (collection_id, git_ref),
    FOREIGN KEY (collection_id) REFERENCES rag_collections(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_rag_refs_collection ON rag_collection_refs (collection_id);
-- At most one primary ref per collection (the app guarantees at least one).
CREATE UNIQUE INDEX idx_rag_refs_one_primary
    ON rag_collection_refs (collection_id) WHERE is_primary = 1;

-- Seed one primary ref per existing collection from its current columns.
-- COALESCE the data_uuid: rows indexed before migration 0015 may have NULL.
INSERT INTO rag_collection_refs
    (collection_id, git_ref, is_primary, data_uuid, status,
     last_indexed_at, last_indexed_commit, last_error, created_at, updated_at)
SELECT
    id, git_ref, 1,
    COALESCE(data_uuid, lower(hex(randomblob(16)))),
    status, last_indexed_at, last_indexed_commit, last_error, created_at, updated_at
FROM rag_collections;
