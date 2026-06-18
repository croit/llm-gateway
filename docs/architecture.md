# Architecture

## One-paragraph summary

The gateway is a single Rust binary built on **rama 0.3**, which is a proxy-native HTTP framework. The same process serves the OpenAI-compatible API (`/v1/*`), the OIDC browser flow (`/auth/*`), the session-authed JSON admin API (`/api/v0/*`), and a server-rendered HTML UI (`/`, `/login`, `/tokens`, `/chat`). HTML templates use the **plait** macro inline in handlers; client-side reactivity is **datastar** (self-hosted, ~34 KB JS) — chat replies stream over SSE and token CRUD uses the same SSE-patch pattern for surgical updates; styling is **daisyUI v5 + Tailwind v4** with a shadcn-flavoured neutral palette. A separate `cli` crate ships as `gw`, talks to the gateway over HTTP, and uses a loopback-handoff flow to acquire a gateway-minted API token.

## Diagram

```
                                ┌──────────────────────────────────────────────┐
                                │            Gateway (Rust, rama 0.3)          │
                                │                                              │
   Browser ───── HTTPS ────────►│  ┌─────────────────────┐  ┌───────────────┐  │
                                │  │  /  /login /tokens  │  │  /auth/*      │──┼──► OIDC provider
                                │  │  /chat (datastar)   │  │  OIDC flow    │  │   (Keycloak/Authentik/…)
                                │  └─────────────────────┘  └───────────────┘  │
                                │                                              │
   OpenAI SDK ── HTTPS ────────►│  ┌────────────────────────────────────────┐  │
   or `gw` CLI                  │  │  /v1/chat/completions, /v1/audio/...   │──┼──► Upstream pool A (chat)
                                │  │  [bearer auth][rbac][tool injection]   │──┼──► Upstream pool B (whisper)
                                │  │  [tool-call loop]    [model routing]   │──┼──► …
                                │  └────────────────────────────────────────┘  │
                                │                                              │
                                │  SQLite (sessions, gateway tokens, audit)    │
                                └──────────────────────────────────────────────┘
```

## Crate boundaries

Three crates live under `crates/`:

### `crates/shared`
Pure data types, no I/O:
- OpenAI request/response schema (`ChatCompletionRequest`, `ChatCompletionResponse`, streaming chunk type, tool-call types, audio transcription types).
- Tool descriptors (`ToolDef`, `ToolSchema`), role identifiers, RBAC rule types.
- Gateway error type (rendered identically by server and CLI).

Depends only on `serde`, `serde_json`, `thiserror`.

### `crates/gateway`
The single gateway binary. Split into two modules at the top level:

**`server/` — framework-neutral building blocks.** No rama imports here:
- `auth/oidc.rs` — hand-rolled OIDC client (discovery + JWKS-verified ID tokens, runs on reqwest).
- `auth/token.rs` — gateway-token mint/hash helpers.
- `config.rs` — typed `[upstream_pools]`, `[[models]]`, `[oidc]`, `[rbac]` schema.
- `db/` — sqlx, tables for users / tokens / sessions / pending_logins / cli_logins.
- `rbac/` — role lookup + per-user allowed-tool computation.
- `state.rs` — `AppState` (`Arc<UpstreamRegistry>`, `Arc<ToolRegistry>`, `Arc<Resolver>`, db pool, optional `Arc<OidcClient>`, the `reqwest::Client`).
- `tools/` — `Tool` trait, `ToolRegistry`, the round-loop runner.
- `upstreams/` — pool registry, backend health probes, RAII `Acquired` guard for in-flight accounting.

**`rama_server/` — rama-flavoured I/O surface.** All rama/plait imports live here:
- `state.rs` — `RamaState` wraps `AppState` (via `Deref`) and adds the `SessionStore`.
- `router.rs` — builds the `rama::http::service::web::Router`.
- `auth.rs` — `require_bearer` helper for the `/v1/*` routes.
- `session.rs` — hand-rolled signed-cookie + sqlite session store (replaces `tower-sessions`).
- `proxy.rs` — `/v1/{models,chat/completions,audio/transcriptions}` handlers. The chat path branches between a streaming fast-path (no tool grants) and the buffered tool-call loop.
- `api.rs` — session-authed JSON at `/api/v0/{me,tokens,tokens/{id}/revoke,tokens/{id}}`.
- `pages/` — plait-rendered HTML, split per route. `mod.rs` carries the shared chrome (layout, nav, theme, SSE framing helpers, `Flash`, the session gate, `/login`, `/theme/toggle`); `chat/` is a directory module for the multi-conversation chat (handlers in `mod.rs`, streaming worker in `worker.rs`, renderers in `render.rs`); `tokens.rs` owns `/tokens` CRUD; `dashboard.rs` owns `/`.
- `chat_workers.rs` — per-user registry of in-flight chat workers (cancel flag + `broadcast::Sender<TurnUpdate>`). One worker per user max; the messages handler refuses concurrent submits, the tail handler attaches to the existing worker for reconnects.
- `oidc_handlers.rs` — `/auth/{login,callback,logout}`. Replaces the tower-sessions key/value bag with a `pending_logins` row keyed by the OIDC `state` parameter.
- `cli_handlers.rs` — `/auth/cli/{start,begin,poll}` for the `gw auth login` loopback.
- `assets.rs` — `include_bytes!`'d `app.css` (Tailwind + daisyUI bundle) + `datastar.js` + `app.js` + `pcm-recorder.js`. Each is served at a `?v=<sha256-prefix>` versioned URL with `Cache-Control: immutable`.

`main.rs` wires it all: config → db → upstreams → tools → rbac → SessionStore → OIDC → `rama_server::router::serve`.

### `crates/cli`
Ships as the `gw` binary. Modules:
- `cmd::auth` — `login`, `logout`, `whoami`. Implements the loopback handoff: posts `pkce_challenge` to `/auth/cli/start`, opens the browser at the returned URL, polls `/auth/cli/poll` until the gateway has stashed the freshly-minted token plaintext.
- `cmd::ping`, `cmd::models`, `cmd::tools` — list-only, useful for debugging RBAC config.
- `client` — thin HTTP client over reqwest, sets `Authorization: Bearer …`.

Depends on `shared` for response types, never on `gateway`.

## Request flow: `POST /v1/chat/completions`

1. **`rama_server::auth::require_bearer`** validates `Authorization: Bearer gwk_…` against the `tokens` table, resolves the user. 401 on miss.
2. **RBAC** (`state.rbac`) maps the user's OIDC roles → role IDs → set of allowed tool IDs.
3. **Branch on the request body:**
   - *Fast path* — no allowed tools. Nothing to inject, so resolve `model` → pool → backend via `state.upstreams.acquire_for`, then `forward_streaming` wraps the upstream's `bytes_stream()` in a `rama::http::Body::from_stream`. The `Acquired` guard rides inside the stream's scan closure so the in-flight slot stays held for the lifetime of the response. (A client-supplied `tools` array does *not* divert here — when the user has grants we take the tool path and union ours in.)
   - *Tool path* — taken whenever the user has tool grants, including when the client brought its own `tools` (unioned in, de-duped by name). The runner in `server::tools::runner` injects tool defs, forces `stream: false`, and loops: acquire pool → forward → if the turn's `tool_calls` are gateway-owned *only*, execute them concurrently and feed the results back as `role: "tool"` messages → re-POST. A turn that calls any client-owned tool is returned to the client unchanged (it drives its own tools). Bounded at 10 rounds. Final response carries an `x-gateway-tool-rounds` header.
4. **`Acquired::drop`** releases the in-flight slot. The pool's atomic counter decrements on the next pick.

## Request flow: `/chat/stream` (datastar SSE)

1. Browser submits the chat composer form with `data-on-submit__prevent="@post('/chat/stream', {contentType: 'form'})"`.
2. Datastar's client runtime sends `application/x-www-form-urlencoded`, expects `text/event-stream` back.
3. `pages::chat_stream` validates the form, resolves the user (session cookie), acquires a chat-pool backend, and starts an upstream `stream: true` request.
4. First SSE event from the gateway: `datastar-patch-elements` with `mode append` — drops a user bubble + an empty `#active-reply` bubble into `#conversation`.
5. Each upstream `delta.content` triggers another `datastar-patch-elements` event with `mode outer` targeting `#active-reply`, carrying the running accumulated text.
6. On upstream's `data: [DONE]`, a final event swaps off the `id="active-reply"` so the next submission can spawn a fresh bubble. The `Acquired` guard releases.

## Configuration

Single TOML file. Location resolved in this order: `$GATEWAY_CONFIG` env var → `./gateway.toml`. Secret material (OIDC client secret, session HMAC key) is **only** read from env vars referenced *by name* in the TOML (`api_key_env = "GPU01_KEY"`), never inline in the config file.

See the per-subsystem docs for the exact config shape.
