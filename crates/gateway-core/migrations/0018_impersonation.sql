-- Admin impersonation: let an `admin`-role user act as another user for
-- debugging, with an auditable, never-silent trail.
--
-- `sessions.impersonator_id` marks a session as an impersonation: the
-- session's `user_id` is the *target* being acted as, and
-- `impersonator_id` is the admin who started it. NULL for ordinary
-- sessions. The persistent "you are impersonating …" banner and the
-- `/impersonate/stop` route both key off this column; stop mints a fresh
-- normal session for `impersonator_id` and drops the impersonation row.
ALTER TABLE sessions ADD COLUMN impersonator_id TEXT;

-- Append-only audit of every impersonation start/stop. Kept in its own
-- table (not folded into a generic event log) so the /admin/users page
-- can cheaply show the recent trail and operators have a clear record of
-- who acted as whom. No FKs: we want the row to survive even if a user is
-- later deleted, so the trail can't be erased by cascade.
CREATE TABLE impersonation_audit (
    id          TEXT PRIMARY KEY NOT NULL,    -- UUID v4
    actor_id    TEXT NOT NULL,                -- admin who impersonated
    actor_email TEXT NOT NULL,                -- denormalised for display after deletes
    target_id   TEXT NOT NULL,                -- user being impersonated
    target_email TEXT NOT NULL,               -- denormalised for display after deletes
    action      TEXT NOT NULL,                -- 'start' | 'stop'
    created_at  TEXT NOT NULL                 -- RFC 3339
);

CREATE INDEX impersonation_audit_created_at ON impersonation_audit(created_at);
