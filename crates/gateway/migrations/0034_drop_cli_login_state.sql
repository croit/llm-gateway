-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH
--
-- Drop the `gw` CLI's server-side state.
--
-- The command-line client and its loopback-via-polling login flow
-- (`/auth/cli/*`) have been removed. Two DB artifacts backed only that
-- flow and are now dead:
--
--   * the `cli_logins` table (introduced in 0001_init) held short-lived
--     rows between `/auth/cli/start` and `/auth/cli/poll`, and
--   * `pending_logins.cli_state` (introduced in 0003) tagged an OIDC login
--     that a CLI handoff had initiated so the callback could finish it.
--
-- Nothing writes or reads either any more, so remove them. Migrations here
-- are forward-only (no downgrades), so there is no rolled-back older binary
-- to keep the `cli_state` column alive for.

DROP TABLE IF EXISTS cli_logins;

ALTER TABLE pending_logins DROP COLUMN cli_state;
