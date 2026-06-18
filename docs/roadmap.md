# Roadmap

Phased plan. Each phase is small enough to land, fully tested, in a handful of PRs. **Don't start phase N+1 until phase N's "Done when" criteria are green.**

> **Stack note (2026-05).** Phases 0–6 below describe the original Dioxus + Axum + tower-sessions stack as it landed. Phase 7 (Chat UI) was the trigger for a full rewrite onto **rama 0.3 + plait + daisyUI/Tailwind v4 + datastar**, captured under "Phase 8 — rama rewrite" at the bottom. The phase notes above are preserved as a record of what shipped *under* each phase, not what the current code looks like — the architecture-of-record is in `docs/architecture.md`.

## Phase 0 — Bootstrap ✅ current

Docs, toolchain, workspace manifest.

- [x] `AGENTS.md`, `README.md`, `docs/*` written.
- [x] `mise.toml` with toolchain pin + tasks.
- [x] Root `Cargo.toml` workspace stub (no members yet).
- [x] `.gitignore`.

**Done when**: `mise install` succeeds and the docs cover every locked decision from kickoff.

## Phase 1 — Skeleton crates + healthz ✅ scaffolded

- [x] Created `crates/shared`, `crates/gateway`, `crates/cli` (hand-written, no `dx new` template; no npm).
- [x] Gateway has `/healthz` and `/readyz` Axum routes (200 OK).
- [x] Gateway has one Dioxus page (`/`) with the "It works" placeholder.
- [x] CLI has `gw --version` and `gw ping` (hits `/healthz`, checks body).
- [x] Tests: in-process `oneshot` tests for `/healthz`, `/readyz`, 404; CLI tests against a real `tokio::spawn`-ed axum server covering success/wrong-status/wrong-body/dead-address.
- [x] `mise run lint` green (`cargo fmt --check` + `clippy -D warnings`).
- [x] Gateway tests green via `cargo test --package gateway --features server`.
- [ ] `mise run dev` smoke-tested locally — to be verified by the user once they run the toolchain on their machine.
- [ ] CLI ping tests verified locally — they require loopback TCP, which works on normal workstations and CI but is blocked in some sandboxes.

**Done when**: `mise run dev` serves the page locally and `mise run cli -- ping` reports OK against it.

## Phase 2 — Passthrough OpenAI proxy ✅ scaffolded

- [x] `[upstream]` config schema with a single backend (TOML + env-var override for secrets). Multi-pool is Phase 4.
- [x] `POST /v1/chat/completions` forwards body to the configured upstream and streams the response back (SSE works via `Body::from_stream` over `reqwest::Response::bytes_stream()`).
- [x] `GET /v1/models` forwards to the upstream as-is.
- [x] OpenAI-shaped error envelope for gateway-side failures (`upstream_not_configured` → 503, `upstream_unreachable` → 502, `internal_error` → 500). Upstream 4xx/5xx responses are relayed verbatim.
- [x] Tests: 9 unit tests (config parser, header denylists) + 3 healthz integration + 9 wiremock-backed proxy tests covering happy path, error relay, missing config, auth injection/drop, SSE body+content-type, models list, header forwarding.
- [x] `mise run lint` green; in-process tests green. Wiremock-backed tests require loopback TCP — will run in CI / on workstation.

**Done when**: `OPENAI_BASE_URL=http://localhost:8080/v1 openai api chat_completions.create -m … -g 'hi'` works end-to-end against the dev gateway (verify locally once Phase 3 lands and we have a real upstream pointed at).

## Phase 3 — OIDC + CLI loopback handoff + gateway tokens ✅ scaffolded

- [x] OIDC dependency wiring (`openidconnect 4`), `[oidc]` + `[gateway]` config blocks.
- [x] Browser flow: `/auth/login`, `/auth/callback`, `/auth/logout` via `tower-sessions` + sqlite store.
- [x] CLI flow: `/auth/cli/start`, `/auth/cli/begin`, `/auth/cli/poll` with PKCE between CLI and gateway.
- [x] Gateway tokens table + bearer middleware on `/v1/*` (Part 1). Bearer-auth identity endpoints `/v1/me` and `/v1/auth/logout`.
- [x] Token-management endpoints `/api/v0/me`, `/api/v0/tokens` (list/create/revoke), session-authenticated.
- [x] `gw auth login` (polling-based loopback handoff), `gw auth whoami`, `gw auth logout`.
- [x] Credentials file at `~/.config/gw/credentials.toml` (mode 0600 on unix).
- [x] Tests: 41 gateway unit + 6 CLI unit + 9 session-route integration + 7-of-13 proxy (rest need TCP). 0 mock-OIDC end-to-end yet — that needs a wiremock-shaped OIDC discovery doc and is the highest-value Phase 3 follow-up.

**Done when**: `gw auth login` against the dev gateway with a local Keycloak yields a working bearer that can call `/v1/chat/completions`. Outstanding: web UI for token management (Phase 4) and mock_oidc end-to-end test (still TODO).

## Phase 4 — Web UI for token management ✅ scaffolded

- [x] Dioxus Router with three routes: `/`, `/login`, `/tokens`.
- [x] Shared `Header` nav with sign-out form (POST /auth/logout).
- [x] `/` Dashboard renders user info from `/api/v0/me` or an "Sign in" CTA if anonymous.
- [x] `/tokens` lists tokens (name, created, last used, expires, status), inline create form (name + TTL days), revoke buttons, just-minted plaintext banner with `user-select: all` for easy copy.
- [x] `/login` page with a button that links to `/auth/login`.
- [x] WASM-only `gloo-net` HTTP client, gated via `[target.'cfg(target_arch = "wasm32")'.dependencies]` so it never enters the native binary. SSR-side stubs return `pending` futures so the page hydrates and refetches on the client.
- [x] Shared API response types in `crates/shared/src/api.rs` so server, CLI and web all deserialise the same structs.
- [x] Plain CSS styling in `assets/main.css` (dark theme, no Tailwind).
- [ ] WASM-level component tests (defer to when tests fail to surface a real bug; existing /api/v0/* tests cover the data layer).

**Done when**: signed in via OIDC, a user can navigate to `/tokens`, mint a token, copy the plaintext, and revoke an old one — all without leaving the page.

## Phase 5 — Multi-provider routing + Whisper ✅ scaffolded

- [x] Full `[upstream_pools.*]` schema with multiple pools, multiple backends, strategies.
- [x] Health check loop.
- [x] Model → pool resolution (exact + longest prefix).
- [x] `POST /v1/audio/transcriptions` passthrough multipart (parses `model` from multipart, routes via the same `[[models]]` table, forwards original bytes preserving the boundary).
- [x] In-flight accounting + `429` (`upstream_unreachable`) on saturation via the `Acquired` RAII guard.
- [x] Tests: 14 picker/registry tests + 7 transcription multipart tests.

**Done when**: a request for `llama-3.1-…` lands on `gpu-01` or `gpu-02` per the picker, with health-check-driven failover demonstrated in a test.

## Phase 6 — Tools + RBAC ✅ scaffolded

- [x] `Tool` trait + `ToolRegistry`. Object-safe via a hand-rolled `ToolFuture` so we skip `async_trait`. Builder-style `.with()` registration, panics on duplicate ids.
- [x] Two example tools: `Echo` (smoke-test fixture, returns its `message` argument) and `CurrentTimestamp` (real implementation, returns time in UTC or any IANA timezone via jiff). DB-touching company tool comes when there's a real domain DB.
- [x] `[rbac]` + `[[roles]]` config. OIDC-claim → role mapping with default-role baseline. Pattern matching on models (`*` and trailing `*` prefix); `*` on tools expands to the registry.
- [x] Tool injection on `/v1/chat/completions` (union with client-supplied, dedupe by function name).
- [x] Tool-call loop bounded at 10 rounds, concurrent within a round (semaphore 4), each tool capped at 30s. Tool failures come back as `{error: …}` content on the tool message so the model can recover.
- [x] `gw auth tools` CLI command listing the user's granted tools.
- [x] Unit + integration tests (12 runner tests, 11 RBAC tests, 15 tool tests). End-to-end wiremock-backed tool-call test is the obvious follow-up; the runner tests cover the loop logic in isolation.
- [ ] Final-round streaming when client requests `stream: true` AND tool calls happened — currently the response is non-streaming JSON when any tool round ran. Listed in `runner.rs` as a follow-up.

**Done when**: a chat completion as a user whose role grants `get_current_timestamp` against a tool-calling model invokes the tool and the model produces a coherent final answer. Verified by the runner tests in isolation; end-to-end wiremock test is outstanding.

## Phase 7 — Chat UI (under old stack; superseded by Phase 8)

Landed end-to-end on Dioxus + Axum SSE + server fns + WASM-side fetch. All bullets below shipped, but the whole surface was rewritten under Phase 8. The original goals — composer, model picker, streaming assistant reply, speak-to-compose — are all preserved in the rama port; the implementation shape is what changed.

- [x] `/chat` route, inline model picker against `/api/v0/models`, composer.
- [x] SSE streaming the assistant reply via WASM `ReadableStream` + OpenAI-shaped delta parser.
- [x] Speak-to-compose: `getUserMedia` → `MediaRecorder` → multipart POST to `/api/v0/transcriptions`.
- [x] Persist conversation history (server-side, per user) — landed in the rama-port phase with the multi-conversation refactor.
- [x] Tool-call disclosure: render gateway-side tool calls/results inline when present — landed with the persisted-chat refactor; the `chat_tool_calls` table backs collapsible per-call rows.

## Phase 8 — rama rewrite ✅ landed on `main`

Triggered by "what if we went with a completely different architecture and used rama and its built-in support for datastar?" — answered with a full rewrite. The old `dioxus` / `axum` / `tower-sessions` / `dioxus-primitives` stack is gone from the runtime tree.

- [x] All server-side HTTP runs on rama 0.3.0-alpha.4 (`http-full` features). Server bind, router, request/response types, multipart, SSE bodies — all rama.
- [x] Pages are server-rendered via `plait::html! { ... }` inline in handlers. No WASM, no client-side router, no hydration.
- [x] Live UI updates ride **datastar v1.0.1** SSE — `datastar-patch-elements` events with `selector` / `mode` / `elements`, plus `datastar-patch-signals` for reactive state transitions (end-of-stream flag flips, etc.). The chat stream lives on `POST /chat/{id}/messages` with a parallel `GET /chat/{id}/tail` for reconnect (distinct from the OpenAI-compatible `POST /v1/chat/completions`). In-page navigation and theme toggle ride the same patch envelope (no full reloads).
- [x] Styling: **daisyUI v5** component classes on **Tailwind v4**, with a shadcn-flavoured theme overlay (`ui/src/main.css` defines `light`/`dark` plugins + unlayered overrides for `.btn`, `.input`, `.card`, `.badge`, toast). Compiled into a single static `app.css` by the mise-installed `tailwindcss-cli`; no `node_modules` at runtime.
- [x] Sessions are hand-rolled — HMAC-SHA256-signed cookie + sqlite `sessions` row + `pending_logins` row keyed by OIDC `state`. Replaces `tower-sessions` entirely.
- [x] Auth is resolved inline at the top of each rama handler (not a middleware layer): page routes 303 to `/login` on miss, API routes 401-JSON.
- [x] Tool-call loop ported to the rama proxy handler. `x-gateway-tool-rounds: N` header on tool-path responses; fast-path streams straight through.
- [x] Integration test suite rebuilt against `router(state).serve(req)` direct-call (no socket binding).
- [x] OIDC dance covered end-to-end by a wiremock IdP + an RSA-signed ID token in `tests/oidc_integration.rs`. Tool loop covered by a stateful wiremock responder in `tests/tool_loop.rs`. Datastar SSE wire shape covered in `tests/chat_stream.rs`.
- [x] `Dockerfile`, GitLab CI (including sccache wiring), and `mise.toml` rebuilt around the rama-only path.
- [x] `rama-spike` merged to `main`.
- [x] Persist conversation history (multi-session, DB-backed, with resume-on-reconnect — see [docs/ui.md](ui.md#persisted-chat--multi-session-resume-on-reconnect)).
- [x] Tool-call disclosure: each `tool_call` lands in `chat_tool_calls` and renders as a collapsible row with input + output JSON; reasoning blocks (`<details>` with "Thought for X.Xs") render the same way.

---

## Known bugs / follow-ups

Real bugs we've observed but deferred. Each has a test or code comment pointing
back here so we don't lose track.

- **Final-round streaming when tools were used.** The proxy currently swallows
  the streaming flag when at least one tool round ran, returning the final
  assistant turn as a single JSON body. The upstream `stream: true` request is
  honored on tool-call rounds (because we need the full body to dispatch the
  call) but the *final* round should still stream to the client if the caller
  asked for it. Plumbing this through the round loop is non-trivial — flagged
  in `runner.rs` as well.

## Anti-goals (for now)

- Service-to-service / machine-account auth.
- Token scopes (per-token RBAC narrowing).
- Tool result caching.
- MCP bridging.
- Multi-tenant isolation beyond per-user RBAC.
- Prometheus metrics endpoint (will be added when we hit operational scale).

These are deliberate omissions, not oversights. Adding them prematurely creates abstractions we can't justify yet.
