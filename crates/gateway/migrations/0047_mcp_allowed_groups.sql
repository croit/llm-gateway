-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Unify MCP connector access control with pools + RAG (Phase 4 of the RBAC
-- work): replace the single-valued `required_role` with a multi-valued
-- `allowed_groups` gateway-group list, matched exactly like a pool's
-- `allowed_groups` (empty = everyone, admins bypass, otherwise the caller must
-- hold one of the listed groups).
--
-- The old `required_role` already matched a resolved gateway-group id, so its
-- value carries over 1:1 as a single-element list. A connector with no gate
-- becomes `[]` (unrestricted), unchanged in behaviour.

ALTER TABLE mcp_catalog_connectors
    ADD COLUMN allowed_groups TEXT NOT NULL DEFAULT '[]';

UPDATE mcp_catalog_connectors
    SET allowed_groups = json_array(required_role)
    WHERE required_role IS NOT NULL AND TRIM(required_role) <> '';

ALTER TABLE mcp_catalog_connectors
    DROP COLUMN required_role;
