# `gw` CLI

The CLI is a thin client over the gateway HTTP API plus the browser-based OIDC login flow. It is **not** required to use the gateway — any OpenAI SDK with `base_url` set works — but it owns the auth UX and a couple of quality-of-life commands.

The command surface lives in `crates/cli/src/parser.rs` (top-level) and `crates/cli/src/cmd/auth.rs` (the `auth` group). It is pinned to the README's CLI table by `crates/cli/tests/readme_cli.rs`, so the two can't drift.

## Commands

```text
gw ping                      Verify the gateway is reachable. Hits GET /healthz, prints "ok".

gw auth login                Browser OIDC login; mints and stores a gateway token on disk.
                               --no-browser      Don't open a browser; just print the URL.
                               --profile <name>  Save under a named profile (default: "default").
gw auth whoami               Show the authenticated user (id, email, name, roles). Calls GET /v1/me.
gw auth tools                List the tools your role(s) grant. Calls GET /v1/me.
gw auth logout               Revoke the local token on the gateway (POST /v1/auth/logout) and forget it locally.
```

That is the complete command set. There is a single global flag, `--gateway <url>` (also read from `$GW_GATEWAY_URL`); when unset it defaults to `http://localhost:8080`. `--profile` exists only on `gw auth login`.

## `gw auth login` — flow

End-user view:
```
$ gw auth login
→ Opening sign-in page in your browser:
  https://gateway.example.com/auth/cli/begin?state=…

  After signing in, authorize the request and confirm this code:
      1234-5678
  (If the browser shows a different code, do NOT authorize — someone
   else may be trying to sign in as you.)

  Waiting for sign-in (5m timeout)…
✓ Signed in as alice@example.com
  Token stored in ~/.config/gw/credentials.toml
```

Under the hood (matches [`docs/auth.md`](auth.md)):
1. Generate a PKCE verifier + challenge.
2. `POST <gateway>/auth/cli/start` with the challenge. Receive `{state, login_url}`.
3. Derive a short human-readable confirmation code from the `state` (`shared::cli_login_code`) and print it. The browser shows the same code at the authorize step; the user compares them before approving, so a phished login attempt against a different `state` shows a mismatched code.
4. Open `login_url` via the `webbrowser` crate (unless `--no-browser`); the URL is printed either way for headless/SSH cases.
5. Poll `POST /auth/cli/poll` with `{state, verifier}` every 1s, up to a 5-minute timeout. On success the gateway returns the minted `gwk_…` token.
6. Best-effort `GET /v1/me` to record the user's email, then write `~/.config/gw/credentials.toml`.

## Credentials file

```toml
# ~/.config/gw/credentials.toml — mode 0600 on unix
default_profile = "default"

[profiles.default]
gateway_url = "https://gateway.example.com"
token       = "gwk_…"
user_email  = "alice@example.com"   # optional
issued_at   = "2026-05-16T10:32:11Z"
```

On unix the file is written with mode `0600` (verified by a unit test). Multiple named profiles can coexist in the file; the first profile written becomes `default_profile`.

`gw auth logout` makes a best-effort `POST /v1/auth/logout` to revoke the token server-side, then removes the active profile from the file regardless of whether the server call succeeded.

## Config resolution

The gateway URL is resolved as:
1. `--gateway <url>` flag, or its `$GW_GATEWAY_URL` env fallback (clap treats them as one).
2. Built-in default `http://localhost:8080`.

For the `auth whoami` / `auth tools` / `auth logout` subcommands, if no explicit `--gateway`/`$GW_GATEWAY_URL` was given (i.e. the value is still the `http://localhost:8080` default) the saved profile's `gateway_url` is used instead, so you don't have to repeat the URL after logging in.

## Exit codes

The process exits `0` on success and `1` on any error (the error is printed to stderr as `gw: <message>`). Argument-parsing errors exit `2` (clap's default). There are no finer-grained exit codes today.
