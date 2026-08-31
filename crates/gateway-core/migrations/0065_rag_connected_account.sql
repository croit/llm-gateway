-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Who a shared corpus is read as.
--
-- An OAuth source is indexed through one account's permissions, and everyone
-- who can search the collection sees whatever that account can see. That was
-- true but invisible: it lived in a doc comment and one generic help string,
-- and the only way to find out was to click "Test connection" and read a
-- toast. With `allowed_groups` as the sole access control, "whose eyes is
-- this corpus indexed through" is the question an operator most needs
-- answered, so it is recorded rather than inferred.
--
-- Written by the consent callback; cleared when the source changes kind.
ALTER TABLE rag_collections ADD COLUMN connected_account TEXT;
ALTER TABLE rag_collections ADD COLUMN connected_by TEXT;
ALTER TABLE rag_collections ADD COLUMN connected_at TEXT;
