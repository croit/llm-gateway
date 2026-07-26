-- Per-user browser geolocation. Captured by `geo.ts`
-- (`navigator.geolocation.getCurrentPosition`) when the user explicitly
-- shares their location — from the button on `/tools`, or via the chat
-- feedback-loop prompt — and POSTed to `/api/v0/me/location`.
--
-- Stored on `users` (keyed by user id) so the `get_user_location` tool
-- reads it the same way `get_current_timestamp` reads `users.timezone`,
-- regardless of whether the call arrives through the chat page (session)
-- or `/v1/chat/completions` (bearer). Last shared position wins.
--
-- `loc_updated_at` (RFC 3339) drives a freshness check: a precise GPS
-- fix goes stale as the user moves, so the tool only trusts a recent
-- one and otherwise falls back to coarse GeoIP. All columns nullable —
-- a user who never shares a position simply opts out, and the tool
-- falls back to GeoIP / "unknown".
ALTER TABLE users ADD COLUMN loc_lat REAL;
ALTER TABLE users ADD COLUMN loc_lon REAL;
ALTER TABLE users ADD COLUMN loc_accuracy REAL;
ALTER TABLE users ADD COLUMN loc_updated_at TEXT;
