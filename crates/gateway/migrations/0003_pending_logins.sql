-- Short-lived state for the OIDC browser flow. Inserted by /auth/login,
-- consumed and deleted by /auth/callback. Replaces what tower-sessions did
-- with arbitrary key/value storage in the session row — we use the
-- `state` parameter (already round-tripped through the IdP) as the
-- primary key, so no cookie is needed for the in-flight flow.

CREATE TABLE pending_logins (
    state          TEXT PRIMARY KEY NOT NULL,    -- OIDC state param (also our CSRF token)
    pkce_verifier  TEXT NOT NULL,
    nonce          TEXT NOT NULL,
    return_to      TEXT,                          -- where to redirect after successful login
    cli_state      TEXT,                          -- if this login was triggered by `gw auth login`
    created_at     TEXT NOT NULL,
    expires_at     TEXT NOT NULL
);

CREATE INDEX pending_logins_expires_at ON pending_logins(expires_at);
