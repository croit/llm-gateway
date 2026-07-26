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
gateway            bin + router/proxy/api/oidc      6.5k  ← thinnest, most-edited glue
   ├── gateway-web     server-rendered HTML pages  25.5k  ← siblings: neither
   └── gateway-tools   the tool implementations    14.5k  ←   depends on the other
          └── gateway-runtime  tool API + AppState/RamaState + chat driver   14.7k
                 ├── gateway-features  RAG, skills, ComfyUI, push, geoip, …  13.9k
                 └── gateway-core      db, config, crypto, rbac, upstreams   22.1k
                        ├── session-core   chat-UI substrate
                        └── shared         OpenAI wire types
```

What that buys, in lines that must recompile after a one-line edit:

| edit site | recompiled |
|---|---|
| pre-split monolith | **97,310** (one unit) |
| `gateway` | 6,510 |
| `gateway-tools` | 21,041 |
| `gateway-web` | 32,017 |
| `gateway-runtime` | 61,266 |
| `gateway-features` | 75,124 |
| `gateway-core` | 97,189 |

The gains are front-loaded deliberately: the layers that churn most (pages, tools,
glue — about 60% of file touches over six months) are the cheapest to rebuild, and
`gateway-core` — the one that still costs a full rebuild — is the least-edited.

**Rule of thumb when adding code:** put it as high in the stack as it will go.
Something only belongs in `gateway-core` if code below the feature layer genuinely
needs it. Pushing a module downward for convenience is what makes builds slow
again.

### `crates/shared`
Pure data types, no I/O:
- OpenAI request/response schema (`ChatCompletionRequest`, `ChatCompletionResponse`, streaming chunk type, tool-call types, audio transcription types).
- Tool descriptors (`ToolDef`, `ToolSchema`), role identifiers, RBAC rule types.
- Gateway error type (rendered identically by server and CLI).

Depends only on `serde`, `serde_json`, `thiserror`.

### `crates/gateway-core`
The base layer — the things everything else stands on, and the least-edited code
in the tree. No routing, no `AppState`, no tool registry:
- `auth/oidc.rs` — hand-rolled OIDC client (discovery + JWKS-verified ID tokens, on reqwest).
- `auth/token.rs` — gateway-token mint/hash helpers.
- `config.rs` — typed `[upstream_pools]`, `[[models]]`, `[oidc]`, `[rbac]` schema.
- `db/` — sqlx; users / tokens / sessions / prefs / usage / …, plus `migrations/` at the crate root, embedded by `db/mod.rs`'s `sqlx::migrate!`.
- `crypto.rs` — AES-256-GCM at-rest sealing for DB-stored secrets.
- `rbac/` — role lookup and grant resolution. It filters grants against the tool and skill registries through the [`GrantableSet`] trait (two methods, used via generics) rather than depending on them, which is what lets RBAC sit at the bottom while the registries live two layers up.
- `upstreams/` — pool registry, backend health probes, RAII `Acquired` guard for in-flight accounting.
- `reasoning.rs`, `model_defaults.rs`, `feature_defaults.rs` — per-model capability and effort tables.
- `tool_naming.rs` — the well-known tool ids/prefixes (`comfyui_`, `typst_`, `enable_tools`, `read_skill`) and the slug→title humaniser. Down here because RBAC, the typst discovery pass, and the catalog all need it and they're on three different layers.
- `usage/`, `limits/` — the metrics sink and the rate-limit/quota enforcer.
- `rama_server/session.rs` — signed-cookie + sqlite session store, plus the `is_safe_return_to` redirect guard the OIDC callback and the page chrome both need; `rama_server/cors.rs` — the CORS layer. Neither needs `AppState`, so both stay here.

### `crates/gateway-features`
The optional subsystems — what a deployment switches on in `gateway.toml` and can
run entirely without: `rag/`, `skills.rs`, `comfyui/` (client, store, manifest,
runner, scheduler), `push/`, `github/`, `geoip/`, `typst.rs`, `image_gen.rs`,
`chat_attachments.rs`, `embeddings.rs`, `speech.rs`, `pdf.rs`, `ocr.rs`,
`search_settings.rs`, and `document_canvas.rs` (the chat canvas renderer, shared
by the chat page above and the document tools above).

Each stands on `gateway-core` and knows nothing about `AppState`, the tool
registry, or routing. That ignorance is the whole point — it's what lets this
layer sit below the runtime. A reference from here up into `gateway-runtime`
collapses the split.

### `crates/gateway-runtime`
Where the world gets tied together:
- `server/tools/` — the tool *machinery*: the `Tool` trait and `ToolContext`, the `ToolRegistry`, the round-loop `runner`, the `catalog` (tool id → group → toggle key), the MCP connection manager, and the sandbox client. Implementations live in `gateway-tools`, above; `echo` and `get_current_timestamp` stay here as the canonical trivial tools that the registry/runner tests build registries out of.
- `server/state.rs` — `AppState`: the db pool, config, `Arc<UpstreamRegistry>`, `Arc<ToolRegistry>`, `Arc<Resolver>`, and the optional feature handles (RAG indexer, skills, ComfyUI, push, geoip, sandbox client, MCP manager).
- `rama_server/state.rs` — `RamaState` wraps `AppState` (via `Deref`) and adds the session store, worker registry, usage sink and rate-limit enforcer; `rama_server/auth.rs` — `require_bearer` for `/v1/*`.
- `openai_driver.rs` — the `session_core::SessionDriver` impl that streams a chat completion, plus `loop_guard.rs`.
- `server/{scheduled,webhooks,compaction,headless}` — the background workers that need state.
- `server/comfyui_tool.rs` — the ComfyUI `Tool`/`ToolSource` impls and the `ComfyuiHandle` that `AppState` holds. Split out of `gateway-features`' `comfyui/` because it needs the tool API.

`gateway-tools` and `gateway-web` both sit on this and neither depends on the
other, so a tool edit and a page edit stay independent.

### `crates/gateway-tools`
The tool implementations — one module per tool family (`fetch_url`,
`fetch_attachment`, `search_web`, `typst_render`, `document`, `rag`, `memory`,
`qr`, `netcheck`, …). Each holds `Tool` impls; they plug into the machinery in
`gateway-runtime` and are registered into the `ToolRegistry` that `gateway`'s
`main.rs` builds.

A pure sink like `gateway-web`, and a sibling of it. Two tests live in
`tests/` rather than beside their code because they span both layers — the
catalog-grouping and `AppState`-authorization tests need the machinery from
`gateway-runtime` *and* the real concrete tools from here. A unit test inside
`gateway-runtime` can't reach them: a `cfg(test)` build of a crate is a separate
crate instance, so its types don't unify with a dependent crate's. That same
constraint is why a handful of test-support helpers (`ToolContext::for_test`,
`pdf::test_support`, `comfyui::Client::with_http`) are plain `pub` rather than
`#[cfg(test)]`.

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
