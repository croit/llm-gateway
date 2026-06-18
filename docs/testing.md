# Testing strategy

"Thorough testing" is a project-level rule (see `AGENTS.md`). Concretely, that means each layer below is non-empty and runs in CI.

## Layers

| Layer | Lives in | What it covers |
|---|---|---|
| **Unit** | `#[cfg(test)] mod tests` next to the code | Pure functions, parsers, picker strategies, config validation, schema round-trips |
| **Integration (in-process)** | `crates/gateway/tests/` | Build a `RamaState` against an in-memory SQLite + a wiremock upstream, call `router(state).serve(req)` directly. No socket binding — `rama::Service::serve` is a pure async function. Shared helpers live in `tests/common/mod.rs`. |
| **Integration (with mocked upstreams)** | `crates/gateway/tests/` | `wiremock` instances stand in for LLM backends; verify routing, retries, tool-loop behavior, streaming, OIDC dance, datastar SSE wire shape |
| **E2E (CLI ↔ gateway)** | `crates/cli/tests/` | Spawn the gateway as a child process on a random port; drive the CLI binary; assert end-to-end. OIDC is mocked with a tiny test server. |
| **E2E (browser ↔ gateway)** | `e2e/*.test.mjs` | Playwright + `node:test` against a running `mise run dev`. Anonymous page flows + plain-fetch checks of the public HTTP surface. See `e2e/README.md`. |
| **Property-based** (where it makes sense) | various | OpenAI schema serde round-trips, picker fairness, config merge |
| **WASM** (Phase 7+) | `crates/gateway/tests/` with `wasm-bindgen-test` | Browser-side rendering checks. Adds cost; defer until UI grows. |

## Style: test-first, Chicago / Classicist

Write the test before the code — red, green, refactor (**TDD**). Tests are **state-based**: assert on observable results, exercising real collaborators (in-memory SQLite, `wiremock` upstreams, actual `ToolRegistry` / `UpstreamRegistry`) rather than interaction mocks. Behaviour-verification (London-school) mocks are the exception, reserved for collaborators you genuinely can't stand up in-process — and the test says why in a comment. The mocking philosophy below is the practical edge of this: we fake only the things that reach outside the process.

## Mocking philosophy

- **Upstream LLMs are always mocked in tests.** Real upstream calls in tests are forbidden. `wiremock` runs in-process.
- **OIDC is mocked end-to-end.** `tests/oidc_integration.rs` builds the IdP out of wiremock: discovery, JWKS (RSA public half of a freshly minted dev keypair), and a token endpoint that returns an RS256-signed ID token whose `nonce` matches whatever the gateway just generated. The CLI tests ship their own simpler `mock_oidc` test helper.
- **Time is injectable.** Token expiry, health-check intervals, and the tool-loop bound all take a `Clock` so tests advance time deterministically. No `sleep` in tests longer than 50ms.
- **DB is real-but-ephemeral.** Integration tests open SQLite via `db::open(":memory:")`. The schema migrations run exactly as in prod; the in-memory backing just means we don't leak files. One pool per test.

## What every PR must include

- New public function → unit test for the happy path and at least one failure mode.
- New rama route → integration test asserting:
    - Returns 401 without bearer.
    - Returns 403 when RBAC denies.
    - Returns the documented success shape.
- New tool → test that invokes it via the registry (with a mocked upstream that fakes a `tool_calls` response).
- Schema change → round-trip serde test (`from_json(to_json(v)) == v` for a representative fixture).

If a change has no tests, the PR description must explain why and which existing test covers it.

## Performance / load tests

Not part of the per-PR loop. We'll add a `criterion`-based bench suite at Phase 4 (multi-provider routing) targeting the per-request middleware overhead and the picker. Threshold: a no-op `/v1/chat/completions` with a mocked instant upstream should add <2ms p50 of gateway overhead.

## CI shape

```text
mise install
mise run lint
mise run test
```

That's it. Same commands a developer runs locally. No GHA-specific scripts.

`mise run e2e` is **not** in the CI default — it needs `mise run dev` running in parallel and Chromium installed. Wire it into CI once we have either an in-CI `mise run dev` orchestration or a pre-built gateway binary the e2e job can boot. Locally just run it in another terminal while iterating on the UI.

## E2E browser tests (`e2e/`)

- Driver: Node's built-in `node:test` + Playwright. No project-level `node_modules` — we import `playwright` directly out of the mise-installed `npm:@playwright/cli` tool, with the path overridable via `$PLAYWRIGHT_DIR`.
- Run with `mise run e2e` while `mise run dev` is up in another terminal.
- Set `CHROMIUM_HEADED=1` to watch the browser locally; `GATEWAY_URL=https://gw.dev` to target a remote.
- First-time setup (shared libs + chromium download) is in `e2e/README.md`.
- Authenticated browser flows aren't covered yet — needs either a mock-OIDC fixture or a `--dev-login` flag on the gateway. The session-routes Rust integration tests (`crates/gateway/tests/session_routes.rs`) already cover the API side with an inline seed endpoint; the browser tests would replay that pattern.

## Coverage

We don't enforce a line-coverage number — it incentivizes the wrong tests. Instead the "what every PR must include" checklist is the gate.

## Crates dedicated to testing

Pre-approved dev-dependencies are listed in `docs/dependencies.md`. Adding anything else requires the same justification step as a runtime dep.
