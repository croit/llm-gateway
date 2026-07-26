-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Add a `scope` dimension to the MCP connector catalog so one catalog can
-- express both connection models instead of two disjoint systems:
--
--   * `per_user` (the default, and every connector seeded before this) — each
--     user connects their own account (OAuth) or pastes their own token; the
--     credential + connection live per-user in `user_mcp_connections`.
--   * `global` — one shared identity for the whole gateway (e.g. a Discord bot
--     token). No per-user connection row; the connection is established once
--     and shared by everyone RBAC allows. Any secret lives on the connector
--     row itself (`client_secret_ct`), like the OAuth client secret does.
--
-- This replaces the old config-file `[[mcp.servers]]` mechanism, which was the
-- only home for shared/global servers. Global connectors are HTTP-only and use
-- `auth` of `none` (Discord — token baked into the sidecar, loopback endpoint)
-- or `static_bearer` (shared token, encrypted on the connector row).

ALTER TABLE mcp_catalog_connectors
    ADD COLUMN scope TEXT NOT NULL DEFAULT 'per_user';
