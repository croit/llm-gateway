-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- In-flight OAuth consent for a RAG source (Google Drive today).
--
-- Mirrors `pending_mcp_oauth`, with one difference that matters: an MCP
-- connection belongs to a *user*, but a RAG collection is a shared corpus an
-- operator configures once and a background worker indexes. So the consent is
-- keyed by collection, not by user, and the resulting refresh token is sealed
-- into `rag_collections.source_secrets_ct` alongside the client secret rather
-- than into a per-user row.
--
-- `admin_user_id` is recorded for the audit trail — who granted the gateway
-- access to this Drive — and is not used to scope the token.
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
