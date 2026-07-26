-- Move per-collection RAG content out of the shared gateway DB into a
-- self-contained store folder per collection.
--
-- Rationale: a collection's chunk text + FTS index + vectors are 100%
-- regenerable from git, and can run to gigabytes for a big codebase.
-- Keeping them in `gateway.sqlite` (alongside irreplaceable users / chat /
-- sessions) bloated backups, widened the blast radius, and made a long
-- reindex contend on the single shared write lock. Each collection now
-- owns a folder `<rag.data_dir>/<data_uuid>/` containing its own
-- `rag.sqlite` (files + chunks + FTS), `index.usearch`, and `clone/`.
-- Deleting a collection becomes `rm -rf` of that one folder.
--
-- `gateway.sqlite` keeps only the lightweight central registry
-- (`rag_collections`: config + status), gaining a `data_uuid` that names
-- the collection's folder. The heavy tables move out, so we drop them
-- here. Existing collections are reset to `pending`: their content lived
-- in the now-dropped tables, so the indexer rebuilds it into the new
-- per-collection store on the next pass.

ALTER TABLE rag_collections ADD COLUMN data_uuid TEXT;

-- Content tables relocate to each collection's own rag.sqlite. Drop the
-- FTS triggers + virtual table first, then the base tables.
DROP TRIGGER IF EXISTS rag_chunks_fts_ai;
DROP TRIGGER IF EXISTS rag_chunks_fts_ad;
DROP TRIGGER IF EXISTS rag_chunks_fts_au;
DROP TABLE IF EXISTS rag_chunks_fts;
DROP TABLE IF EXISTS rag_chunks;
DROP TABLE IF EXISTS rag_files;

-- Force a rebuild into the new per-collection stores.
UPDATE rag_collections
   SET status = 'pending',
       last_indexed_at = NULL,
       last_indexed_commit = NULL,
       last_error = NULL;
