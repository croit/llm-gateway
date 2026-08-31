-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Which document extractors were available the last time this ref was built.
--
-- A file the ladder could not read is recorded as "skipped", not "failed",
-- and a skip does not stop the pass recording its directory versions — which
-- is right, because the file will not become readable on its own. But it does
-- become readable the moment an operator wires up the capability that was
-- missing: index 500 scanned PDFs with no OCR pool, then add one, and every
-- later sync saw those directories unchanged and pruned straight past them.
-- The scans stayed invisible until someone thought to force a full rebuild.
--
-- Comparing this fingerprint makes turning on OCR (or the document sandbox)
-- behave like any other change to how documents are read: it invalidates
-- what was indexed under the old answer.
ALTER TABLE rag_collection_refs
    ADD COLUMN extractor_fingerprint TEXT;
