-- RAG subsystem — operator-defined codebase collections that the gateway
-- clones, chunks, embeds, and exposes through the `rag_search` tool.
--
-- Three tables, owned end-to-end by the indexer + admin API:
--
--   * `rag_collections` — one row per configured codebase. Holds the source
--     (git URL + ref), the embedding model that will be invoked through the
--     existing `PoolKind::Embedding` pool, the chunk parameters, and an
--     indexer-managed status field. PATs for private repos go in plaintext;
--     deployment infra is trusted (see the working-agreement decision in
--     the planning thread — no AEAD).
--
--   * `rag_files` — one row per file the indexer has touched. `content_hash`
--     lets the next re-pull diff against what's already embedded and only
--     re-chunk + re-embed the files that changed. Delete a row → the
--     ON DELETE CASCADE on `rag_chunks` reaps the chunks (and the indexer
--     follows up by removing the matching vector ids from the usearch index
--     file at data/rag/<collection_id>.usearch).
--
--   * `rag_chunks` — one row per (file, chunk_index). `vector_id` is the
--     u64 key the usearch index uses for this chunk; search returns a list
--     of vector_ids + scores, the tool joins back to this table for the
--     content + provenance.
--
-- Vector storage itself lives outside SQLite (mmap'd usearch file per
-- collection). Keeping the metadata in SQLite means the existing backup +
-- migration story still covers everything the chat side can read.

CREATE TABLE rag_collections (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    name                TEXT NOT NULL UNIQUE,
    description         TEXT,
    git_url             TEXT NOT NULL,
    git_ref             TEXT NOT NULL DEFAULT 'main',
    -- Personal access token for private repos. Plaintext is deliberate:
    -- gateway runs on trusted infra, see docs/rag.md.
    pat                 TEXT,
    embedding_model     TEXT NOT NULL,
    include_globs_json  TEXT NOT NULL DEFAULT '[]',       -- JSON array
    exclude_globs_json  TEXT NOT NULL DEFAULT '[]',       -- JSON array
    chunk_size          INTEGER NOT NULL DEFAULT 800,
    chunk_overlap       INTEGER NOT NULL DEFAULT 100,
    -- Indexer-owned. status ∈ {pending, cloning, indexing, ready, error}.
    status              TEXT NOT NULL DEFAULT 'pending',
    last_indexed_at     TEXT,                              -- RFC 3339
    last_indexed_commit TEXT,                              -- 40-char sha
    last_error          TEXT,
    created_at          TEXT NOT NULL,                     -- RFC 3339
    updated_at          TEXT NOT NULL                      -- RFC 3339
) STRICT;

CREATE TABLE rag_files (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    collection_id INTEGER NOT NULL,
    path          TEXT NOT NULL,                           -- repo-relative
    content_hash  TEXT NOT NULL,                           -- sha256 hex
    indexed_at    TEXT NOT NULL,                           -- RFC 3339
    UNIQUE (collection_id, path),
    FOREIGN KEY (collection_id) REFERENCES rag_collections(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_rag_files_collection ON rag_files (collection_id);

CREATE TABLE rag_chunks (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    collection_id INTEGER NOT NULL,
    file_id       INTEGER NOT NULL,
    chunk_index   INTEGER NOT NULL,                        -- 0-based position in file
    start_line    INTEGER NOT NULL,
    end_line      INTEGER NOT NULL,
    content       TEXT NOT NULL,
    -- Key into the per-collection usearch index file. Allocated by the
    -- indexer (monotonic per collection); used to map a similarity hit
    -- back to its provenance row.
    vector_id     INTEGER NOT NULL,
    UNIQUE (collection_id, vector_id),
    FOREIGN KEY (collection_id) REFERENCES rag_collections(id) ON DELETE CASCADE,
    FOREIGN KEY (file_id) REFERENCES rag_files(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_rag_chunks_collection ON rag_chunks (collection_id);
CREATE INDEX idx_rag_chunks_file ON rag_chunks (file_id);
