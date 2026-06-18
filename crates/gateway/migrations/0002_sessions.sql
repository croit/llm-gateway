-- Hand-rolled session table for the rama-based server. Replaces what
-- tower-sessions used to do — sliding expiration, multiple stores, signed
-- payloads — with the bare minimum we actually need: identify the user
-- behind a session cookie, expire after a fixed TTL.

CREATE TABLE sessions (
    id          TEXT PRIMARY KEY NOT NULL,    -- random 256-bit, hex-encoded
    user_id     TEXT NOT NULL,
    created_at  TEXT NOT NULL,                -- RFC 3339
    expires_at  TEXT NOT NULL,                -- RFC 3339
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE INDEX sessions_user_id ON sessions(user_id);
CREATE INDEX sessions_expires_at ON sessions(expires_at);
