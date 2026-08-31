-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Make extraction profiles operator-editable.
--
-- 0059 seeded two profiles and made a collection reference one. That is
-- enough for an invoice archive and a project folder, and not enough for
-- anything else: what a "vendor" or a "project" means differs per customer,
-- and a corpus of contracts or lab reports wants fields nobody here can
-- guess. The editor at `/rag` writes through these columns.
--
-- `builtin` marks the seeded profiles. They can be edited and copied like any
-- other, but not deleted: a collection pointing at a profile that vanished
-- indexes without fields and the operator gets a puzzle instead of an error.
-- Deleting a *custom* profile is allowed and blocked at the handler when a
-- collection still uses it.
ALTER TABLE rag_document_profiles ADD COLUMN builtin INTEGER NOT NULL DEFAULT 0;

UPDATE rag_document_profiles SET builtin = 1 WHERE name IN ('invoice', 'project_document');
