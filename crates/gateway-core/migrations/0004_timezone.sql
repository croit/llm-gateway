-- Per-user IANA timezone. Captured by `app.js`
-- (`Intl.DateTimeFormat().resolvedOptions().timeZone`) on first authed
-- page load and POSTed to `/api/v0/me/timezone`. Stored on both
-- `users` and `sessions` so:
--   - The session row carries the device's current timezone (a user
--     on a laptop in Berlin + a phone in NYC sees two different
--     session.timezone values at the same moment).
--   - The user row keeps the last-written value for tool calls that
--     arrive via bearer auth (no session) — `/v1/chat/completions`
--     from `gw` CLI or an external API client. Fallback to UTC in
--     `get_current_timestamp` if both are null.
-- Both columns are nullable: pre-existing rows, headless clients,
-- and any caller that hasn't told us their timezone simply opt out.

ALTER TABLE users ADD COLUMN timezone TEXT;
ALTER TABLE sessions ADD COLUMN timezone TEXT;
