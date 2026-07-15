-- Web Push subscriptions: one row per (user, browser/device) that opted in
-- to notifications. Populated by `POST /api/v0/push/subscribe` after the
-- browser's PushManager returns a subscription; consulted when an assistant
-- turn finalizes (see `spawn_assistant_worker`) to notify the owner that
-- their turn is done while they're away from the app.
--
-- A subscription is the browser-issued push endpoint URL plus the two keys
-- needed to encrypt a payload for it (RFC 8291): `p256dh` (the client's
-- P-256 public key, base64url) and `auth` (a 16-byte shared secret,
-- base64url). None of this is a gateway secret — it authorizes sending TO
-- this browser, not acting AS the user — so it's stored verbatim.
--
-- The push endpoint is globally unique per browser subscription, so it's the
-- natural conflict key: re-subscribing the same browser (e.g. after the key
-- rotated) upserts rather than duplicating. Rows are pruned lazily when the
-- push service reports the endpoint gone (404/410) at send time.
CREATE TABLE push_subscriptions (
    id          TEXT PRIMARY KEY NOT NULL,   -- UUID v4
    user_id     TEXT NOT NULL,               -- owner; sends are scoped to it
    endpoint    TEXT NOT NULL,               -- push service URL (unique per browser sub)
    p256dh      TEXT NOT NULL,               -- client public key, base64url (65-byte point)
    auth        TEXT NOT NULL,               -- client auth secret, base64url (16 bytes)
    lang        TEXT,                         -- UI language code at subscribe time (en/de/…),
                                              -- so the notification is localized for this device;
                                              -- NULL falls back to the gateway default
    user_agent  TEXT,                         -- UA string at subscribe time, for the list UI
    created_at  TEXT NOT NULL,                -- RFC 3339
    updated_at  TEXT NOT NULL,                -- RFC 3339
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

-- One row per browser subscription; re-subscribe upserts on this.
CREATE UNIQUE INDEX push_subscriptions_endpoint ON push_subscriptions(endpoint);

-- The send path reads "all of this user's subscriptions" per finalized turn.
CREATE INDEX push_subscriptions_user ON push_subscriptions(user_id);
