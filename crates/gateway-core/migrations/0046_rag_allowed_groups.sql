-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Per-collection RAG access control by gateway group (Phase 3 of the RBAC work).
--
-- A JSON array of gateway-group names allowed to list + search this collection
-- via the `rag_list_collections` / `rag_search` tools. Empty (`[]`, the default)
-- = unrestricted: any user whose role grants the RAG tools sees every
-- collection, exactly as before this migration. When non-empty, only users
-- holding one of the listed groups (or an admin) see the collection in the
-- listing AND can search it by name — a restricted collection is invisible and
-- unsearchable to everyone else, so filtering the list can't be bypassed by
-- passing the name to `rag_search` directly.
ALTER TABLE rag_collections
    ADD COLUMN allowed_groups TEXT NOT NULL DEFAULT '[]';
