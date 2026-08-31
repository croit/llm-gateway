-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Separate "rebuild this from scratch" from "has this ever finished".
--
-- `request_full_rebuild` used to force a fresh build by NULLing
-- `last_indexed_commit`, because that is what `CollectionRef::is_searchable`
-- reads and what the worker branches on to choose the fresh-folder path. One
-- column doing two jobs: the side effect of asking for a rebuild was that
-- `rag_search` reported the collection as never indexed for the whole build,
-- even though a full rebuild is atomic and the live store keeps serving until
-- the swap. On a large corpus that is hours of "its first index hasn't
-- completed" for a corpus that is sitting right there.
ALTER TABLE rag_collection_refs
    ADD COLUMN force_full_rebuild INTEGER NOT NULL DEFAULT 0;
