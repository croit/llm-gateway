# e2e/

End-to-end browser tests for the gateway, driven by Playwright through Node's built-in test runner. Zero project-level `node_modules` — the tests import `playwright` directly out of the mise-installed `npm:@playwright/cli` tool.

## Run

```bash
# In one terminal:
mise run dev

# In another:
mise run e2e
```

That runs every `e2e/*.test.mjs`:
- `api.test.mjs` — plain `fetch` against the public HTTP surface.
- `anonymous.test.mjs` — Playwright-driven browser flows for the anonymous UI.
- `authed.test.mjs` — Playwright flows on an authenticated page, seeding a session per test via the debug-only `/__dev/seed-session` endpoint (no OIDC needed; the endpoint only exists in a `cfg(debug_assertions)` build such as `mise run dev`).

The `e2e` mise task points `PLAYWRIGHT_DIR` at the mise-installed `npm:@playwright/cli` tool automatically; export it yourself only to override.

Set `CHROMIUM_HEADED=1` to watch the browser locally:

```bash
CHROMIUM_HEADED=1 mise run e2e
```

Set `GATEWAY_URL=https://gw.dev` to point at a remote gateway.

## What's covered

- `/healthz`, `/readyz`, 404 routes.
- `/api/v0/me`, `/api/v0/tokens`: 401 OpenAI envelope when anonymous.
- `/v1/chat/completions`: 401 with no bearer / malformed bearer.
- Dashboard, `/login`, `/tokens` anonymous renders + nav links.
- Authenticated UI interactions on the populated `/tokens` page (Toast / Dialog / Skeleton), each on a fresh seeded session.

## What's not covered yet

- **Tool-call loop end-to-end** — the runner is fully unit-tested in isolation, but driving the full proxy + wiremock-upstream + injection path from the browser belongs here too.

## First-time setup notes

Chromium needs a few shared libs on Debian trixie:

```bash
sudo apt-get install -y libnss3 libnspr4 libatk1.0-0t64 libatk-bridge2.0-0t64 \
    libcups2t64 libdbus-1-3 libdrm2 libxkbcommon0 libxcomposite1 libxdamage1 \
    libxfixes3 libxrandr2 libgbm1 libpango-1.0-0 libcairo2 libasound2t64
```

And a one-time Chromium download (uses the mise-installed `npm:@playwright/cli`, located via `mise where` so it works wherever mise put it):

```bash
node "$(mise where 'npm:@playwright/cli')/lib/node_modules/@playwright/cli/node_modules/playwright/cli.js" install chromium
```

This downloads into Playwright's default browser cache (`~/.cache/ms-playwright` on Linux, `~/Library/Caches/ms-playwright` on macOS). On the shared CI/dev host, set `PLAYWRIGHT_BROWSERS_PATH=/var/host-cache/playwright/browsers` on both the download command and `mise run e2e` so the browser survives across project builds (the path lines up with `MISE_CACHE_DIR`).
