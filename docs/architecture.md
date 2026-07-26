# Architecture

## One-paragraph summary

The gateway is a single Rust binary built on **rama 0.3**, which is a proxy-native HTTP framework. The same process serves the OpenAI-compatible API (`/v1/*`), the OIDC browser flow (`/auth/*`), the session-authed JSON admin API (`/api/v0/*`), and a server-rendered HTML UI (`/`, `/login`, `/tokens`, `/chat`). HTML templates use the **plait** macro inline in handlers; client-side reactivity is **datastar** (self-hosted, ~34 KB JS) — chat replies stream over SSE and token CRUD uses the same SSE-patch pattern for surgical updates; styling is **daisyUI v5 + Tailwind v4** with a shadcn-flavoured neutral palette.

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
                                │  │  /v1/chat/completions, /v1/audio/...   │──┼──► Upstream pool A (chat)
                                │  │  [bearer auth][rbac][tool injection]   │──┼──► Upstream pool B (whisper)
                                │  │  [tool-call loop]    [model routing]   │──┼──► …
                                │  └────────────────────────────────────────┘  │
                                │                                              │
                                │  SQLite (sessions, gateway tokens, audit)    │
                                └──────────────────────────────────────────────┘
```

## Crate boundaries

The gateway is one binary assembled from a layered stack of crates under
`crates/`. The layering is load-bearing for dev-build speed, not just tidiness:
the `gateway` crate used to be ~108k lines in a single compilation unit, so
editing *any* file re-ran the whole frontend + codegen. Each crate below depends
only on the ones beneath it, so an edit recompiles that crate and what sits above
it — never what sits below.

```
gateway          bin + router/proxy/api/oidc      ← thinnest, most-edited glue
   ├── gateway-web    server-rendered HTML pages   ← pure sink, nothing below it uses it
   └── gateway-core   application body (server/, openai_driver, RamaState)
          ├── session-core   chat-UI substrate
          └── shared         OpenAI wire types
```

**Rule of thumb when adding code:** put it as high in the stack as it will go. A
thing only belongs in `gateway-core` if something below the page layer actually
needs it. Moving a module *down* is what makes builds slow again.

### `crates/shared`
Pure data types, no I/O:
- OpenAI request/response schema (`ChatCompletionRequest`, `ChatCompletionResponse`, streaming chunk type, tool-call types, audio transcription types).
- Tool descriptors (`ToolDef`, `ToolSchema`), role identifiers, RBAC rule types.
- Gateway error type (rendered identically by server and CLI).

Depends only on `serde`, `serde_json`, `thiserror`.

### `crates/gateway-core`
The application body — everything the HTTP surface stands on. Two modules:

**`server/` — framework-neutral building blocks.** No routing here:
- `auth/oidc.rs` — hand-rolled OIDC client (discovery + JWKS-verified ID tokens, runs on reqwest).
- `auth/token.rs` — gateway-token mint/hash helpers.
- `config.rs` — typed `[upstream_pools]`, `[[models]]`, `[oidc]`, `[rbac]` schema.
- `db/` — sqlx, tables for users / tokens / sessions / pending_logins.
- `rbac/` — role lookup + per-user allowed-tool computation.
- `state.rs` — `AppState` (`Arc<UpstreamRegistry>`, `Arc<ToolRegistry>`, `Arc<Resolver>`, db pool, optional `Arc<OidcClient>`, the `reqwest::Client`).
- `tools/` — `Tool` trait, `ToolRegistry`, the round-loop runner, and the tool implementations.
- `upstreams/` — pool registry, backend health probes, RAII `Acquired` guard for in-flight accounting.
- the feature subsystems: `rag/`, `skills.rs`, `comfyui/`, `push/`, `scheduled/`, `limits/`, `usage/`, `geoip/`, `webhooks.rs`, `typst.rs`, `image_gen.rs`, `chat_attachments.rs`.
- `migrations/` (crate root) — the sqlx migration set, embedded by `db/mod.rs`'s `sqlx::migrate!`.

**`rama_server/` — the shared rama handles.** Only what *both* the pages and the
router need, which is why it sits below both:
- `state.rs` — `RamaState` wraps `AppState` (via `Deref`) and adds the `SessionStore`, worker registry, usage sink, and rate-limit enforcer.
- `session.rs` — hand-rolled signed-cookie + sqlite session store (replaces `tower-sessions`), plus the `is_safe_return_to` redirect guard both the OIDC callback and the page chrome's login links need.
- `auth.rs` — `require_bearer` helper for the `/v1/*` routes.
- `cors.rs` — the CORS layer.

`openai_driver.rs` is the `session_core::SessionDriver` implementation that drives
a streaming OpenAI chat completion for the chat pages.

### `crates/gateway-web`
The server-rendered HTML: `pages/`, split per route. `mod.rs` carries the shared
chrome (layout, nav, theme, SSE framing helpers, `Flash`, the session gate,
`/login`, `/theme/toggle`); `chat/` is a directory module for the
multi-conversation chat (handlers in `mod.rs`, renderers in `render.rs`,
auto-titling in `title.rs`); `tokens.rs` owns `/tokens` CRUD; `admin.rs` and its
siblings own the `/admin/*` screens.

This crate is a **pure sink** — nothing in `gateway-core` references it, and only
the router mounts it. Keep it that way: a back-edge from `gateway-core` into a
page would collapse the split. `build_info.rs` (and the `build.rs` that stamps the
git SHA into it) lives here too, because the page chrome is its only consumer and
that keeps a new commit from invalidating `gateway-core`.

### `crates/gateway`
The binary and its routing glue — deliberately thin:
- `router.rs` — builds the `rama::http::service::web::Router`, mounting handlers from `gateway-web` and this crate.
- `proxy.rs` — `/v1/{models,chat/completions,audio/transcriptions,audio/speech,embeddings,images/generations,images/edits}` handlers. The chat path branches between a streaming fast-path (no tool grants) and the buffered tool-call loop; embeddings, images, and speech are byte-dumb relays to their pool kind.
- `api.rs` — session-authed JSON at `/api/v0/*`.
- `oidc_handlers.rs` — `/auth/{login,callback,logout}`, backed by a `pending_logins` row keyed by the OIDC `state` parameter.
- `rag_api.rs`, `sandbox_api.rs`, `comfyui_api.rs` — the remaining JSON surfaces.
- `vad.rs` — neural voice-activity detection, trimming silence off uploaded voice notes before Whisper sees them.

`main.rs` wires it all: config → db → upstreams → tools → rbac → SessionStore →
OIDC → `rama_server::router::serve`. The lib target exists so the integration
tests in `tests/` can build the router and drive it with `router.serve(req)`
without binding a socket.

Static assets (`app.css`, `datastar.js`, `app.js`, `pcm-recorder.js`) are
`include_bytes!`'d and served by `session_core::assets` at a
`?v=<sha256-prefix>` versioned URL with `Cache-Control: immutable`.

## Request flow: `POST /v1/chat/completions`

1. **`rama_server::auth::require_bearer`** validates `Authorization: Bearer gwk_…` against the `tokens` table, resolves the user. 401 on miss.
2. **RBAC** (`state.rbac`) maps the user's OIDC roles → role IDs → set of allowed tool IDs.
3. **Branch on the request body:**
   - *Fast path* — no allowed tools. Nothing to inject, so resolve `model` → pool → backend via `state.upstreams.acquire_for`, then `forward_streaming` wraps the upstream's `bytes_stream()` in a `rama::http::Body::from_stream`. The `Acquired` guard rides inside the stream's scan closure so the in-flight slot stays held for the lifetime of the response. (A client-supplied `tools` array does *not* divert here — when the user has grants we take the tool path and union ours in.)
   - *Tool path* — taken whenever the user has tool grants, including when the client brought its own `tools` (unioned in, de-duped by name). The runner in `server::tools::runner` injects tool defs, forces `stream: false`, and loops: acquire pool → forward → if the turn's `tool_calls` are gateway-owned *only*, execute them concurrently and feed the results back as `role: "tool"` messages → re-POST. A turn that calls any client-owned tool is returned to the client unchanged (it drives its own tools). Bounded at 10 rounds. Final response carries an `x-gateway-tool-rounds` header.
4. **`Acquired::drop`** releases the in-flight slot. The pool's atomic counter decrements on the next pick.

## Request flow: chat page (datastar SSE)

1. The browser submits the chat composer form with `@post('/chat/{id}/messages', {contentType: 'form'})` — datastar sends `application/x-www-form-urlencoded` and expects `text/event-stream` back.
2. `pages::chat_message_send` validates the form, resolves the user (session cookie), and confirms they own the session.
3. It registers a per-user **worker** slot (a broadcast channel keyed by the user id). A second concurrent submit for the same user gets a "still streaming" error rather than a parallel stream.
4. It persists the user turn + an `in_progress` assistant turn, auto-titles the session (a heuristic title synchronously, then a background LLM-generated one), and spawns the assistant worker. The worker drives the model — including the tool-call loop and reasoning — and pushes `TurnUpdate`s onto its broadcast.
5. The handler returns an SSE response subscribed to that broadcast. The first event appends a user bubble + an empty assistant bubble to `#conversation`; subsequent `datastar-patch-elements` events patch the assistant turn with the accumulated content, reasoning, and tool-call state, plus sidebar-row updates. The stream closes when the worker finalizes the turn.
6. `GET /chat/{id}/tail` (`pages::chat_tail`) lets a client that reconnected or reloaded mid-stream re-attach to a live worker's broadcast. If no worker is live for the session it signals `chatStreaming:false` and closes, so the viewer just sees the static snapshot.

## Configuration

Single TOML file. Location resolved in this order: `$GATEWAY_CONFIG` env var → `./gateway.toml` → `/etc/gateway/config.toml`. Secret material (OIDC client secret, session HMAC key) is **only** read from env vars referenced *by name* in the TOML (`api_key_env = "GPU01_KEY"`), never inline in the config file.

See the per-subsystem docs for the exact config shape.
