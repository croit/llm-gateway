-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Gateway groups: the IdP-independent access-control unit, and the DB home for
-- what used to live only in the `[rbac]` + `[[roles]]` config.
--
-- Rationale (mirrors the upstreams move in 0042): the config file is a poor
-- place to manage day-to-day access — every edit needs an operator + restart.
-- We make the DB the runtime source of truth and seed it once from the config
-- on first boot (see `db::gateway_groups::seed_from_config`). After that the
-- `/admin/groups` UI manages everything; the config `[[roles]]` block becomes
-- a legacy seed-only mechanism and can be removed. The only RBAC bits that
-- stay in the config are the OIDC provider setup and the break-glass admin
-- (`[gateway].bootstrap_admin_groups`) — the anti-lockout anchor that resolves
-- straight from raw OIDC claims so a botched mapping table can't lock the
-- operator out of the UI that fixes it.
--
-- A "gateway group" and an internal "role id" are the same thing in the same
-- namespace: resources (pools, RAG collections, MCP connectors) reference a
-- group by name in their `allowed_groups`, and `Resolver::role_ids_for` maps a
-- user's raw OIDC claim values onto the set of group names they hold.

CREATE TABLE gateway_groups (
    -- Clean, stable, IdP-independent name. Referenced by resource
    -- `allowed_groups` and by the grant tables below.
    name        TEXT PRIMARY KEY NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    -- Grants the admin UI (`/admin/*`) + admin-only actions, exactly like the
    -- old `[[roles]].admin` flag. More than one group may carry it.
    is_admin    INTEGER NOT NULL DEFAULT 0,
    -- Applied to every authenticated user regardless of their claims — the DB
    -- equivalent of the old `[rbac].default_role`. At most one group should
    -- carry this, but the resolver tolerates several (unions them).
    is_default  INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
) STRICT;

-- OIDC claim value → gateway group. n:1 (several claim values may map to one
-- group; one value may map to several groups). This is the ONLY place messy
-- IdP identifiers get normalised: Entra object-id GUIDs, LDAP DNs, or a handful
-- of per-team groups all collapse onto one clean gateway group name here, so no
-- raw claim string ever leaks onto a resource ACL.
CREATE TABLE oidc_group_mappings (
    -- Which OIDC claim the value came from (e.g. `groups`). Informational today
    -- — matching is by `oidc_value` across the user's whole claim list — but
    -- kept for a future per-claim disambiguation, matching the old
    -- `[[rbac.mapping]].oidc_claim`.
    oidc_claim    TEXT NOT NULL DEFAULT 'groups',
    oidc_value    TEXT NOT NULL,
    gateway_group TEXT NOT NULL REFERENCES gateway_groups(name) ON DELETE CASCADE,
    PRIMARY KEY (oidc_value, gateway_group)
) STRICT;

-- Tool grants per gateway group (replaces `[[roles]].tools`). `tool_id` is a
-- registered tool id, or `*` for "every registered tool". Filtering to
-- currently-registered tools happens in the resolver, so a dangling row for a
-- removed tool is harmless.
CREATE TABLE group_tool_grants (
    gateway_group TEXT NOT NULL REFERENCES gateway_groups(name) ON DELETE CASCADE,
    tool_id       TEXT NOT NULL,
    PRIMARY KEY (gateway_group, tool_id)
) STRICT;
