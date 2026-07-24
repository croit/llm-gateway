#!/usr/bin/env node
// Screenshot any gateway page with Playwright, matching the repo's README
// images (1400x950 @2x -> 2800x1900, dark). Self-discovers the mise-managed
// Playwright lib and the cached Chromium binary, so it survives version bumps
// without "search around".
//
// Usage:
//   node screenshot.mjs --url http://localhost:8080/admin/skills \
//     --out docs/img/skills.png --cookie "id=<seed cookie value>" \
//     [--wait "text=Loaded skills"] [--strip-file-input] [--light] \
//     [--width 1400 --height 950 --scale 2]
//
// The cookie: start `mise run dev-ui` (or `cargo run --example dev_ui -p
// gateway`); it prints `id=<value>`. Pass that whole `id=...` (or just the
// value) as --cookie. Anonymous pages (/, /login) need no cookie.

import fs from "node:fs";
import os from "node:os";
import path from "node:path";

// ---- args ----
const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, a, i, arr) => {
    if (a.startsWith("--")) {
      const k = a.slice(2);
      const v = arr[i + 1] && !arr[i + 1].startsWith("--") ? arr[i + 1] : "true";
      acc.push([k, v]);
    }
    return acc;
  }, []),
);
const URL_ = args.url ?? "http://localhost:8080/";
const OUT = path.resolve(args.out ?? "screenshot.png");
const WIDTH = Number(args.width ?? 1400);
const HEIGHT = Number(args.height ?? 950);
const SCALE = Number(args.scale ?? 2);
const WAIT = args.wait; // a Playwright selector to wait for, e.g. "text=Loaded skills"
const COOKIE = args.cookie; // "id=VALUE" or just "VALUE"
// Sidebar nav-group fold state (the `nav_sections` cookie). Comma list of
// open groups, e.g. "workspace" or "admin" — expands the group the page
// belongs to. Absent → server default (Workspace open, Account/Admin shut).
const NAV_SECTIONS = args["nav-sections"];
const STRIP_FILE_INPUT = args["strip-file-input"] === "true";
// Arbitrary JS run in the page just before capture — e.g. open a modal/overlay
// (`document.getElementById('voice-modal').showModal()`) so a dialog can be
// screenshot. Runs after --wait and --strip-file-input.
const EVAL = args.eval && args.eval !== "true" ? args.eval : null;
const SCHEME = args.light === "true" ? "light" : "dark";

// ---- discover the mise-managed Playwright lib ----
function discoverPlaywright() {
  if (process.env.PLAYWRIGHT_DIR) return process.env.PLAYWRIGHT_DIR;
  const base = path.join(
    os.homedir(),
    ".local/share/mise/installs/npm-playwright-cli",
  );
  const versions = fs.existsSync(base)
    ? fs.readdirSync(base).filter((v) => /^\d+\.\d+\.\d+$/.test(v)).sort()
    : [];
  for (const v of versions.reverse()) {
    const p = path.join(
      base, v,
      "lib/node_modules/@playwright/cli/node_modules/playwright/index.mjs",
    );
    if (fs.existsSync(p)) return path.dirname(p);
  }
  throw new Error(
    "Could not find the mise Playwright lib. Is `npm:@playwright/cli` installed via mise? " +
      "Override with PLAYWRIGHT_DIR=<path to playwright pkg dir>.",
  );
}

// ---- discover the newest cached Chromium ("Google Chrome for Testing") ----
// The bundled Playwright's expected browser revision often mismatches what's
// cached; launching the cached binary by executablePath sidesteps the
// "playwright install" error.
function discoverChromium() {
  if (process.env.CHROME_EXE) return process.env.CHROME_EXE;
  const cache = path.join(os.homedir(), "Library/Caches/ms-playwright");
  const revs = fs.existsSync(cache)
    ? fs
        .readdirSync(cache)
        .filter((d) => /^chromium-\d+$/.test(d))
        .sort((a, b) => Number(a.split("-")[1]) - Number(b.split("-")[1]))
    : [];
  for (const r of revs.reverse()) {
    // arm64 + x64 layouts both ship "Google Chrome for Testing.app".
    for (const arch of ["chrome-mac-arm64", "chrome-mac-x64", "chrome-mac"]) {
      const exe = path.join(
        cache, r, arch,
        "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
      );
      if (fs.existsSync(exe)) return exe;
    }
  }
  return null; // fall back to Playwright's own resolution
}

const PLAYWRIGHT_DIR = discoverPlaywright();
const CHROME_EXE = discoverChromium();
const { chromium } = await import(`${PLAYWRIGHT_DIR}/index.mjs`);

const browser = await chromium.launch(
  CHROME_EXE ? { executablePath: CHROME_EXE } : {},
);
const ctx = await browser.newContext({
  viewport: { width: WIDTH, height: HEIGHT },
  deviceScaleFactor: SCALE,
  colorScheme: SCHEME,
  locale: "en-US",
});
if (COOKIE) {
  const value = COOKIE.startsWith("id=") ? COOKIE.slice(3) : COOKIE;
  await ctx.addCookies([{ name: "id", value, url: new URL(URL_).origin }]);
}
if (NAV_SECTIONS) {
  await ctx.addCookies([
    { name: "nav_sections", value: NAV_SECTIONS, url: new URL(URL_).origin },
  ]);
}

const page = await ctx.newPage();
const resp = await page.goto(URL_, { waitUntil: "networkidle" });
console.log(`status=${resp?.status()} url=${page.url()} title=${await page.title()}`);
if (page.url().includes("/login") && !URL_.includes("/login")) {
  console.warn("WARN: redirected to /login — the cookie is missing/invalid for this server.");
}
if (WAIT) {
  await page.waitForSelector(WAIT, { timeout: 8000 }).catch(() => {
    console.warn(`WARN: selector ${JSON.stringify(WAIT)} not found; capturing anyway.`);
  });
}
if (STRIP_FILE_INPUT) {
  // macOS renders the native <input type=file> button text in the OS UI
  // locale (can't be set per-page). Swap it for an English daisyUI-styled
  // stand-in so README shots read cleanly regardless of system language.
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('input[type="file"]')) {
      const d = document.createElement("div");
      d.className = el.className;
      d.style.cssText = "display:flex;align-items:center;padding:0;overflow:hidden;cursor:default";
      d.innerHTML =
        '<span style="background:oklch(0.31 0.01 260);color:oklch(0.9 0 0);font-weight:600;' +
        "padding:0 .7rem;align-self:stretch;display:flex;align-items:center;font-size:.78rem;" +
        'border-right:1px solid oklch(0.4 0.01 260)">Choose File</span>' +
        '<span style="padding-left:.7rem;opacity:.55;font-size:.8rem">No file chosen</span>';
      el.replaceWith(d);
    }
  });
}
if (EVAL) {
  await page.evaluate((code) => {
    // eslint-disable-next-line no-eval
    (0, eval)(code);
  }, EVAL);
  await page.waitForTimeout(400);
}
await page.waitForTimeout(300);
await page.screenshot({ path: OUT, fullPage: false });
console.log(`saved ${OUT} (${WIDTH}x${HEIGHT} @${SCALE}x)`);
await browser.close();
