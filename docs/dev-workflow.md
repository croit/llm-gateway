# Dev workflow

## Toolchain

Everything is pinned in `mise.toml`. Run `mise install` once after cloning. It installs:

- The **Rust** toolchain pinned to `1.95` (with the `rustfmt`, `clippy`, and `cargo` components).
- **`cargo-binstall`** — used behind mise's `cargo:` backend to install Rust binaries quickly (prebuilt when available, source build as fallback).
- **`sccache`** — an rustc wrapper for compilation caching. It's installed but **OFF by default** (no `RUSTC_WRAPPER` is set). Opt in locally with `RUSTC_WRAPPER=sccache` if you want it.
- **Node 24** — needed for `ui/`'s Tailwind v4 + daisyUI CSS/JS build.
- **`typst` 0.15.0** — the CLI backing the `typst_<template>` tools. The `fetch-typst-cli` task copies mise's installed binary into `target/release/typst` so the release build and runtime image pick it up through the same artifact pipeline as the gateway binary.

We **do not** check in a `rust-toolchain.toml`; mise is the single source of truth.

## Daily commands

The gateway binary `include_bytes!`s its static assets (`app.css`, `datastar.js`, `app.js`, `pcm-recorder.js` — see `crates/session-core/src/assets.rs`), so it won't compile without freshly built bundles in `crates/session-core/assets/`. The mise tasks that build or run the binary (`dev`, `dev-build`, `build`, `dev-ui`, `test`, `lint`) all depend on the composite **`build-assets`** task (which runs `build-css` + `build-js`), so a fresh checkout needs no manual asset step — pick a goal and run it.

| Goal | Command |
|---|---|
| Run gateway against local config | `mise run dev` |
| Run a stub gateway for UI debugging (seeded session, mock LLM) | `mise run dev-ui` |
| Build the gateway debug binary (no run) | `mise run dev-build` |
| Fast type-check across the workspace | `mise run check` |
| **Release** build (slow, for deploys) | `mise run build` |
| Tests | `mise run test` |
| Tests with stdout visible | `mise run test-nocapture` |
| Lint (clippy `-D warnings` + `fmt --check` + `tsc --noEmit`) | `mise run lint` |
| Apply Rust formatting | `mise run fmt` |
| Tailwind / daisyUI CSS — one-shot | `mise run build-css` |
| Tailwind / daisyUI CSS — live rebuild | `mise run watch-css` |
| Bundle the TypeScript page glue — one-shot | `mise run build-js` |
| Bundle the TypeScript page glue — live rebuild | `mise run watch-js` |
| TypeScript type-check only (`tsc --noEmit`) | `mise run typecheck` |
| Everything CI runs (lint + test + release build) | `mise run ci` |
| Enable the version-controlled git hooks (pre-push CI gate) | `mise run setup-hooks` |
| Run the CLI | `mise run cli -- auth login` |

**Debug vs release.** `mise run build` (release) takes ~12 s cold-incremental and ~70 s from clean — only use it when you actually want optimised output (deploys, perf measurement). For day-to-day iteration (running locally, screenshotting pages, smoke-testing changes) use `mise run dev` or `mise run dev-build`; those produce a debug binary in ~2 s incremental (vs ~11 s for a release build). Runtime perf is identical for any UX you'd interact with; only synthetic benchmarks notice the difference.

`mise run setup-hooks` points `core.hooksPath` at `.githooks/`, which installs a pre-push gate that runs CI before a push lands.

Anything not covered: add a task to `mise.toml` rather than typing the raw command into a script. Discoverability matters.

## Layout while developing

`mise run dev` runs `cargo run --package gateway`. On startup the binary:

- binds the address from the `IP` / `PORT` env vars (defaults `127.0.0.1` / `8080`);
- resolves its config file in this order: `$GATEWAY_CONFIG` → `./gateway.toml` → `/etc/gateway/config.toml` (see `Config::resolve_path` in `crates/gateway/src/server/config.rs`). If none is found it boots with built-in defaults (no upstreams, no OIDC);
- opens the SQLite database at `[db].path` (default `gateway.sqlite`) and runs migrations;
- builds the upstream registry and spawns the health probes;
- builds the OIDC client if an `[oidc]` block is configured, otherwise starts without login.

For local dev, copy the committed reference config and edit it:

```bash
cp gateway.example.toml gateway.toml
$EDITOR gateway.toml   # set at least one [upstream_pools.*] backend (and [oidc] to sign in)
mise run dev
```

There's no WASM step, no `dx`, no hot reload of HTML — the rama server serves plain server-rendered HTML and reloads happen via the browser's refresh button. The asset bundles rebuild live if you run `mise run watch-css` and/or `mise run watch-js` in separate terminals, so style/JS changes appear after one refresh. (The committed bundles mean the watchers are optional for plain backend work.)

The CLI is run independently via `mise run cli -- <args>`. It currently supports `ping` and `auth` (`login` / `whoami` / `logout`). It needs a gateway running somewhere — point it at the dev server with the `--gateway` flag or the `$GW_GATEWAY_URL` env var (default `http://localhost:8080`).

## Environment

Env config is layered through mise, not a `.env` file:

- **`mise.toml` `[env]`** holds the non-secret defaults committed to the repo (`RUST_BACKTRACE=1`, `RUST_LOG=info,gateway=debug,cli=debug`).
- **`mise.local.toml` `[env]`** holds secrets and machine-local overrides — it is **gitignored**. This is where local dev keys go: `GATEWAY_SESSION_KEY`, `GATEWAY_OIDC_CLIENT_SECRET`, provider keys (`OPENAI_API_KEY`, `ZAI_API_KEY`, …), `BRAVE_SEARCH_API_KEY`, `SEARCH_PROVIDER`, etc.

Secrets never live in `gateway.toml`. The config holds only the *names* of environment variables (e.g. `api_key_env = "GPU01_KEY"`, `session_key_env = "GATEWAY_SESSION_KEY"`); the gateway reads the actual values from its environment at startup.

Which env vars each subsystem needs is documented in `docs/auth.md` (OIDC) and `docs/upstreams.md` (provider keys).

`GATEWAY_SESSION_KEY` — 64 hex chars (32 bytes) for the session-cookie HMAC. When it's unset the binary falls back to an ephemeral random key with a warning; that's fine locally but every restart invalidates open sessions. Generate a stable one with `openssl rand -hex 32`.

## Debugging the UI

Every authed page (`/`, `/tokens`, `/chat`, `/theme/toggle`, the `/admin/*` and `/rag` screens, the `/api/v0/*` JSON routes) is gated by OIDC, which makes ad-hoc browser debugging (browser automation, devtools, screenshotting bugs) annoying — you'd otherwise need a full OIDC provider wired up just to *see* the page. The `dev-ui` mise task short-circuits that:

```bash
mise run dev-ui
```

This runs the `dev_ui` example (`crates/gateway/examples/dev_ui.rs`), which boots the real rama gateway on `127.0.0.1:8080` against:

- an **in-memory SQLite**;
- an in-process **`wiremock` chat pool** that serves `GET /models` (advertising `demo-model` + `demo-model-pro`) and `POST /chat/completions` (a streaming variant emitting two SSE deltas + `[DONE]`, plus non-streaming and feedback-extraction variants);
- an in-process **`wiremock` transcription pool** that serves `GET /models` (advertising `demo-whisper` + `demo-whisper-large`) and `POST /audio/transcriptions` (a stubbed JSON response);
- a pre-seeded **`dev@example.com`** user with an `admin` role (every model / tool / skill granted), the `examples/demo-skills` bundle loaded, and representative demo data (a finished chat conversation, scheduled actions, RAG collections, and an MCP connector catalog) so the pages render populated.

It's a local-only convenience — not a test target, and not run by CI.

It prints the signed session cookie on startup, e.g.:

```
dev gateway listening on http://127.0.0.1:8080
seed cookie (paste into playwright / curl):
    id=03aab419…
```

### From curl

Paste the cookie to reach any authed page or endpoint:

```bash
COOKIE='id=…'
curl -b "$COOKIE" http://127.0.0.1:8080/chat        # any authed GET page
```

The chat composer submits to `POST /chat/{id}/messages` (create a session first with `POST /chat/sessions`), which streams the reply back as datastar SSE. The wiremock backend resolves every prompt in ~no time, so the full submit → SSE → DOM-update cycle is observable without flake.

### From a browser / automation

Open any origin page (e.g. `http://127.0.0.1:8080/login`), then inject the cookie via devtools (`document.cookie = 'id=…; Path=/'`) or your automation tool's cookie API, and navigate to the page you want. From there the page runs with real datastar SSE streaming against the mock backend.

The repo's README/docs screenshots are produced this way — see the `take-screenshots` helper under `.claude/skills/take-screenshots/`, which drives Playwright with the seeded cookie.

### Why a seeded session instead of patching out auth?

Every code path under test (cookie parsing, session lookup, RBAC, flash cookies, datastar's preventDefault, …) is the same one production runs. The only things faked are the upstream LLM and the OIDC handoff.

## CI

GitHub Actions is wired up in `.github/workflows/ci.yml`. It triggers on pushes to `main`, on tags, and on pull requests. The toolchain comes from `mise.toml` via `jdx/mise-action`; `Swatinem/rust-cache` caches the cargo registry + `target/` across runs (CI does **not** use sccache). There are four jobs:

1. **ci** — runs `mise run ci`, which fans out via mise's DAG to lint + test + release-build (each transitively depending on `build-assets`). It then builds the `sandbox-runner` binary and uploads the `gateway-binaries` artifact: `target/release/{gateway, sandbox-runner, typst, libpdfium.so}` (7-day retention). Debuginfo is dropped from the dev/test profiles (`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`) so the multi-profile compile doesn't run the runner out of disk.
2. **container** (needs `ci`) — downloads the artifact and builds the production image from `/Dockerfile` with `docker/build-push-action`. On pull requests it builds with `push: false` (validation only). On the default branch and on tags it pushes to GHCR (`ghcr.io/croit/llm-gateway`) with tags from `docker/metadata-action` (branch, tag, `sha-<short>`, and `latest` on the default branch).
3. **sandbox-image** (needs `ci`, `push` events only) — builds and pushes the code-execution sandbox gold image (`ghcr.io/croit/llm-gateway-sandbox`) from `sandbox-image/Containerfile`.
4. **sandbox-runner-image** (needs `ci`, `push` events only) — builds and pushes the sandbox runner image (`ghcr.io/croit/llm-gateway-sandbox-runner`) from `deploy/sandbox-runner/Containerfile`.

The two sandbox images are large and slow to build, so they only run on `push` (main/tags), never on PRs. See `docs/sandbox.md`.

The production `Dockerfile` is **runtime-only** — it compiles nothing. Starting from `debian:trixie-slim`, it:

- `apt-get install`s `git` + `ca-certificates` (the RAG indexer shells out to `git clone`, which validates TLS via the OS trust store, not the Rust binary's baked-in `webpki-roots`);
- `COPY`s the prebuilt `gateway` binary, plus `typst` (→ `/usr/local/bin/typst`), `libpdfium.so` (→ `/usr/local/lib/`), and the sample `examples/typst-templates` (→ `/opt/typst-templates`);
- runs as a non-root `gateway` user and exposes `8080`.

The CSS, `datastar.js`, and JS bundles are `include_bytes!`'d into the binary, so the runtime image ships no separate asset directory. No `cargo`, `npm`, or `tailwindcss` runs in the image build — those all happen in the `ci` job, and the binaries arrive as artifacts.

CI never invokes `cargo`, `npm`, or `tailwindcss` directly; everything routes through mise tasks. If you need a new CI step, add a `[tasks.…]` entry to `mise.toml` and call it from the workflow.
