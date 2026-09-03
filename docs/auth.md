# Authentication

Two distinct concerns, often conflated; keep them separate in code and docs.

1. **Login** — OIDC against a generic provider, used once to establish identity.
2. **Ongoing API auth** — gateway-minted bearer tokens, used on every `/v1/*` call.

## Login (OIDC)

We use the `openidconnect` crate (PKCE, discovery, code exchange) against any standards-compliant OIDC provider — Keycloak, Authentik, Auth0, Okta, Microsoft Entra, Google. The provider is configured by issuer URL; we never hard-code one.

### Config

The provider lives in the **database**, entered through the setup wizard at
`/setup` (see below). Issuer, client id, scopes and the roles claim are plain
`app_settings` rows; the client secret is sealed with the at-rest key
(`gateway_core::server::setup`).

The legacy `[oidc]` block in `gateway.toml` is import-only. On the first boot
after the setup-wizard release, `setup::import_config_once` copies it into the
database (resolving `client_secret_env` to its value), marks setup complete —
the deployment demonstrably already worked — and ignores the block from then
on. Nothing to do when upgrading; a fresh install has no file and lands in the
wizard.

The import is only *finalised* once it has actually happened. A boot that finds
no config file, or one whose `client_secret_env` is not set yet, imports nothing
and leaves the marker unset so the next boot can still import. Burning it early
was a real bug: an existing deployment that booted once without its file — a
volume mounted late — ended up with no provider, `setup.completed` set from
`has_been_used`, and therefore no way in except `restore-setup`.

```toml
# gateway.toml — legacy, import-only. New installs need none of this.
[oidc]
issuer = "https://id.example.com/realms/company"
client_id = "llm-gateway"
client_secret_env = "GATEWAY_OIDC_CLIENT_SECRET"
scopes = ["profile", "email", "groups"]
roles_claim = "groups"
```

### Setup wizard (`/setup`)

Two screens, one proof. Screen 1 takes the public URL and the provider's
issuer/client id/secret and shows the exact `{public_url}/auth/callback`
redirect URI to whitelist. Submitting runs a **genuine authorization-code round
trip** — through that same production redirect URI, marked by
`pending_logins.purpose = 'setup'` so `/auth/callback` routes it to the wizard
instead of minting a session. Screen 2 shows the verified ID token's claims and
asks which claim value grants admin.

The round trip is the point: discovery only proves a URL answers. It does not
prove the client secret, the redirect whitelisting, or — the thing nobody can
guess — what the provider calls its groups claim and what values it contains.
There is no way to reach screen 2 without a login that worked.

Finishing stores the provider, creates an `admins` group (`is_admin`) mapped to
the chosen value plus a default `users` group, and calls
`AppState::set_runtime` — so the live OIDC client and public URL are swapped in
without a restart.

**Access.** `SetupAccess::FirstRun` (nothing configured) is open: there is no
account to authenticate against and nothing configured worth stealing.
`SetupAccess::Closed` (configured) 404s. `SetupAccess::Recovery` is opened by
`restore-setup` on the host for 30 minutes and needs the one-time token that
command prints, carried afterwards by a `gw_setup` cookie scoped to `/setup`.

**Recovery is not first-run mode.** With a recovery window open the gateway
keeps serving normally — chats, `/v1`, existing sessions — and only `/setup`
becomes reachable again. Conflating the two would let one locked-out admin take
a production gateway offline for everyone else. `setup_wizard.rs` pins this.

The wizard cannot help if the provider itself is gone, since it proves a
provider by signing in through it. `[gateway].bootstrap_admin_groups` remains
the break-glass anchor that does not depend on the group tables.

Required env: none for OIDC. `GATEWAY_SESSION_KEY` is required for the gateway
to boot at all (it signs sessions and derives the at-rest key).

### Browser flow (web UI users)

Standard server-side OIDC:

1. User hits a protected page → middleware sees no session → redirects to `/auth/login`.
2. `/auth/login` generates PKCE verifier + state, stashes them in the session, and 302s to the provider's auth endpoint.
3. Provider redirects back to `/auth/callback?code=…&state=…`.
4. Gateway verifies state, exchanges code for ID/access tokens, validates the ID token signature, extracts subject + email + roles claim.
5. Gateway upserts the user in SQLite, attaches the user id to the session, redirects to the originally requested page.

Sessions are a hand-rolled `SessionStore` (see `rama_server::session`): an HMAC-SHA256-signed cookie `id=<session_id>.<hmac-b64url>` plus a row in the `sessions` table. The pending OIDC handshake (PKCE verifier + nonce + return_to) lives in `pending_logins`, keyed by the OIDC `state` parameter. Cookie attributes: `HttpOnly; Secure; SameSite=Lax`.

### Endpoints

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET  | `/auth/login`        | none | Start browser OIDC flow |
| GET  | `/auth/callback`     | state cookie | OIDC redirect target — for a sign-in *and* for the wizard's test login, told apart by `pending_logins.purpose` |
| POST | `/auth/logout`       | session | Clear session, revoke gateway tokens (optional) |
| GET  | `/setup`             | open on a first run; one-time token in recovery | Setup wizard |
| POST | `/setup/test`        | same | Stash the entered provider and start the test login |
| POST | `/setup/restart`     | same | Discard the proven login, back to screen 1 |
| POST | `/setup/finish`      | same | Persist, create the admin group, swap the live client in |

## Ongoing API auth (gateway tokens)

After login, any OpenAI SDK pointed at us sends `Authorization: Bearer <gateway-token>` on every `/v1/*` call.

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

The distinction between API routes (`/v1/*`, `/api/v0/*`) and page routes (`/`, `/tokens`, `/chat`) only matters for the *failure* mode: API routes return 401 JSON, page routes 303 to `/login`. The lookup itself is the same.

## What's intentionally out of scope (for now)

- **Refresh tokens between CLI and gateway** — re-login is acceptable for a 90-day TTL.
- **Service-to-service auth** — no machine accounts yet. When we add them, they're a separate token kind with their own table and explicit RBAC config.
- **Per-model token scopes** — a token can already be scoped to a subset of its user's *tools* (and MCP ask/off policy) from the `/tokens` page; scoping a token to a subset of *models* (e.g. "transcription-only") is not yet implemented — every token can reach all of its user's permitted models.
