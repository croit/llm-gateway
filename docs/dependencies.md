# Dependency policy

## Hard rules

1. **No NPM / Node in the runtime tree.** Cargo only. The single carve-out is **test tooling**: Node + `@playwright/cli` (via mise's `npm:` backend) for the e2e browser tests and the screenshot driver. This is the same shape as wiremock for upstream mocking — test-only, never linked into the gateway binary or the CLI. Documented in the test-tooling section below.
2. **Every Cargo dep needs a justification.** Add it to the table below in the same PR that introduces it. A one-line "why" is enough.
3. **Prefer stdlib.** Don't pull in `chrono` for a single `Instant::now()`. Don't pull in `lazy_static` — use `std::sync::OnceLock` or `LazyLock`.
4. **Prefer crates rama already brings in.** rama re-exports `tokio`, `hyper`, `http`, `http-body`, and tower-style traits. Adding features to existing crates doesn't grow the tree — pulling in a parallel implementation does.
5. **No `*` or unbounded version ranges.** Pin a major+minor in `[workspace.dependencies]`.
6. **No `git = "..."` deps in main.** Pre-releases of `rama` are pulled by version from crates.io.

## Allowed runtime dependencies

These are pre-approved; just add them to the relevant crate's `Cargo.toml` (referencing `workspace = true` where possible).

| Crate | Used in | Why |
|---|---|---|
| `rama` | `gateway` | HTTP framework + proxying primitives. Features `http-full` + `tower`; rustls deliberately off (aws-lc-sys is cmake-only). |
| `plait` | `gateway` | `html! { ... }` macro for server-rendered HTML in the rama page handlers. Type-checked, auto-escaping. |
| `serde_urlencoded` | `gateway` | Form-body parsing for the page-level POST handlers (`/chat/stream`, etc.). |
| `tokio` | `gateway`, `cli` | Async runtime. |
| `serde`, `serde_json` | all | Data interchange (OpenAI schema, config). |
| `thiserror` | all | Library-style error types. |
| `tracing`, `tracing-subscriber` | `gateway`, `cli` | Structured logging. |
| `reqwest` (rustls-tls, stream) | `gateway`, `cli` | Outbound HTTP — upstream LLM calls (gateway) + gateway-API calls (cli). ring-backed rustls avoids the aws-lc-sys / cmake dependency that comes with rama's TLS features. See `crates/gateway/src/rama_server/proxy.rs` for why the gateway keeps reqwest rather than driving rama's client side directly. |
| `openidconnect` | `gateway` | OIDC discovery + code exchange. The Rust ecosystem's standard. |
| `sqlx` (sqlite, runtime-tokio-rustls, macros, migrate) | `gateway` | Persistence for users, gateway tokens, sessions, pending_logins, audit log. |
| `hmac` + `sha2` | `gateway` | HMAC-SHA256 for the signed session cookie; SHA-256 for indexed bearer-token lookup. Tokens are 256-bit OS-random opaque strings, so argon2id would only add CPU cost without security gain. |
| `rand` (with OsRng) | `gateway` | Session IDs and new gateway tokens from the OS RNG. |
| `uuid` (v4, serde) | `gateway` | Stable IDs for tokens, CLI handoff state. |
| `url` | `gateway` | OIDC redirect-URI construction and parsing. |
| `webbrowser` | `cli` | Opens the OIDC login URL when the user runs `gw auth login`. Trivially small. |
| `clap` (derive) | `cli` | Argument parsing. The standard. |
| `toml` | `gateway`, `cli` | Config and credentials file. |
| `anyhow` | `gateway`, `cli` (binaries only) | Error chains in binary code paths. **Not** allowed in `shared` or any code whose errors cross an API boundary — those use `thiserror`. See `docs/errors.md`. |
| `jiff` | `gateway`, `shared` | Timestamps, durations, TTLs (token expiry, health-check intervals, audit-log times). Chosen over `chrono` and `time` for the cleaner API and serde support. |
| `jsonwebtoken` | `gateway` | RS256 verification of OIDC ID tokens (via `openidconnect`'s plumbing) and signing of test-only ID tokens in `tests/oidc_integration.rs`. |
| `multer` | `gateway` | Streaming multipart parser for `/v1/audio/transcriptions`, `/v1/audio/translations`, and the chat composer's attachment submit (`POST /chat/{id}/messages` is `multipart/form-data` so each file lands as a `name=attachment` part alongside the `model` + `message` text fields). |
| `rust-s3` (tokio-rustls-tls) | `gateway` | S3 (or S3-compatible) client for chat attachment uploads. Each attached file lands at `<key_prefix>/<turn_id>/<filename>`; the resulting object URL is what goes into OpenAI's `image_url` content parts + the `[gw-attachment …]` marker we persist in `chat_turns.user_text` for history replay. Path-style requests so MinIO/Backblaze work without DNS gymnastics. |
| `futures-util` | `gateway` | Stream + sink combinators used in the proxy streaming path. |
| `bytes` | `gateway` | `Bytes`-typed bodies for rama handlers. |
| `markdown` | `gateway` | CommonMark→HTML for chat assistant replies + reasoning blocks. GFM features (tables, strikethrough, autolinks); raw HTML and `javascript:` / `vbscript:` URLs rejected by default so a `<script>` inside LLM output renders as escaped text. |
| `earshot` | `gateway` | Pure-Rust neural VAD (~110 KiB, no ONNX runtime) on the `/v1/audio/transcriptions` upload path. Strips leading/trailing silence + clips long pauses before forwarding to Whisper, since silence is the dominant source of Whisper hallucinations. |
| `lumis` (`default-features = false` + explicit grammar list) | `gateway` | Server-side syntax highlighting for fenced code blocks in chat replies (rendered to inline-styled spans by the chat-render post-pass; no client-side highlighter on top of datastar). We enable lumis' full `all-languages` set **minus `lang-caddy`** — `tree-sitter-caddy` is GPL-3.0, the only copyleft grammar in the set, and would force the binary to GPL (incompatible with our AGPL-3.0 license). The remaining ~116 tree-sitter grammars (all MIT/Apache-2.0) compile via build.rs — adds a few seconds to cold builds. Trade-off accepted so anything an LLM emits (svelte, zig, terraform, kotlin, …) renders coloured rather than monochrome; only Caddyfile blocks fall back to plain text. Light/dark theme switching uses `HtmlMultiThemesBuilder` with `tokyonight_day` + `tokyonight_night` and `default_theme = "light-dark()"`, so the browser flips colours from the document's `color-scheme` (which daisyUI sets per `data-theme`) without a re-render. |
| `regex` | `gateway` | Already a transitive dep via `tracing-subscriber`'s env-filter; pulled in explicitly so the chat-render post-pass can match fenced code blocks for the `lumis` rewrite step. |
| `ip2location` | `gateway` | Reads an IP2Location LITE DB11 `.BIN` (memory-mapped, sync, `Send + Sync`) to resolve a caller's source IP → coarse city/country/lat-lon for the `get_user_location` tool. Optional at runtime: with no DB file the feature is simply inactive. |
| `notify` | `gateway` | Filesystem watcher that hot-reloads the GeoIP `.BIN` when it changes (operator drop-in or the weekly updater) without a gateway restart. Cross-platform backend (inotify/FSEvents/…) via default features. |
| `zip` (default-features off, `deflate` only) | `gateway` | Unpacks the IP2Location LITE distribution downloaded by the optional weekly GeoIP updater. `deflate`-only — the LITE archives use standard deflate, so the C-backed bzip2/lzma/zstd codecs in zip's defaults stay out of the tree. |
| `async-trait` | `session-core`, `gateway` | Trait `SessionDriver` has an async method that needs to be `dyn`-compatible (we hand `Box<dyn SessionDriver>` to the spawned worker harness). Native async-fn-in-traits is dyn-safe in latest Rust but `async_trait` makes the lifetime semantics explicit and the trait shape Rustfmt-stable. |
| `usearch` (default-features off) | `gateway` | In-process HNSW vector index for the RAG subsystem (one index file per collection, mmap'd on open). Single-header C++ statically linked via the crate's build.rs — no runtime `.so` / `.dylib`, no network at build time, single static gateway binary preserved. `default-features = false` drops `numkong` (Unum's SIMD helper); its FP8 / SME-dispatch C files segfault Apple-Silicon clang. usearch falls back to its built-in distance kernels — plenty for codebase RAG at tens of thousands of chunks per collection. `simsimd` and `openmp` deliberately off. |
| `pdf-extract` | `gateway` | Pure-Rust (via `lopdf`) text-layer extraction for the `fetch_attachment` PDF reader's default `mode="text"` tier. No native deps; a born-digital PDF reads back like any text attachment. |
| `pdfium-render` | `gateway` | The PDF reader's `mode="images"` escalation tier: rasterises pages to bitmaps for a vision model when the text layer is empty (scanned PDFs). **The one runtime native-library exception to the static-binary rule** — it loads Chromium's `pdfium` (BSD-3) *dynamically at runtime* (pre-generated bindings, so no bindgen/clang at build time, and the build never needs the lib). It's *optional*: with no `pdfium` deployed the tier returns a clean "renderer unavailable" note (`server::pdf::bind_pdfium`), and the text tier is unaffected. Operators enable it by dropping `libpdfium` on the system search path or pointing `PDFIUM_LIB_PATH` at it. |
| `image` (default-features off, `png` only) | `gateway` | Encodes the bitmaps `pdfium-render` produces to PNG for the vision `image_url` parts. `png`-only keeps the jpeg/gif/webp/tiff codec set out of the tree; version matches `pdfium-render`'s `image_latest` so the two unify on one copy. |

## Allowed dev-dependencies

| Crate | Used in | Why |
|---|---|---|
| `wiremock` | `gateway` tests | Mock upstream LLM endpoints and the OIDC IdP (`tests/oidc_integration.rs`). The mocking exception to "minimize deps". |
| `tempfile` | tests | Scratch directories for SQLite + credential-file tests. |
| `http-body-util` | tests | `BodyExt::collect()` for draining rama response bodies in tests. |
| `rsa` (features: sha2) | `gateway` tests | Generates a throw-away RSA keypair per OIDC integration test run: the public half feeds the mock JWKS endpoint, the private half signs the ID token the gateway verifies. Test-only; never linked into the binary. |

## Test-tooling carve-out (Node + Playwright)

Pinned in `mise.toml`:

| Tool | Why |
|---|---|
| `node = "24"` | Runtime for the Playwright lib + Node's built-in `node:test` runner. |
| `npm:@playwright/cli` | Brings in the `playwright` JS lib + the chromium-headless-shell download. |

Used by:
- `e2e/*.test.mjs` — Playwright-driven browser tests for the page UI + plain-fetch tests for the public HTTP surface. Run via `mise run e2e`.
- `docs/images/take-screenshots.mjs` — generates the README screenshots. Run via `mise run screenshots`.

Neither file pulls in a project-level `package.json` or `node_modules` — both scripts `import` Playwright directly out of the mise tool's install directory (path overridable via `$PLAYWRIGHT_DIR`). Adding any other Node tool needs the same justification step as a Cargo dep.

## Explicitly not allowed (yet)

| Crate | Why not |
|---|---|
| `tailwindcss` (npm) | NPM ban (runtime). The page UI is built on daisyUI v5 + Tailwind v4 compiled into a single static CSS file at build time via the mise-installed `tailwindcss-cli`; no node_modules at runtime. The test-tooling carve-out above does *not* extend to runtime styling. |
| `chrono`, `time` | `jiff` is the chosen time crate. Don't mix. |
| `lazy_static`, `once_cell` | `std::sync::OnceLock` / `LazyLock` cover it. |
| `serde_yaml` | Config is TOML. One format. |
| `figment` | Hand-roll config layering until it stops being trivial. |
| `axum`, `tower-sessions`, `tower-http` | The server stack is rama-only. Sessions are hand-rolled (`rama_server::session`); HTTP-layer concerns ride on rama services. Bringing axum back would mean running two routers in parallel. |
| `dioxus`, `dioxus-primitives`, `dioxus-icons` | Dropped during the rama spike. Replaced by plait (server-rendered HTML) + daisyUI v5 (Tailwind v4 component classes) + datastar (SSE-driven DOM patches). |
| `base64` | Hand-rolled `base64url_nopad` in two places (`rama_server::session`, `tests/oidc_integration.rs`) — saves a transitive dep. |

## Adding a dep — checklist

1. Does an existing dep do this? (Check `cargo tree` first.)
2. Is the crate maintained? (Look at last release + issue traffic.)
3. What's its dep tree? (Run `cargo tree -e features -p <crate>` mentally — if it pulls in 30 transitive crates, push back.)
4. Add a row to the table above with the one-line justification.
5. Pin in `[workspace.dependencies]`, reference via `workspace = true` in member crates.
