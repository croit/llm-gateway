-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Fileshare RAG: index a remote file host, extract structured fields from the
-- documents in it, and keep both fresh incrementally.
--
-- Four things at once, because they are one feature and there is no useful
-- intermediate state to land:
--
--   1. A collection can name a *source* other than git — a provider kind plus
--      a settings map plus sealed secrets — and a ref remembers enough about
--      the last walk to make the next one cheap.
--   2. Documents can carry an extraction *profile*: a prompt plus a field
--      schema, applied per document and cached by content hash, which is what
--      turns a folder of invoices into something you can ask "which invoices
--      did we get from X" about rather than only "which passage mentions X".
--   3. A source that is authorised by browser consent (Google Drive) needs
--      somewhere to park an in-flight authorization, and the collection needs
--      to record whose access it ends up reading through.
--   4. A ref needs to know when its own settings have moved out from under
--      it — a rebuild was requested, or the set of available extractors
--      changed — separately from whether it has ever finished a build.
--
-- See `docs/fileshare-rag.md`.

-- ---------------------------------------------------------------------------
-- Collections: where the files come from, how they are read, and as whom.

-- `git` (the original behaviour) or a registered provider kind.
ALTER TABLE rag_collections ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'git';
ALTER TABLE rag_collections ADD COLUMN source_config_json TEXT NOT NULL DEFAULT '{}';
-- AES-256-GCM ciphertext + nonce, as `backends.api_key_ct` works. The DB
-- layer stores them opaquely and never sees the plaintext.
ALTER TABLE rag_collections ADD COLUMN source_secrets_ct BLOB;
ALTER TABLE rag_collections ADD COLUMN source_secrets_nonce BLOB;

-- Extraction profile applied to each document, and the chat model that runs
-- it. Both NULL is the right default: a code collection has no fields worth
-- pulling out and the pass costs one model call per file.
ALTER TABLE rag_collections ADD COLUMN profile_id INTEGER;
ALTER TABLE rag_collections ADD COLUMN extraction_model TEXT;

-- Hash of the collection's sync-hook token. The token itself is shown once
-- and never stored, exactly as API tokens and webhook secrets are handled.
ALTER TABLE rag_collections ADD COLUMN sync_token_hash TEXT;
CREATE UNIQUE INDEX idx_rag_collections_sync_token
    ON rag_collections (sync_token_hash) WHERE sync_token_hash IS NOT NULL;

-- Who a shared corpus is read as. An OAuth source is indexed through one
-- account's permissions, and everyone who can search the collection sees
-- whatever that account can see. With `allowed_groups` as the only access
-- control that is the fact with a security consequence, so it is recorded
-- rather than left to be discovered by clicking "Test connection".
ALTER TABLE rag_collections ADD COLUMN connected_account TEXT;
ALTER TABLE rag_collections ADD COLUMN connected_by TEXT;
ALTER TABLE rag_collections ADD COLUMN connected_at TEXT;

-- ---------------------------------------------------------------------------
-- Refs: what the last walk saw, and what must be thrown away.

-- Directory path -> the version token the source reported for it. A provider
-- whose directory versions propagate (the ownCloud lineage) can then answer
-- "unchanged" for a whole subtree without listing it, which is the single
-- biggest saving in a re-sync.
ALTER TABLE rag_collection_refs ADD COLUMN dir_versions_json TEXT NOT NULL DEFAULT '{}';

-- Cursor for a provider-native change feed. **Not yet written by anything.**
-- The column, `DeltaPage` and `FileProvider::delta` are the seam for a
-- cursor-based provider (Microsoft Graph, Dropbox); the worker has no
-- consumer for one, so every provider currently re-walks.
ALTER TABLE rag_collection_refs ADD COLUMN delta_cursor TEXT;

-- "Rebuild from scratch", kept separate from "has ever finished a build"
-- (`last_indexed_commit`). Conflating them meant that asking for a rebuild
-- made the corpus unsearchable for the whole build — on data that was sitting
-- right there, since a full rebuild is atomic and the live store keeps
-- serving until the swap.
ALTER TABLE rag_collection_refs ADD COLUMN force_full_rebuild INTEGER NOT NULL DEFAULT 0;

-- Which document extractors were available when this ref was last built.
--
-- A file the ladder could not read is recorded as "skipped", not "failed",
-- and a skip does not stop the pass recording its directory versions — which
-- is right, because the file will not become readable on its own. But it does
-- the moment an operator wires up the capability that was missing: index 500
-- scanned PDFs with no OCR pool, then add one, and every later sync would see
-- those directories unchanged and prune straight past them. Comparing this
-- makes turning on OCR (or the document sandbox) behave like any other change
-- to how documents are read: it invalidates what was indexed under the old
-- answer.
ALTER TABLE rag_collection_refs ADD COLUMN extractor_fingerprint TEXT;

-- ---------------------------------------------------------------------------
-- Document profiles: the extraction schema, and its cache.

CREATE TABLE rag_document_profiles (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT NOT NULL UNIQUE,
    description  TEXT,
    -- Instruction text prepended to the extraction call. Operator-editable,
    -- because what "vendor" means differs between an invoice archive and a
    -- contract repository.
    prompt       TEXT NOT NULL,
    fields_json  TEXT NOT NULL,
    -- Bumped on any semantic edit to the prompt or fields. Part of the
    -- extraction cache key, so a changed profile re-extracts rather than
    -- serving fields that answered a different question.
    version      INTEGER NOT NULL DEFAULT 1,
    -- 1 for the profiles seeded below. They can be edited like any other,
    -- but the UI says where they came from.
    builtin      INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
) STRICT;

CREATE TABLE rag_extractions (
    doc_sha256      TEXT NOT NULL,
    profile_id      INTEGER NOT NULL,
    profile_version INTEGER NOT NULL,
    model           TEXT NOT NULL,
    -- The extracted object, or NULL when the run failed.
    fields_json     TEXT,
    summary         TEXT,
    -- Kept for the operator; reads as a miss so a transient backend failure
    -- retries next pass instead of poisoning the document forever.
    error           TEXT,
    created_at      TEXT NOT NULL,
    PRIMARY KEY (doc_sha256, profile_id, profile_version, model)
) STRICT;

-- ---------------------------------------------------------------------------
-- In-flight browser consent for an OAuth source.
--
-- Mirrors `pending_mcp_oauth`, with one difference that matters: an MCP
-- connection belongs to a *user*, but a RAG collection is a shared corpus an
-- operator configures once and a background worker indexes. So the consent is
-- keyed by collection, not by user, and the resulting refresh token is sealed
-- into `rag_collections.source_secrets_ct` alongside the client secret rather
-- than into a per-user row.
CREATE TABLE pending_rag_oauth (
    state          TEXT PRIMARY KEY NOT NULL,   -- CSRF token, also the lookup key
    collection_id  INTEGER NOT NULL,
    -- Which provider this consent is for, so the callback can find the
    -- factory without trusting anything from the redirect.
    source_kind    TEXT NOT NULL,
    pkce_verifier  TEXT NOT NULL,
    redirect_uri   TEXT NOT NULL,
    token_url      TEXT NOT NULL,
    admin_user_id  TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    expires_at     TEXT NOT NULL,
    FOREIGN KEY (collection_id) REFERENCES rag_collections(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_pending_rag_oauth_expires ON pending_rag_oauth (expires_at);

-- ---------------------------------------------------------------------------
-- Seeded profiles. Both are ordinary rows an operator may edit or delete.

INSERT INTO rag_document_profiles (name, description, prompt, fields_json, builtin, created_at, updated_at)
VALUES (
    'invoice',
    'Invoices and receipts: who billed us, when, and how much.',
    'You are reading a business document, most likely an invoice or receipt. Extract only what the document actually states. Leave a field out entirely rather than guessing. Dates must be ISO-8601 (YYYY-MM-DD). Amounts must be plain decimal numbers with a dot separator and no thousands separators or currency symbols; put the currency in its own field as an ISO-4217 code. Normalise regardless of the document''s language.',
    '[
      {"key":"doc_type","label":"Document type","type":"enum","values":["invoice","credit_note","receipt","reminder","other"],"description":"What kind of document this is.","filterable":true,"sortable":false},
      {"key":"vendor","label":"Vendor","type":"text","description":"The company that issued the document (who is billing us).","filterable":true,"sortable":true},
      {"key":"doc_date","label":"Document date","type":"date","description":"The invoice/issue date, not the due date or the payment date.","filterable":true,"sortable":true},
      {"key":"due_date","label":"Due date","type":"date","description":"When payment is due, if stated.","filterable":true,"sortable":true},
      {"key":"invoice_number","label":"Invoice number","type":"text","description":"The document number the issuer assigned.","filterable":true,"sortable":false},
      {"key":"total_gross","label":"Total (gross)","type":"number","description":"The final amount payable including tax.","filterable":true,"sortable":true},
      {"key":"currency","label":"Currency","type":"text","description":"ISO-4217 code, e.g. EUR or USD.","filterable":true,"sortable":false}
    ]',
    1,
    datetime('now'),
    datetime('now')
);

INSERT INTO rag_document_profiles (name, description, prompt, fields_json, builtin, created_at, updated_at)
VALUES (
    'project_document',
    'Project documentation: what it is about, when, and for which project.',
    'You are reading a document belonging to a project or product. Extract only what the document actually states or clearly identifies itself as. Leave a field out rather than guessing. Dates must be ISO-8601 (YYYY-MM-DD). Normalise regardless of the document''s language.',
    '[
      {"key":"doc_type","label":"Document type","type":"enum","values":["specification","report","meeting_notes","proposal","manual","presentation","other"],"description":"What kind of document this is.","filterable":true,"sortable":false},
      {"key":"project","label":"Project","type":"text","description":"The project, product or customer this document belongs to.","filterable":true,"sortable":true},
      {"key":"doc_date","label":"Document date","type":"date","description":"The date the document carries, if any.","filterable":true,"sortable":true},
      {"key":"authors","label":"Authors","type":"text","description":"Named authors or the owning team, if stated.","filterable":true,"sortable":false},
      {"key":"status","label":"Status","type":"text","description":"Draft, final, approved, superseded — if the document says.","filterable":true,"sortable":false}
    ]',
    1,
    datetime('now'),
    datetime('now')
);
