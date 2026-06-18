# Dev workflow

## Toolchain

Everything is pinned in `mise.toml`. Run `mise install` once after cloning. It installs:
- The Rust toolchain pinned to 1.95 (rustc, cargo, rustfmt, clippy).
- `cargo-binstall` (used by mise's `cargo:` backend for fast prebuilt installs).
- Node 24 — needed for `ui/`'s Tailwind v4 + daisyUI v5 build.

We **do not** check in a `rust-toolchain.toml`; mise is the single source of truth.

## Daily commands

The gateway binary `include_bytes!`'s the CSS, so the binary won't compile without a fresh `assets/app.css`. The mise tasks that need it (`dev`, `dev-build`, `build`, `test`, `lint`) all depend on `build-css`, so a fresh checkout doesn't need any manual setup — pick a goal and run it.

| Goal | Command |
|---|---|
| Run gateway against local config | `mise run dev` |
| Run a stub gateway for UI debugging (real auth, fake LLM) | `mise run dev-ui` |
| Build the gateway debug binary (no run) | `mise run dev-build` |
| Tailwind / daisyUI CSS — one-shot | `mise run build-css` |
| Tailwind / daisyUI CSS — live rebuild | `mise run watch-css` |
| Type-check (fast) | `mise run check` |
| **Release** build (slow, for deploys) | `mise run build` |
| Tests | `mise run test` |
| Tests with stdout visible | `mise run test-nocapture` |
| Lint (clippy + fmt --check) | `mise run lint` |
| Apply formatting | `mise run fmt` |
| Run the CLI | `mise run cli -- auth login` |

**Debug vs release.** `mise run build` (release) takes ~12 s incremental and ~70 s from clean — only use it when you actually want optimised output (deploys, perf measurement). For day-to-day iteration (running locally, screenshotting pages, smoke-testing changes) use `mise run dev` or `mise run dev-build`; those produce a debug binary in ~2 s incremental. Runtime perf is identical for any UX you'd interact with; only synthetic benchmarks notice the difference.

Anything not covered: add a task to `mise.toml` rather than typing the raw command into a script. Discoverability matters.

## Layout while developing

`mise run dev` runs `cargo run --package gateway` against the local `gateway.toml`. The binary opens its own port (`PORT` env var, default `8080`), reads `GATEWAY_CONFIG`, opens the SQLite at `db.path`, runs migrations, builds the upstream registry, spawns health probes, builds the OIDC client (or starts without it on a config-less dev box), and binds.

There's no WASM step, no `dx`, no hot reload of HTML — the rama server serves plain HTML and reloads happen via the browser's refresh button. The Tailwind CSS rebuild does run live in `mise run watch-css`, so style changes appear after one refresh.

The CLI is run independently via `mise run cli -- <args>`. It needs a gateway running somewhere — point it at the dev server with `GW_GATEWAY_URL=http://localhost:8080`.

## Environment

A `.env.example` should sit at the workspace root with all required env vars (gateway URL, OIDC issuer, OIDC client id, etc.). Copy to `.env` for local dev. **Never** check in `.env`; `.gitignore` already excludes it.

Required env vars are listed in `docs/auth.md` (OIDC) and `docs/upstreams.md` (provider keys).

`GATEWAY_SESSION_KEY` — 64 hex chars (32 bytes) for the session-cookie HMAC. On a config-less dev box the binary falls back to an ephemeral random key with a warning; that's fine locally but every restart invalidates open sessions.

## Debugging the UI

Every authed page (`/`, `/tokens`, `/chat`, `/theme/toggle`, the
`/api/v0/*` JSON routes) is gated by OIDC, which makes ad-hoc browser
debugging (playwright, devtools, screenshotting bugs) annoying —
you'd otherwise need a full OIDC provider wired up just to *see* the
page. The `dev-ui` mise task short-circuits that:

```bash
mise run dev-ui
```

This boots the real rama gateway on `127.0.0.1:8080` against:
- an in-memory SQLite,
- an in-process `wiremock` upstream that serves both
  `POST /chat/completions` (two SSE deltas + `[DONE]`) and
  `POST /audio/transcriptions` (stubbed JSON), wired to a `demo-model`
  chat route and a `demo-whisper` transcription route,
- a pre-seeded `dev@example.com` session.

It prints the signed session cookie on startup, e.g.:

```
dev gateway listening on http://127.0.0.1:8080
seed cookie (paste into playwright / curl):
    id=03aab419…
```

### From playwright

The CLI lives at `mise run playwright-cli` if installed via mise, or
just `playwright-cli` on PATH. It can't set cookies directly via
`goto`, so the canonical flow is:

```bash
playwright-cli open http://127.0.0.1:8080/login                  # any page on the origin
playwright-cli eval "() => { document.cookie = 'id=…; Path=/'; }" # paste the cookie
playwright-cli goto http://127.0.0.1:8080/chat
```

From there you can `fill`, `click`, `eval`, `snapshot`, etc. against
the real page with real datastar SSE streaming. The wiremock backend
makes every prompt resolve in ~no time, so the full submit → SSE →
DOM-update cycle is observable without flake.

### From curl

```bash
COOKIE='id=…'
curl -b "$COOKIE" -X POST \
  --data-urlencode model=demo-model \
  --data-urlencode message="hi" \
  http://127.0.0.1:8080/chat/stream
```

### Why not just patch out the auth?

The point of using a seeded session over a feature-flag bypass is that
every code path under test (cookie parsing, session lookup, RBAC,
flash cookies, datastar's preventDefault, …) is the same one
production runs. The only thing faked is the upstream LLM.

## CI

GitHub Actions is wired up in `.github/workflows/ci.yml`. Two jobs:

1. **ci** — runs `mise run ci`, which fans out via mise's DAG to lint + test + release-build (each transitively depending on `build-css`). The toolchain comes from `mise.toml` via `jdx/mise-action`; `Swatinem/rust-cache` caches the cargo registry + `target/` across runs. The job uploads `target/release/gateway` (and `target/release/typst`) as an artifact.
2. **container** — builds the production image from `/Dockerfile` with `docker/build-push-action`. On pull requests it builds with `push: false` (validation only). On the default branch and on tags it pushes to GHCR (`ghcr.io/croit/llm-gateway`) with tags from `docker/metadata-action` (branch, tag, `sha-<short>`, and `latest` on the default branch).

The Dockerfile is **runtime-only**: a thin `debian:trixie-slim` that just `COPY`s the binary from the `ci` job's artifact (the CSS bundle and `datastar.js` are `include_bytes!`'d into the binary, so the runtime image needs nothing else). No `cargo`, `npm`, or `tailwindcss` in the image build — that all happens in the `ci` job.

CI never invokes `cargo`, `npm`, or `tailwindcss` directly; everything routes through mise tasks. If you need a new CI step, add a `[tasks.…]` entry to `mise.toml` and call it from the workflow.
