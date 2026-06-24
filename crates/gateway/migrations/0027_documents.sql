-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- The document canvas: long-form documents the model builds up and edits
-- incrementally across turns.
--
-- Generalises the per-template Typst data-document pattern (render stores
-- field values as a JSON attachment, `_edit` applies an RFC 6902 patch and
-- re-renders) into a format-agnostic, freeform store. A `document` is a
-- titled piece of content with a `format` (markdown / text / html / json /
-- yaml / toml); every edit appends a new immutable row to
-- `document_versions` and bumps `documents.current_ver`, so the canvas keeps
-- a full history the UI can scrub and the model can diff against.
--
-- State lives here (not in the chat transcript) so the model never has to
-- resend the whole document to change one passage — it reads/edits slices by
-- id, exactly like the Typst edit tool reads its stored `data_id`.
--
-- Documents are scoped to a chat session: the canvas is a property of the
-- conversation that produced it. ON DELETE CASCADE ties their lifetime to
-- the session (and the versions to their document).

CREATE TABLE documents (
    id           TEXT PRIMARY KEY NOT NULL,   -- doc_<uuid>
    session_id   TEXT NOT NULL,
    user_id      TEXT NOT NULL,
    title        TEXT NOT NULL,
    format       TEXT NOT NULL,               -- markdown|text|html|json|yaml|toml
    current_ver  INTEGER NOT NULL,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE
);

CREATE TABLE document_versions (
    document_id  TEXT NOT NULL,
    version      INTEGER NOT NULL,
    content      TEXT NOT NULL,
    summary      TEXT,                         -- short changelog line for this revision
    turn_id      TEXT,                         -- assistant turn that produced it (if any)
    created_at   TEXT NOT NULL,
    PRIMARY KEY (document_id, version),
    FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE
);

CREATE INDEX idx_documents_session ON documents (session_id);
