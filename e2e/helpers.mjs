// Shared bits for the e2e tests. Imports playwright from the bundled
// @playwright/cli mise tool (so we don't need a project-local node_modules).

const PLAYWRIGHT_DIR = process.env.PLAYWRIGHT_DIR
  ?? "/var/host-cache/mise/installs/npm-playwright-cli/0.1.13/lib/node_modules/@playwright/cli/node_modules/playwright";

const { chromium } = await import(`${PLAYWRIGHT_DIR}/index.mjs`);

export const BASE = process.env.GATEWAY_URL ?? "http://localhost:8080";

export { chromium };

/// Wraps `chromium.launch` so every test file picks up the same options.
export async function launchBrowser() {
    return chromium.launch({
        // Honour CHROMIUM_HEADED=1 when iterating locally; defaults to headless.
        headless: process.env.CHROMIUM_HEADED !== "1",
    });
}

/// Returns true when GET ${BASE}/healthz responds 200. Polled by tests so we
/// can fail fast with a clear "gateway isn't running" message instead of a
/// generic Playwright timeout.
export async function gatewayIsUp() {
    try {
        const r = await fetch(`${BASE}/healthz`, { signal: AbortSignal.timeout(2000) });
        return r.status === 200;
    } catch {
        return false;
    }
}
