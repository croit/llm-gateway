# Testing strategy

"Thorough testing" is a project-level rule (see [`AGENTS.md`](../AGENTS.md)). Concretely, that means each layer below is non-empty and runs in CI.

## Layers

| Layer | Lives in | What it covers |
|---|---|---|
| **Unit** | `#[cfg(test)] mod tests` next to the code | Pure functions, parsers, picker strategies, config validation |
| **Integration (in-process)** | `crates/gateway/tests/` | Build a `RamaState` against an in-memory SQLite + wiremock upstreams, then call `router(state).serve(req)` directly — no socket binding, since `rama`'s service is a plain async function. Shared setup lives in `tests/common/mod.rs`. |
| **Integration (mocked upstreams)** | `crates/gateway/tests/` | `wiremock` instances stand in for LLM backends; verify routing, the tool-call loop, streaming, the full OIDC login flow (`oidc_integration.rs`), and the datastar SSE wire shape. |
| **E2E (browser ↔ gateway)** | `e2e/*.test.mjs` | Playwright + Node's `node:test` against a running `mise run dev`. Anonymous page flows, authenticated flows (session seeded via the debug-only `/__dev/seed-session` endpoint), and plain-`fetch` checks of the public HTTP surface. See `e2e/README.md`. |

## Style: test-first, Chicago / Classicist

Write the test before the code — red, green, refactor (**TDD**). Tests are **state-based**: assert on observable results, exercising real collaborators (in-memory SQLite, `wiremock` upstreams, the actual `ToolRegistry` / `UpstreamRegistry`) rather than interaction mocks. Behaviour-verification (London-school) mocks are the exception, reserved for collaborators you genuinely can't stand up in-process — and the test says why in a comment. The mocking philosophy below is the practical edge of this: we fake only the things that reach outside the process.

## Mocking philosophy

- **Upstream LLMs are always mocked in tests.** Real upstream calls in tests are forbidden. `wiremock` runs in-process.
- **OIDC is mocked end-to-end.** `crates/gateway/tests/oidc_integration.rs` builds the IdP out of wiremock: a discovery document, a JWKS carrying the public half of a freshly minted RSA dev keypair, and a token endpoint that returns an RS256-signed ID token whose `nonce` matches whatever the gateway just generated.
- **DB is real-but-ephemeral.** Integration tests open SQLite via `db::open(":memory:")`. The schema migrations run exactly as in prod; the in-memory backing just means we don't leak files. One pool per test.

## What every PR must include

- New public function → unit test for the happy path and at least one failure mode.
- New rama route → integration test asserting:
    - Returns 401 without a bearer / session.
    - Returns 403 when the route is RBAC-gated and the caller isn't authorized (e.g. a non-admin hitting an admin route via `require_admin_or_403`).
    - Returns the documented success shape.
- New tool → test that invokes it via the registry (with a mocked upstream that fakes a `tool_calls` response).
- Schema change → round-trip serde test (`from_json(to_json(v)) == v` for a representative fixture).
- New UI string → a Fluent key in `locales/en/<module>.ftl` **and** its translation in all 5 other locales (`de`/`fr`/`es`/`ru`/`zh`) — not a checklist item you can skip: `session-core/build.rs` won't let the crate compile otherwise. See [`docs/ui.md`](ui.md#i18n--every-user-facing-string-must-be-translated).

If a change has no tests, the PR description must explain why and which existing test covers it.

## CI shape

CI (`.github/workflows/ci.yml`) runs a single command:

```text
mise run ci
```

`mise run ci` fans out through mise's task DAG to **lint + test + release build**:

- `mise run lint` → `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `tsc --noEmit`.
- `mise run test` → `cargo test --workspace`.
- `mise run build` → the release gateway binary.

Each of those transitively builds the CSS/JS asset bundles first, so a fresh checkout works without manual steps. The same job then builds the `sandbox-runner` binary and uploads both as artifacts; downstream jobs build the container images.

The version-controlled pre-push git hook (`.githooks/pre-push`, enabled with `mise run setup-hooks`) mirrors CI's lint + test locally so breakage is caught before a push triggers CI. It skips the release build — a compile error surfaces in the test step anyway. Bypass a WIP push with `git push --no-verify`.

## E2E browser tests (`e2e/`)

- Driver: Node's built-in `node:test` + Playwright. No project-level `node_modules` — the tests import `playwright` directly out of the mise-installed `npm:@playwright/cli` tool, with the path overridable via `$PLAYWRIGHT_DIR`.
- Run with `mise run e2e` against a live `mise run dev` in another terminal. The task points `PLAYWRIGHT_DIR` at the mise-installed `npm:@playwright/cli` automatically. See `e2e/README.md` for first-time setup (shared libs + a one-time Chromium download).
- `GATEWAY_URL` (default `http://localhost:8080`) targets a specific gateway; `CHROMIUM_HEADED=1` shows the browser instead of running headless.
- **Not part of the CI default** — the browser suite needs a running gateway and Chromium, so it stays a local/opt-in loop.
- Authenticated flows (`e2e/authed.test.mjs`) don't need OIDC: they seed a session through the debug-only `/__dev/seed-session` endpoint, which is compiled in under `cfg(debug_assertions)` and never present in a release build.

## Performance / load tests

Not part of the per-PR loop, and no benchmark suite exists yet. If one is added, the natural target is the per-request middleware overhead and the upstream picker (e.g. a no-op `/v1/chat/completions` against a mocked instant upstream), measured with `criterion`.

## Coverage

We don't enforce a line-coverage number — it incentivizes the wrong tests. Instead the "what every PR must include" checklist is the gate.

## Crates dedicated to testing

Pre-approved dev-dependencies are listed in [`docs/dependencies.md`](dependencies.md). Adding anything else requires the same justification step as a runtime dep.
