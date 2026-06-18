# Authentication

Two distinct concerns, often conflated; keep them separate in code and docs.

1. **Login** — OIDC against a generic provider, used once to establish identity.
2. **Ongoing API auth** — gateway-minted bearer tokens, used on every `/v1/*` call.

## Login (OIDC)

We use the `openidconnect` crate (PKCE, discovery, code exchange) against any standards-compliant OIDC provider — Keycloak, Authentik, Auth0, Okta, Microsoft Entra, Google. The provider is configured by issuer URL; we never hard-code one.

### Config

```toml
# gateway.toml
[oidc]
issuer = "https://id.example.com/realms/company"
client_id = "llm-gateway"
# client_secret comes from $GATEWAY_OIDC_CLIENT_SECRET — never the config file
redirect_uri = "https://gateway.example.com/auth/callback"
scopes = ["openid", "profile", "email", "groups"]
# Optional: which OIDC claim carries role memberships.
# We map claim values → internal role IDs in [rbac.mapping] (see tools-rbac.md).
roles_claim = "groups"
```

Required env: `GATEWAY_OIDC_CLIENT_SECRET`.

### Browser flow (web UI users)

Standard server-side OIDC:

1. User hits a protected page → middleware sees no session → redirects to `/auth/login`.
2. `/auth/login` generates PKCE verifier + state, stashes them in the session, and 302s to the provider's auth endpoint.
3. Provider redirects back to `/auth/callback?code=…&state=…`.
4. Gateway verifies state, exchanges code for ID/access tokens, validates the ID token signature, extracts subject + email + roles claim.
5. Gateway upserts the user in SQLite, attaches the user id to the session, redirects to the originally requested page.

Sessions are a hand-rolled `SessionStore` (see `rama_server::session`): an HMAC-SHA256-signed cookie `id=<session_id>.<hmac-b64url>` plus a row in the `sessions` table. The pending OIDC handshake (PKCE verifier + nonce + return_to + cli_state) lives in `pending_logins`, keyed by the OIDC `state` parameter. Cookie attributes: `HttpOnly; Secure; SameSite=Lax`.

### CLI flow (loopback redirect)

This is the Claude-Code-style flow. The CLI never sees the OIDC client secret and the OIDC provider doesn't need to know about loopback URIs — only the gateway is registered.

```
┌──────┐                      ┌─────────┐                   ┌──────────┐
│ CLI  │                      │ Gateway │                   │ Browser  │
└───┬──┘                      └────┬────┘                   └─────┬────┘
    │ 1. POST /auth/cli/start      │                              │
    │    {challenge: pkce-challenge}                              │
    ├─────────────────────────────►│                              │
    │ 2. {state, login_url}        │                              │
    │◄─────────────────────────────│                              │
    │                              │                              │
    │ 3. open(login_url)           │ login_url points to /auth/cli/begin?state=…
    ├──────────────────────────────┼─────────────────────────────►│
    │                              │                              │
    │                              │ 4. OIDC dance (redirect chain through provider)
    │                              │◄─────────────────────────────┤
    │                              │                              │
    │                              │ 5. mint gateway token bound to state │
    │                              │    render success page                │
    │                              │─────────────────────────────►│
    │                              │                              │
    │ 6. POST /auth/cli/poll       │                              │
    │    {state, pkce-verifier}    │                              │
    ├─────────────────────────────►│                              │
    │ 7. {gateway_token, user}     │                              │
    │◄─────────────────────────────│                              │
```

Notes:
- **PKCE between CLI and gateway** — even though OIDC PKCE is between gateway and provider, we add a second PKCE between CLI and gateway so the gateway only releases the token to the process that initiated the login.
- **Polling, not loopback HTTP server**. Several alternatives considered:
    - *Loopback HTTP server in the CLI*: complex on Windows, awkward port allocation, requires the gateway to know a localhost URL. We avoid it.
    - *Polling* (chosen): CLI calls `/auth/cli/start`, gets a `state` id, opens the browser, then polls `/auth/cli/poll` every ~1s for up to 5 min. State expires server-side. Simpler, no localhost socket.
- **Token TTL**: gateway tokens default to 90 days. Refresh isn't automatic — users re-run `gw auth login`. The gateway has a `/auth/cli/refresh` endpoint that's a no-op today, reserved for later if we want silent renewal.

### Endpoints

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET  | `/auth/login`        | none | Start browser OIDC flow |
| GET  | `/auth/callback`     | state cookie | OIDC redirect target |
| POST | `/auth/logout`       | session | Clear session, revoke gateway tokens (optional) |
| POST | `/auth/cli/start`    | none | CLI initiates login; returns `{state, login_url}` |
| GET  | `/auth/cli/begin`    | state qs | Server-side: kicks the OIDC dance, binds the resulting user to `state` |
| POST | `/auth/cli/poll`     | none | CLI polls with `{state, verifier}`; returns `{token, user}` once ready |
| POST | `/auth/cli/refresh`  | bearer | Reserved. Returns the same token today. |

## Ongoing API auth (gateway tokens)

After login, the CLI (or any OpenAI SDK pointed at us) sends `Authorization: Bearer <gateway-token>` on every `/v1/*` call.

### Token format

Random 256-bit value (32 bytes from `OsRng`, hex-encoded), prefixed `gwk_` so the tokens are greppable in logs and accidentally-pushed configs. Wire form: `gwk_<64 hex chars>`. Stored in SQLite as the **SHA-256 hex** of the bearer string — not the plaintext, not an argon2id hash.

Why SHA-256, not argon2id:
- Argon2id is designed for *low-entropy* secrets (passwords) that need slow-down to resist brute-force.
- Our tokens are 256 bits of OS entropy. Brute-forcing them is computationally infeasible regardless of hash speed.
- Fast hashing matters: every `/v1/*` request hashes the bearer and does a DB lookup. SHA-256 keeps that well under a millisecond.
- The lookup column is hex-encoded so it's a normal indexed string column. No special index needed.

We **don't** use JWTs for gateway tokens. Rationale:
- Revocation is trivial with DB-backed tokens (`UPDATE … SET revoked_at = …`).
- We don't need cross-service verification; the gateway is the only verifier.
- One fewer crate (no `jsonwebtoken`).

### Token-bound metadata

Each token row carries:
- `user_id` (FK to users)
- `name` (user-supplied, e.g. "laptop")
- `created_at`, `last_used_at`, `expires_at`
- `revoked_at` (nullable)

The web UI lets users name, list, and revoke their tokens. Token plaintext is shown **once**, on creation.

### Auth resolution on rama

The rama proxy router resolves auth inline at the top of each handler (no middleware layer — rama Service-style handlers receive the full `Request` and run their own gate):

1. Read `Authorization: Bearer …` *or* the signed session cookie.
2. For bearer: hash + look up in `tokens`. Reject 401 on miss / revoked / expired.
3. For session cookie: verify HMAC, look up `sessions` row, hydrate the `users` row.
4. Bump `last_used_at` on bearer hits (debounced — at most once per minute per token).
5. Build a `UserContext` with `user_id`, role set, and the allowed-tools set derived from `Resolver::allowed_tools`.

The distinction between API routes (`/v1/*`, `/api/v0/*`) and page routes (`/`, `/tokens`, `/chat/stream`) only matters for the *failure* mode: API routes return 401 JSON, page routes 303 to `/login`. The lookup itself is the same.

## What's intentionally out of scope (for now)

- **Refresh tokens between CLI and gateway** — re-login is acceptable for a 90-day TTL.
- **Service-to-service auth** — no machine accounts yet. When we add them, they're a separate token kind with their own table and explicit RBAC config.
- **Token scopes** — every token has full access to its user's permitted tools/models. Scoped tokens (e.g. "transcription-only") are a future feature.
