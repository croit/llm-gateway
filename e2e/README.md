# e2e/

End-to-end browser tests for the gateway, driven by Playwright through Node's built-in test runner. Zero project-level `node_modules` — the tests import `playwright` directly out of the mise-installed `npm:@playwright/cli` tool.

## Run

```bash
# In one terminal:
mise run dev

# In another:
mise run e2e
```

That runs both `e2e/api.test.mjs` (plain fetch against the public HTTP surface) and `e2e/anonymous.test.mjs` (Playwright-driven browser flows for the anonymous UI).

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

## What's not covered yet

- **Authenticated flows** — minting / listing / revoking tokens needs a session, which today requires a real OIDC provider. The follow-up is either a mock-OIDC test fixture or a `--dev-login` flag on the gateway that seeds a session without OIDC. The session-routes integration tests in `crates/gateway/tests/session_routes.rs` already cover the API side with an inline seed endpoint; the browser side would just replay that.
- **Tool-call loop end-to-end** — Phase 6's runner is fully unit-tested in isolation, but driving the full proxy + wiremock-upstream + injection path from the browser belongs here too.

## First-time setup notes

Chromium needs a few shared libs on Debian trixie:

```bash
sudo apt-get install -y libnss3 libnspr4 libatk1.0-0t64 libatk-bridge2.0-0t64 \
    libcups2t64 libdbus-1-3 libdrm2 libxkbcommon0 libxcomposite1 libxdamage1 \
    libxfixes3 libxrandr2 libgbm1 libpango-1.0-0 libcairo2 libasound2t64
```

And a one-time browser download (uses the `npm:@playwright/cli` we already have):

```bash
PLAYWRIGHT_BROWSERS_PATH=/var/host-cache/playwright/browsers \
  node /var/host-cache/mise/installs/npm-playwright-cli/0.1.13/lib/node_modules/@playwright/cli/node_modules/playwright/cli.js install chromium
```

(The cache path lines up with `MISE_CACHE_DIR` so it survives across project builds.)
