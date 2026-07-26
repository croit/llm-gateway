-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Per-user MCP connectors ("connector store"): an admin-curated catalog of
-- remote MCP servers, each user's own OAuth connection to them (tokens
-- encrypted at rest), in-flight OAuth state, tri-state per-tool permissions,
-- and a per-token policy for how `ask` tools behave over the /v1 API.
--
-- Secrets (`*_ct` + matching `*_nonce`) are AES-256-GCM ciphertext + nonce;
-- the DB layer stores them opaquely and never sees plaintext (see
-- `server::crypto`). Catalog rows are seeded at boot from a built-in default
-- set, all `enabled = 0`, so an admin only has to flip the switch.

-- Admin-managed catalog of connectable MCP servers.
CREATE TABLE mcp_catalog_connectors (
    key                  TEXT PRIMARY KEY NOT NULL,   -- stable id, e.g. "gmail"
    name                 TEXT NOT NULL,
    description          TEXT,
    icon                 TEXT,                         -- asset key / emoji for the store UI
    category             TEXT,                         -- grouping label, e.g. "Google"
    url                  TEXT NOT NULL,                -- remote streamable-HTTP MCP endpoint
    auth                 TEXT NOT NULL DEFAULT 'oauth2', -- 'oauth2' | 'none' | 'static_bearer'
    use_dcr              INTEGER NOT NULL DEFAULT 1,    -- try Dynamic Client Registration first
    client_id            TEXT,                         -- static OAuth client (when no DCR)
    client_secret_ct     BLOB,
    client_secret_nonce  BLOB,
    authorize_url        TEXT,                         -- discovery overrides (optional)
    token_url            TEXT,
    registration_url     TEXT,
    scopes_json          TEXT NOT NULL DEFAULT '[]',
    required_role        TEXT,                         -- RBAC gate for *connecting* (optional)
    enabled              INTEGER NOT NULL DEFAULT 0,   -- admin must turn it on
    seeded               INTEGER NOT NULL DEFAULT 0,   -- 1 = shipped default (vs admin-created)
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL
) STRICT;

-- A user's connection to one catalog connector (one row per user+connector).
CREATE TABLE user_mcp_connections (
    id                       TEXT PRIMARY KEY NOT NULL, -- UUID v4
    user_id                  TEXT NOT NULL,
    connector_key            TEXT NOT NULL,
    status                   TEXT NOT NULL,             -- 'connected'|'expired'|'error'
    access_token_ct          BLOB,
    access_token_nonce       BLOB,
    refresh_token_ct         BLOB,
    refresh_token_nonce      BLOB,
    token_expires_at         TEXT,
    -- Resolved OAuth token endpoint, persisted so refresh reuses it instead of
    -- re-running (and re-trusting) discovery on the long-lived refresh path.
    token_url                TEXT,
    scopes_json              TEXT NOT NULL DEFAULT '[]',
    -- Dynamically-registered client (RFC 7591), kept so refresh can re-auth.
    dcr_client_id            TEXT,
    dcr_client_secret_ct     BLOB,
    dcr_client_secret_nonce  BLOB,
    last_error               TEXT,
    created_at               TEXT NOT NULL,
    updated_at               TEXT NOT NULL,
    UNIQUE (user_id, connector_key),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_user_mcp_connections_user ON user_mcp_connections (user_id);

-- In-flight OAuth authorization (consumed by the callback). Mirrors
-- `pending_logins` but carries the connector + any DCR client minted at
-- authorize time.
CREATE TABLE pending_mcp_oauth (
    state                    TEXT PRIMARY KEY NOT NULL,
    user_id                  TEXT NOT NULL,
    connector_key            TEXT NOT NULL,
    pkce_verifier            TEXT NOT NULL,
    redirect_uri             TEXT NOT NULL,
    token_url                TEXT NOT NULL,
    resource                 TEXT,                      -- RFC 8707 canonical MCP URI
    dcr_client_id            TEXT,
    dcr_client_secret_ct     BLOB,
    dcr_client_secret_nonce  BLOB,
    return_to                TEXT,
    created_at               TEXT NOT NULL,
    expires_at               TEXT NOT NULL
) STRICT;

CREATE INDEX idx_pending_mcp_oauth_expires ON pending_mcp_oauth (expires_at);

-- Tri-state per-tool permission, scoped per user + connector + tool.
-- Absence of a row = the connector's default (read=always, write=ask),
-- derived from MCP tool annotations at resolution time.
CREATE TABLE user_mcp_tool_prefs (
    user_id        TEXT NOT NULL,
    connector_key  TEXT NOT NULL,
    tool_name      TEXT NOT NULL,                       -- remote tool name
    mode           TEXT NOT NULL,                       -- 'always' | 'ask' | 'off'
    updated_at     TEXT NOT NULL,
    PRIMARY KEY (user_id, connector_key, tool_name),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) STRICT;

-- Per-token policy for how `ask`-mode tools behave over the /v1 API (which
-- can't pause for interactive approval). connector_key = '*' is the default
-- for all connectors. Absence of a row = 'block'.
CREATE TABLE token_mcp_policy (
    token_id       TEXT NOT NULL,
    connector_key  TEXT NOT NULL,                       -- '*' = default
    ask_over_api   TEXT NOT NULL,                       -- 'block' | 'allow'
    updated_at     TEXT NOT NULL,
    PRIMARY KEY (token_id, connector_key),
    FOREIGN KEY (token_id) REFERENCES tokens(id) ON DELETE CASCADE
) STRICT;
