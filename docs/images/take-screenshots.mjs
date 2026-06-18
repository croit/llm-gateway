#!/usr/bin/env node
// Drives the dev gateway with playwright to produce the screenshots embedded
// in README.md. Run with: `mise run screenshots`.
//
// Requires:
//   - `mise run dev` running in another terminal at http://localhost:8080,
//     **in debug mode** — the `__dev/seed-session` endpoint we use for the
//     authenticated screenshots is `#[cfg(debug_assertions)]` and is not
//     compiled into release builds. (mise run dev is already debug.)
//
// Output: PNGs written next to this script.

import { fileURLToPath } from "node:url";
import path from "node:path";

// Use the playwright bundled inside the mise-managed @playwright/cli tool so
// we don't need a project-local package.json or node_modules.
const PLAYWRIGHT_DIR = process.env.PLAYWRIGHT_DIR
  ?? "/var/host-cache/mise/installs/npm-playwright-cli/0.1.13/lib/node_modules/@playwright/cli/node_modules/playwright";
const { chromium } = await import(`${PLAYWRIGHT_DIR}/index.mjs`);

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BASE = process.env.GATEWAY_URL ?? "http://localhost:8080";

const VIEWPORT = { width: 1200, height: 800 };

const browser = await chromium.launch();

// ----- anonymous shots ------------------------------------------------------
{
  const ctx = await browser.newContext({ viewport: VIEWPORT, colorScheme: "dark" });
  const page = await ctx.newPage();

  for (const [url, file] of [
    [`${BASE}/`, "dashboard.png"],
    [`${BASE}/login`, "login.png"],
  ]) {
    console.log(`→ ${url}`);
    await page.goto(url, { waitUntil: "networkidle" });
    await page.waitForTimeout(400);
    await page.screenshot({ path: path.join(HERE, file), fullPage: false });
  }

  await ctx.close();
}

// ----- authenticated shots --------------------------------------------------
// Seed a session via the debug-only /__dev/seed-session endpoint, then reuse
// the cookie across pages. Each screenshot gets a fresh page from the same
// context so its state is independent (open dialog on one doesn't leak to
// the next, etc.).
{
  const ctx = await browser.newContext({ viewport: VIEWPORT, colorScheme: "dark" });

  // Visit the seed endpoint. The Set-Cookie header populates the context.
  const seedPage = await ctx.newPage();
  const seedResp = await seedPage.goto(`${BASE}/__dev/seed-session`);
  if (!seedResp || !seedResp.ok()) {
    throw new Error(
      `seed-session failed (${seedResp?.status()}). Is mise run dev up in debug mode?`,
    );
  }
  await seedPage.close();

  // Authed dashboard.
  {
    console.log(`→ ${BASE}/  (authed)`);
    const page = await ctx.newPage();
    await page.goto(`${BASE}/`, { waitUntil: "networkidle" });
    await page.waitForSelector("text=Signed in as", { timeout: 5000 });
    await page.waitForTimeout(300);
    await page.screenshot({
      path: path.join(HERE, "dashboard-authed.png"),
      fullPage: false,
    });
    await page.close();
  }

  // Authed tokens (populated).
  {
    console.log(`→ ${BASE}/tokens  (populated)`);
    const page = await ctx.newPage();
    await page.goto(`${BASE}/tokens`, { waitUntil: "networkidle" });
    await page.waitForSelector("text=Your tokens", { timeout: 5000 });
    // Wait for the list to render (skeleton → real rows).
    await page.waitForSelector("table.tokens-table tbody tr", { timeout: 5000 });
    await page.waitForTimeout(400);
    await page.screenshot({
      path: path.join(HERE, "tokens.png"),
      fullPage: false,
    });
    await page.close();
  }

  // Revoke-confirmation dialog open.
  {
    console.log(`→ ${BASE}/tokens  (revoke dialog)`);
    const page = await ctx.newPage();
    await page.goto(`${BASE}/tokens`, { waitUntil: "networkidle" });
    await page.waitForSelector("table.tokens-table tbody tr", { timeout: 5000 });
    // Click the first Revoke button.
    await page.locator('button:has-text("Revoke")').first().click();
    await page.waitForSelector('text=Yes, revoke', { timeout: 5000 });
    await page.waitForTimeout(300);
    await page.screenshot({
      path: path.join(HERE, "tokens-revoke-dialog.png"),
      fullPage: false,
    });
    await page.close();
  }

  await ctx.close();
}

await browser.close();
console.log("done.");
