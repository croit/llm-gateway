# `gw` CLI

The CLI is a thin client over the gateway HTTP API plus the loopback-redirect auth flow. It is **not** required to use the gateway — any OpenAI SDK with `base_url` set works — but it owns the auth UX and a few quality-of-life commands.

## Commands

```text
gw auth login                Run OIDC login; store gateway token on disk.
gw auth logout               Revoke the local token; forget credentials.
gw auth whoami               Show current user, roles, allowed tools.

gw models                    List models available to me (RBAC-filtered).
gw tools                     List tools my role(s) grant.

gw chat                      (Phase 7) Interactive chat against the gateway.

gw config show               Print the resolved config (URL, token presence — never the token itself).
gw config set gateway-url <url>
```

All commands accept `--gateway <url>` and `--profile <name>` to override config. Output is colorless when piped (`std::io::IsTerminal`).

## `gw auth login` — flow

End-user view:
```
$ gw auth login
→ Opening https://gateway.example.com/auth/cli/begin?state=… in your browser…
  Waiting for sign-in (5m timeout)…
✓ Signed in as alice@example.com (roles: engineering)
  Token stored in ~/.config/gw/credentials.toml
```

Under the hood (matches `docs/auth.md`):
1. Generate PKCE verifier + challenge.
2. `POST <gateway>/auth/cli/start` with the challenge. Receive `{state, login_url}`.
3. Open `login_url` via the `webbrowser` crate; also print it for headless/SSH cases.
4. Loop: `POST /auth/cli/poll` with `{state, verifier}` every ~1s, max 5 min.
5. On success, write `~/.config/gw/credentials.toml`.

If the browser can't open (SSH session, headless container), the CLI falls back to printing the URL and offering a manual `--no-browser` flag.

## Credentials file

```toml
# ~/.config/gw/credentials.toml — mode 0600
default_profile = "default"

[profiles.default]
gateway_url = "https://gateway.example.com"
token       = "gwk_…"
user_email  = "alice@example.com"
issued_at   = "2026-05-16T10:32:11Z"
```

Permissions are enforced (0600 on unix). On Windows we set the ACL to user-only. If we detect lax perms on startup, we warn but don't refuse.

`gw auth logout` removes the profile entry and POSTs `/auth/logout` so the token row is marked revoked on the gateway.

## Config resolution order

For any setting (`gateway_url`, etc.):
1. CLI flag (`--gateway …`).
2. `GW_GATEWAY_URL` env var.
3. Selected profile in `~/.config/gw/credentials.toml`.
4. Built-in default (none for `gateway_url` — error if missing).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Generic error |
| 2 | Bad args |
| 3 | Auth required (no token, expired, or revoked) |
| 4 | Permission denied (RBAC) |
| 5 | Upstream / network error talking to the gateway |

Useful for shell pipelines and for `gh`-style automation.

## What's intentionally out of scope (initially)

- **Multiple concurrent profiles in active use.** One default profile is plenty.
- **TUI chat** — basic line-mode `gw chat` first; full TUI is Phase 7+.
- **Per-command shell completions.** Add via `clap_complete` once commands stabilize.
