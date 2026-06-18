// Anonymous (no-session) browser flows. Covers what a fresh visitor sees on
// each route + that navigation works.

import { test, before, after } from "node:test";
import assert from "node:assert";

import { BASE, launchBrowser, gatewayIsUp } from "./helpers.mjs";

let browser;

before(async () => {
    assert.ok(
        await gatewayIsUp(),
        `gateway is not reachable at ${BASE}; run \`mise run dev\` in another terminal`,
    );
    browser = await launchBrowser();
});

after(async () => {
    if (browser) await browser.close();
});

test("dashboard renders the unauthenticated welcome card", async () => {
    const ctx = await browser.newContext({ colorScheme: "dark" });
    const page = await ctx.newPage();
    await page.goto(`${BASE}/`, { waitUntil: "networkidle" });
    await page.waitForSelector("text=You're not signed in", { timeout: 5000 });
    // The "Sign in" link is a server-rendered <a href="/login"> in the DOM.
    const signInLink = page.locator('a[href="/login"]').first();
    assert.equal(await signInLink.count(), 1);
    await ctx.close();
});

test("clicking 'Sign in' from the dashboard lands on /login", async () => {
    const ctx = await browser.newContext({ colorScheme: "dark" });
    const page = await ctx.newPage();
    await page.goto(`${BASE}/`, { waitUntil: "networkidle" });
    await page.locator('a[href="/login"]').first().click();
    await page.waitForURL(`${BASE}/login`);
    await page.waitForSelector("text=Sign in to LLM Gateway", { timeout: 5000 });
    await ctx.close();
});

test("/login posts to /auth/login via a form action", async () => {
    const ctx = await browser.newContext({ colorScheme: "dark" });
    const page = await ctx.newPage();
    await page.goto(`${BASE}/login`, { waitUntil: "networkidle" });
    // The page is intentionally a focused, standalone landing: a single
    // <form action="/auth/login" method="get"> wrapping the CTA button.
    const form = page.locator('form[action="/auth/login"]');
    await form.first().waitFor({ state: "visible", timeout: 5000 });
    await page.locator('button:has-text("Continue with OIDC")').waitFor({ state: "visible" });
    await ctx.close();
});

test("/tokens (anonymous) shows the session-expired card with a sign-in link", async () => {
    const ctx = await browser.newContext({ colorScheme: "dark" });
    const page = await ctx.newPage();
    await page.goto(`${BASE}/tokens`, { waitUntil: "networkidle" });
    await page.waitForSelector("text=Session expired", { timeout: 5000 });
    const signInLink = page.locator('a[href="/login"]').first();
    assert.equal(await signInLink.count(), 1);
    await ctx.close();
});

test("nav header is present on / and /tokens (login intentionally omits it)", async () => {
    const ctx = await browser.newContext({ colorScheme: "dark" });
    const page = await ctx.newPage();
    for (const path of ["/", "/tokens"]) {
        await page.goto(`${BASE}${path}`, { waitUntil: "networkidle" });
        await page.waitForSelector("text=LLM Gateway", { timeout: 5000 });
        assert.equal(await page.locator('a[href="/"]:has-text("Dashboard")').count(), 1);
        assert.equal(await page.locator('a[href="/chat"]:has-text("Chat")').count(), 1);
        assert.equal(await page.locator('a[href="/tokens"]:has-text("Tokens")').count(), 1);
    }
    // /login is the standalone landing — no global nav.
    await page.goto(`${BASE}/login`, { waitUntil: "networkidle" });
    assert.equal(await page.locator('nav.nav').count(), 0);
    await ctx.close();
});

test("/chat scaffold renders the model picker, empty state, and composer", async () => {
    const ctx = await browser.newContext({ colorScheme: "dark" });
    const page = await ctx.newPage();
    await page.goto(`${BASE}/chat`, { waitUntil: "networkidle" });
    // Card title — confirms the page rendered the chat scaffold.
    await page.waitForSelector("text=Chat", { timeout: 5000 });
    // Empty-state hint is visible until the user sends a message.
    await page.waitForSelector("text=Send a message to get started", { timeout: 5000 });
    // Composer + model picker. Both are real <input>/<button> elements.
    assert.equal(await page.locator('input[name="message"]').count(), 1);
    assert.equal(await page.locator('input[name="model"]').count(), 1);
    assert.equal(await page.locator('button:has-text("Send")').count(), 1);
    await ctx.close();
});
