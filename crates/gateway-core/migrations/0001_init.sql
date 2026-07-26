-- Initial schema: users, gateway tokens, CLI handoff state.
-- All timestamps stored as RFC 3339 strings (sqlx + jiff serialize that way).

CREATE TABLE users (
    id          TEXT PRIMARY KEY NOT NULL,            -- OIDC subject (sub claim)
    email       TEXT NOT NULL,
    name        TEXT,
    roles_json  TEXT NOT NULL DEFAULT '[]',           -- JSON array of role strings
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE INDEX users_email ON users(email);

CREATE TABLE tokens (
    id            TEXT PRIMARY KEY NOT NULL,          -- UUID v4
    user_id       TEXT NOT NULL,
    name          TEXT NOT NULL,                       -- user-supplied label
    hash          TEXT NOT NULL UNIQUE,                -- SHA-256 hex of the bearer string
    created_at    TEXT NOT NULL,
    last_used_at  TEXT,
    expires_at    TEXT NOT NULL,
    revoked_at    TEXT,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE INDEX tokens_user_id ON tokens(user_id);

-- Short-lived state for the CLI loopback-via-polling flow. Rows are inserted
-- by /auth/cli/start, consumed and deleted by /auth/cli/poll.
CREATE TABLE cli_logins (
    state          TEXT PRIMARY KEY NOT NULL,
    pkce_challenge TEXT NOT NULL,                     -- SHA256 of CLI's verifier (base64url)
    token_plain    TEXT,                              -- plaintext, null until login completes
    expires_at     TEXT NOT NULL,
    created_at     TEXT NOT NULL
);
