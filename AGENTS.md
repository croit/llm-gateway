# AGENTS.md — read this first

This file is the canonical entry point for any AI agent (or new human contributor) working in this repo. Read it end-to-end before doing anything else.

## What this project is

A single Rust binary plus the supporting crates it lives on:

- **`gateway`** — authenticated, OpenAI-compatible LLM proxy. Speaks `/v1/chat/completions`, `/v1/audio/transcriptions`, `/v1/models` so any OpenAI SDK talks to it. OIDC browser login + gateway-minted bearer tokens. Routes across **multiple upstream LLM backends** with health checks + RAII in-flight accounting. Injects **company-specific tools** gated by **RBAC**. Server-rendered HTML UI (dashboard / tokens / persisted multi-conversation chat).

Shared crates:
- **`session-core`** — chat-style UI substrate (Plait renderers + SSE primitives + DB schema + worker registry + `SessionDriver` trait). The gateway plugs in an `OpenAiDriver`; the trait keeps the renderers driver-agnostic so a future second consumer can paint the same chat surface without forking.
- **`shared`** — OpenAI wire types shared across the workspace.

Built on **rama 0.3** (HTTP server + router + middleware), **plait** (inline-in-handler HTML), **datastar** (client-side reactivity over `datastar-patch-elements` SSE events), and **daisyUI v5 + Tailwind v4** (styling tokens).

## Repo layout

```
/
├── AGENTS.md                    # this file
├── README.md                    # human-facing — keep current with deploy story
├── mise.toml                    # toolchain pin + build/test/lint tasks
├── Cargo.toml                   # workspace manifest (9 members)
├── Dockerfile                   # gateway runtime image
├── docs/                        # detailed design docs (index in docs/README.md)
├── ui/                          # Tailwind v4 + daisyUI v5 → app.css; TS bundle for gateway
├── gateway.example.toml         # template config — copy to gateway.toml
└── crates/
    ├── shared/                  # OpenAI wire types, shared with the CLI
    ├── session-core/            # chat-style UI substrate
    │   ├── src/                     SessionDriver trait, worker registry, db (chat_*
    │   │                            tables), Plait renderers (markdown + lumis-highlighted
    │   │                            code), SSE primitives, icons
    │   └── ui/ts/                   composer + scroll TS
    ├── gateway-core/            # base: db, config, crypto, rbac, upstreams
    ├── gateway-features/        # optional subsystems: rag, skills, comfyui, push, …
    ├── gateway-runtime/         # tool API + AppState/RamaState + chat driver
    ├── gateway-tools/           # the tool implementations
    ├── gateway-web/             # the server-rendered HTML pages
    ├── gateway/                 # the binary: router, proxy, api, main
    └── sandbox-runner/          # the sandboxed-tool execution service
```

### The gateway crate stack

The gateway is one binary assembled from three layered crates. This is **load
bearing for build speed**, not cosmetic: it used to be ~108k lines in one
compilation unit, so editing any file re-ran the whole frontend + codegen. Each
crate depends only on the ones beneath it.

```
gateway            bin + router/proxy/api/oidc      6.5k  ← thinnest, most-edited
   ├── gateway-web     server-rendered HTML pages  25.5k  ← siblings: neither
   └── gateway-tools   the tool implementations    14.5k  ←   depends on the other
          └── gateway-runtime  tool API + AppState/RamaState + chat driver  14.7k
                 ├── gateway-features  RAG, skills, ComfyUI, push, geoip, …  13.9k
                 └── gateway-core      db, config, crypto, rbac, upstreams   22.1k
```

Lines that must recompile after a one-line edit: `gateway` 6.5k, `gateway-tools`
21k, `gateway-web` 32k, `gateway-runtime` 61k, `gateway-features` 75k,
`gateway-core` 97k — against **97k for any edit** before the split. The gains are
front-loaded on purpose: the layers that churn most are the cheapest to rebuild.

`gateway-web` and `gateway-tools` are siblings: neither depends on the other, so
editing a page doesn't rebuild the tools and vice versa.

Two rules keep it that way, and both are easy to break by accident:
1. **Put new code as high in the stack as it will go.** Something belongs in
   `gateway-core` only if code below the feature layer genuinely needs it.
2. **Never reference upward.** `gateway-features` must not name `AppState` or the
   tool registry; `gateway-core` must not name a feature. One such reference
   collapses a layer.

**When adding code, put it as high in the stack as it will go.** Something only
belongs in `gateway-core` if code below the page layer actually needs it. Adding a
reference from `gateway-core` to a page — or pushing a module downward for
convenience — makes every build slow again. See [`docs/architecture.md`](docs/architecture.md#crate-boundaries).

Inside `crates/gateway-core/src/` (base layer):

```
migrations/               # (crate root) sqlx migration set, embedded by db/mod.rs
server/
    auth/oidc.rs              hand-rolled OIDC client (reqwest)
    auth/token.rs             gateway-token mint/hash helpers
    config.rs                 [upstream_pools] + [[models]] + [oidc] schema
    db/                       sqlx; users/tokens/sessions/prefs/usage — chat_* live
                              in session-core, gateway just runs the migration
    crypto.rs                 AES-256-GCM at-rest sealing
    rbac/                     role → tool/model resolution; filters against the
                              registries via the GrantableSet trait, so it can sit
                              at the bottom while they live two layers up
    upstreams/                pool registry, health probes, RAII Acquired guard
    tool_naming.rs            well-known tool ids/prefixes + slug→title humaniser
    usage/ limits/            metrics sink, rate-limit + quota enforcer
rama_server/
    session.rs                signed-cookie + sqlite session store; is_safe_return_to
    cors.rs                   the CORS layer
```

Inside `crates/gateway-features/src/server/`: the optional subsystems — `rag/`,
`skills.rs`, `comfyui/`, `push/`, `github/`, `geoip/`, `typst.rs`, `image_gen.rs`,
`chat_attachments.rs`, `embeddings.rs`, `speech.rs`, `pdf.rs`, `ocr.rs`,
`search_settings.rs`, `document_canvas.rs`. None of them may name `AppState` or the
tool registry.

Inside `crates/gateway-runtime/src/`:

```
openai_driver.rs          # SessionDriver impl: OpenAI streaming chat-completions
loop_guard.rs
server/
    tools/                    Tool trait, ToolContext, registry, catalog, runner,
                              MCP manager, sandbox client (impls: gateway-tools;
                              echo + time stay here as the test fixtures)
    state.rs                  AppState
    comfyui_tool.rs           ComfyUI Tool/ToolSource impls + ComfyuiHandle
    scheduled/ webhooks.rs compaction.rs headless.rs   state-dependent workers
rama_server/
    state.rs                  RamaState (wraps AppState + sessions/usage/limits)
    auth.rs                   require_bearer for /v1/*
```

Inside `crates/gateway-tools/src/`: one module per tool family (`fetch_url`,
`search_web`, `typst_render`, `document`, `rag`, `qr`, `netcheck`, …). Register a
new tool in the `ToolRegistry` that `gateway`'s `main.rs` builds, and grant it in
`[rbac]`. `tests/` holds the two test modules that need both the machinery and the
real tools (catalog grouping, `AppState` authorization).

Inside `crates/gateway-web/src/`:

```
build_info.rs             # git SHA / version label (build.rs stamps it) — page chrome only
pages/                    # plait-rendered HTML
    mod.rs                    shared chrome — layout, nav, theme, SSE framing, Flash
    chat/                     gateway-side chat handler (delegates to session_core::worker
                              + OpenAiDriver). render.rs is the gateway's page-chrome
                              wrapper around session_core::render
    tokens.rs                 /tokens CRUD + row / minted-banner renderers
    admin.rs + siblings       the /admin/* screens
```

Inside `crates/gateway/src/`:

```
main.rs                   # boot: config → state → SessionStore → OIDC → rama serve
rama_server/              # routing glue only:
    router.rs                 the rama::http::service::web::Router builder
    proxy.rs                  /v1/{models,chat/completions,audio/transcriptions,…}
    api.rs                    session-authed /api/v0/* JSON endpoints
    oidc_handlers.rs          /auth/{login,callback,logout}
    rag_api.rs sandbox_api.rs comfyui_api.rs
    vad.rs                    silence trimming ahead of Whisper
tests/it/                 # integration suite — builds the router, serves requests in-process
```

Static assets (`app.css`, `datastar.js`, `app.js`, `pcm-recorder.js`) are
`include_bytes!`'d and served through `session_core::assets`.

## Hard rules — do not violate without asking

1. **Minimize Cargo dependencies.** Every new crate added to `Cargo.toml` requires a one-line justification in [`docs/dependencies.md`](docs/dependencies.md). Prefer stdlib + what rama already brings in.
2. **All toolchain and build/test/lint commands go through `mise`.** No `Makefile`, no `justfile`, no ad-hoc shell scripts checked in. See [`docs/dev-workflow.md`](docs/dev-workflow.md).
3. **Thorough testing, test-first (TDD).** Write the failing test before the implementation — red, green, refactor. Every public function has unit tests; every rama route has an integration test (`crates/gateway/tests/`); upstream LLMs are mocked with `wiremock` so tests run offline. The rama integration pattern is `router.serve(req).await` — no socket binding. **Style is Chicago / Classicist (state-based):** assert on observable results and real collaborators (in-memory SQLite via `:memory:`, `wiremock` upstreams, actual registries), not on interaction mocks. Reach for London-school behaviour-verification mocks only when a collaborator is genuinely un-fakeable (network you can't stand up, a clock, randomness) — and say so in a comment. Full strategy + required coverage in [`docs/testing.md`](docs/testing.md).
4. **Error messages are a product surface.** Use `thiserror` at API boundaries, `anyhow` + `.context()` internally, and write messages that say *what was happening, what went wrong, and what to do about it*. Full rules in [`docs/errors.md`](docs/errors.md).
5. **UI uses daisyUI component classes + Tailwind utilities, not hand-invented CSS.** Every visual element gets daisyUI semantic classes (`btn btn-primary`, `card card-body`, `alert alert-error`, `dropdown dropdown-end`, `badge badge-outline`, …) on plain HTML rendered through plait's `html!` macro. Token utilities for bespoke layout (`bg-base-100`, `text-base-content/60`, `border-base-300`, `text-error`, …) plus standard Tailwind layout (`flex`, `mb-4`, `grid`). One-off ".tagline" / ".brand-mark" classes are not — drop the visual treatment or push daisyUI for the missing component. Interactive surfaces (chat streaming, token CRUD) are driven by datastar SSE patches; see the [SSE pattern in `docs/ui.md`](docs/ui.md#datastar-driven-updates) before adding new actions.
6. **No comments explaining what code does** — names and types should already say that. Only comment *why* when it's non-obvious. Docs explain the system; code shows it.
7. **No backwards-compat shims** while the project is pre-1.0. We're starting fresh; if something needs to change, change it.
8. **Keep `README.md` deploy-current.** When you add or change a runtime knob, a config field, a host-package requirement, or a mise task on the deploy path, update `README.md` in the **same commit**. The README's "Quick start" + "Build + deploy" sections are the only thing a new operator reads before standing the stack up; if they don't reflect today's state, the next person wastes an hour. This is a strengthening of the broader "update docs in the same change as the code" rule from the working agreement at the bottom of this file — same spirit, just calling out the entry door explicitly so it doesn't drift.
9. **English only — no mixed languages.** Every string the app emits — UI labels, buttons, toasts, banners, tooltips, error messages, log lines, comments, and identifiers — is written in **English**. Do not introduce text in German or any other language, and never mix languages within the product. The app is not localized; there is no i18n layer, so a non-English string is simply a bug. The only place non-English text is allowed is *domain content that is intrinsically in another language* — e.g. the German business-letter fixture under `examples/typst-templates/letter/`, or a non-ASCII character used deliberately in a test (`'ß'` for a UTF-8 boundary case). Those are data, not app strings. When in doubt, write English. If you find existing non-English app text, translate it to English (and update any tests that assert on it) rather than adding more.
10. **Code principles — DRY, SOLID, KISS, Ubiquitous Language.** Default to the simplest thing that works (**KISS**) and don't repeat a fact or a shape in two places (**DRY** — extract a helper like `ChunkMeta::envelope` rather than copy a JSON literal twice). Follow **SOLID** where it pulls its weight: the `Tool` trait + `ToolRegistry` already give you open/closed extension (add a tool, don't touch the loop) and dependency inversion (drivers depend on the `SessionDriver` trait, not a concrete bin) — keep new code on that grain. Speak the codebase's **Ubiquitous Language** consistently in names, comments, and docs: `upstream` / `pool` / `backend`, `gateway-owned` vs `client-owned` tool calls, `turn` / `round`, `byte-dumb proxy`, `Acquired` in-flight guard. Don't coin a synonym for a term that already exists. These are guidance, not gates — if applying one would bloat or obscure, prefer the simpler code and note why.

## Daily workflow

After `mise install` (one time):

```bash
# Two terminals during UI work — one for the CSS, one for the gateway.
mise run watch-css         # tailwind --watch in ui/ → assets/app.css
mise run dev               # debug-mode `cargo run --package gateway`

# Other day-to-day tasks
mise run dev-build         # debug build only (target/debug/gateway), ~2 s incremental
mise run build-css         # one-shot CSS build
mise run test              # cargo test --workspace
mise run lint              # cargo fmt --check + clippy -D warnings
mise run fmt               # cargo fmt

# Browser-driven UI debugging (no OIDC required)
mise run dev-ui            # real rama server on :8080 + wiremock chat &
                           # transcription backends + a pre-seeded session;
                           # prints the cookie for playwright. Use this —
                           # NOT a hand-rolled test.html — when debugging
                           # ANY authed page (chat, tokens, dashboard,
                           # theme toggle, /api/v0/*). See docs/dev-workflow.md
                           # → "Debugging the UI".

# Slow path — DON'T use for iteration
mise run build             # cargo build --release. ~12 s incremental, ~70 s clean.
                           # Only for deploys / perf measurement.
```

**Debug, not release.** `mise run dev` and `mise run dev-build` produce debug binaries — runtime perf is identical to release for anything you'd interact with manually (the entire UI surface, smoke testing). Use `mise run build` only when you're shipping or actually benchmarking; rebuilding release on every iteration wastes 10 s per cycle for no gain.

The mise tasks DAG handles the CSS prerequisite automatically: `dev`, `dev-build`, `build`, `test`, and `lint` all depend on `build-css`, so a fresh checkout doesn't need any manual setup.

Full reference: [`docs/dev-workflow.md`](docs/dev-workflow.md).

## Where to find what

Start in [`docs/README.md`](docs/README.md) for the index. The topical docs:

| Topic | Doc |
|---|---|
| System architecture, request flow, component boundaries | [`docs/architecture.md`](docs/architecture.md) |
| Toolchain, mise tasks, dev loop | [`docs/dev-workflow.md`](docs/dev-workflow.md) |
| Dependency policy + the current allowed list | [`docs/dependencies.md`](docs/dependencies.md) |
| OIDC login + gateway-minted tokens | [`docs/auth.md`](docs/auth.md) |
| OpenAI-compat endpoints, streaming, transcription | [`docs/gateway-api.md`](docs/gateway-api.md) |
| Multi-provider routing, load balancing, health checks | [`docs/upstreams.md`](docs/upstreams.md) |
| Tool registry, role→tool mapping, execution loop | [`docs/tools-rbac.md`](docs/tools-rbac.md) |
| Which tools exist, their gates and toggle keys | [`docs/tools-inventory.md`](docs/tools-inventory.md) |
| Web UI — plait + daisyUI + datastar SSE patterns | [`docs/ui.md`](docs/ui.md) |
| Testing strategy and required coverage | [`docs/testing.md`](docs/testing.md) |
| Error handling — types, messages, OpenAI mapping | [`docs/errors.md`](docs/errors.md) |
| Phased delivery plan + current phase | [`docs/roadmap.md`](docs/roadmap.md) |

## Working agreement for agents

- **Plan before you implement.** For anything that touches more than one file or one concept, draft an approach and confirm before writing code.
- **Update docs in the same change as the code.** If you change the auth flow, update `docs/auth.md` in the same commit. Stale docs are worse than no docs.
- **When you discover a missing piece** — an undocumented invariant, a non-obvious gotcha — add it to the relevant doc. Don't rely on conversation history.
- **Tests live next to the code.** Unit tests in `#[cfg(test)] mod tests`, integration tests in `crates/gateway/tests/`. Run `mise run test` before declaring a task done.
- **If a hard rule is in your way**, surface it to the user. Don't quietly bypass.
