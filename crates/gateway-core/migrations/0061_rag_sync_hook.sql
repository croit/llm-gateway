-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright (C) 2026 croit GmbH

-- Event-driven freshness: let the file host tell us when to re-sync.
--
-- Until now a collection re-synced on the indexer's poll interval. That is
-- fine for a nightly corpus and wrong for one people expect to search minutes
-- after saving a file — and polling faster is the wrong fix, because an
-- unchanged corpus still costs a walk each time.
--
-- A collection can carry a **sync token**. `POST /hooks/rag/{token}` re-queues
-- its refs; Nextcloud's webhook_listeners app (or ownCloud's, or a cron job,
-- or a script anywhere) fires it on file events. The endpoint does not care
-- what the payload says: it is a doorbell, not a change feed. The walk that
-- follows is what establishes what actually changed, and it is cheap on a
-- source that supports subtree pruning.
--
-- Stored as a SHA-256 hash rather than the token itself, exactly as
-- `webhooks.secret_hash` is: a leaked database must not hand out working
-- trigger URLs. NULL means the collection has no hook.
ALTER TABLE rag_collections ADD COLUMN sync_token_hash TEXT;

-- Indexed because the lookup is by hash on every hook call, and it is the
-- only way in: an unindexed scan here would be a free amplification target.
CREATE UNIQUE INDEX idx_rag_collections_sync_token
    ON rag_collections (sync_token_hash) WHERE sync_token_hash IS NOT NULL;
