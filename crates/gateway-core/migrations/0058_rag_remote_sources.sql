-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Pluggable remote sources for RAG collections.
--
-- Until now a collection had exactly one shape: a git URL plus a ref. That is
-- now one *provider* among several, alongside WebDAV (Nextcloud, ownCloud,
-- OpenCloud, generic) and whatever registers next — OneDrive/Graph, Dropbox,
-- S3. `source_kind` names the provider; `git` is the default so every existing
-- row keeps its current behaviour with no data migration.
--
-- `source_config_json` is a flat string map of that provider's non-secret
-- settings. Deliberately a JSON bag rather than typed columns: the setting set
-- is owned by the provider (see `ProviderFactory::config_fields`), and a schema
-- that needed a migration per provider would make "extensible" a word rather
-- than a property. The admin form, its validation, and this column are all
-- driven by the provider's own declared fields.
--
-- Secrets are split out and sealed with the at-rest key (AES-256-GCM), matching
-- `backends.api_key_ct` and the MCP connector tables. The existing git `pat`
-- column is plaintext by an earlier decision; a file-host app password grants
-- read access to a company's whole shared document store, so it does not get
-- the same treatment.
ALTER TABLE rag_collections ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'git';
ALTER TABLE rag_collections ADD COLUMN source_config_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE rag_collections ADD COLUMN source_secrets_ct BLOB;
ALTER TABLE rag_collections ADD COLUMN source_secrets_nonce BLOB;

-- Directory versions from the last completed walk, per ref, as a JSON object
-- of {relative_path: version}. Lives in the central DB rather than the
-- per-collection store because a rebuild allocates a *fresh* store folder and
-- would otherwise throw this away — and this map is exactly what lets the next
-- walk skip unchanged subtrees.
--
-- Only meaningful for providers reporting `subtree_pruning`; others leave it
-- empty. It is a cache: losing it costs one full walk, never correctness.
ALTER TABLE rag_collection_refs ADD COLUMN dir_versions_json TEXT NOT NULL DEFAULT '{}';

-- Provider-native change-feed cursor (Graph `/delta`, Dropbox
-- `list_folder/continue`). NULL means "no cursor yet, start with a full walk".
ALTER TABLE rag_collection_refs ADD COLUMN delta_cursor TEXT;
