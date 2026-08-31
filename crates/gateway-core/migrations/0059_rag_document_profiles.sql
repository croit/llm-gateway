-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Document profiles: what to extract from each document in a collection.
--
-- Retrieval answers "find me the passage". It cannot answer "when did we last
-- get an invoice from ACME, and how much" — that is a superlative over a
-- filtered *set*, and top-k similarity over thousands of near-identical
-- invoices is a coin flip. Worse, the model cannot tell that it only saw five.
--
-- A profile fixes that by turning each document into a row of normalised
-- fields at index time, queried by filter/sort/aggregate rather than by
-- similarity. The extraction itself is an LLM call, which is what keeps it
-- multilingual: no keyword lists, no per-language date parsers — the prompt
-- asks for ISO-8601 dates and decimal amounts and the model normalises
-- `31.12.2025` and `12/31/2025` and `1.234,56 €` alike.
--
-- `fields_json` is a JSON array of field definitions:
--   [{"key":"vendor","label":"Vendor","type":"text","description":"…",
--     "filterable":true,"sortable":false}]
-- `type` ∈ {text, number, date, enum}; `enum` adds "values":[…].
--
-- The schema belongs to the profile, not to this table, for the same reason
-- provider settings do (migration 0058): an operator adding a field must not
-- need a migration.
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
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
) STRICT;

-- NULL profile_id = no extraction, which is the right default: a code
-- collection has no invoices to pull fields out of, and the pass costs one
-- LLM call per document.
ALTER TABLE rag_collections ADD COLUMN profile_id INTEGER;
-- Chat model used for the extraction pass. NULL falls back to the first model
-- the chat pool advertises; a small, cheap model is the right choice here.
ALTER TABLE rag_collections ADD COLUMN extraction_model TEXT;

-- Cache of completed extractions, keyed the same way `ocr_derivatives` is:
-- the document's own bytes plus everything that changes what comes back.
-- Consequence worth stating: a full corpus rebuild re-embeds but re-runs
-- neither OCR nor field extraction.
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

-- Seed two profiles, because the two shapes of question this feature exists
-- to answer want genuinely different fields, and hardcoding invoice columns
-- would make the project-documentation case worse rather than better.
INSERT INTO rag_document_profiles (name, description, prompt, fields_json, created_at, updated_at)
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
    datetime('now'),
    datetime('now')
);

INSERT INTO rag_document_profiles (name, description, prompt, fields_json, created_at, updated_at)
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
    datetime('now'),
    datetime('now')
);
