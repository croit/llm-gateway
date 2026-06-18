// Authed browser flows — exercises the Toast/Dialog/Skeleton interactions on
// the populated tokens page. Each test seeds a fresh dev session via the
// debug-only /__dev/seed-session endpoint (compiled in by cfg(debug_assertions),
// never in release), so all tests are independent and don't depend on test
// ordering.

import { test, before, after } from "node:test";
import assert from "node:assert";

import { BASE, launchBrowser, gatewayIsUp } from "./helpers.mjs";

let browser;

before(async () => {
    assert.ok(
        await gatewayIsUp(),
        `gateway is not reachable at ${BASE}; run \`mise run dev\` in another terminal`,
    );
    // Pre-flight: confirm the dev seed endpoint exists (i.e. server is debug).
    const probe = await fetch(`${BASE}/__dev/seed-session`);
    assert.ok(
        probe.ok,
        `\`/__dev/seed-session\` is unreachable (${probe.status}). Server must be a debug build (e.g. \`mise run dev\`).`,
    );
    browser = await launchBrowser();
});

after(async () => {
    if (browser) await browser.close();
});

/// Returns a Playwright context that's already authenticated as the dev
/// fixture user, with the DB wiped+reseeded to the canonical 3-token state.
async function seededContext() {
    const ctx = await browser.newContext({ colorScheme: "dark" });
    const page = await ctx.newPage();
    const resp = await page.goto(`${BASE}/__dev/seed-session`);
    assert.ok(resp && resp.ok(), `seed-session failed: ${resp?.status()}`);
    await page.close();
    return ctx;
}

test("dashboard shows the signed-in user, roles, and a manage-tokens link", async () => {
    const ctx = await seededContext();
    const page = await ctx.newPage();
    await page.goto(`${BASE}/`, { waitUntil: "networkidle" });
    await page.waitForSelector("text=Signed in as alice@example.com");
    await page.waitForSelector("text=engineering, admin");
    assert.equal(await page.locator('a[href="/tokens"]:has-text("Manage API tokens")').count(), 1);
    await ctx.close();
});

test("tokens page renders the seeded 3 rows with correct status badges", async () => {
    const ctx = await seededContext();
    const page = await ctx.newPage();
    await page.goto(`${BASE}/tokens`, { waitUntil: "networkidle" });
    await page.waitForSelector("table.tokens-table tbody tr", { timeout: 5000 });

    const rows = page.locator("table.tokens-table tbody tr");
    assert.equal(await rows.count(), 3);

    // The 2 active rows have an "active" badge; the revoked row has "revoked".
    const activeBadges = page.locator('table.tokens-table tbody td span:has-text("active")');
    const revokedBadges = page.locator('table.tokens-table tbody td span:has-text("revoked")');
    assert.equal(await activeBadges.count(), 2);
    assert.equal(await revokedBadges.count(), 1);

    await ctx.close();
});

test("creating a token surfaces a plaintext banner with gwk_ prefix and adds a row", async () => {
    const ctx = await seededContext();
    const page = await ctx.newPage();
    await page.goto(`${BASE}/tokens`, { waitUntil: "networkidle" });
    await page.waitForSelector("table.tokens-table tbody tr");

    await page.locator('input#token-name').fill("e2e-test-token");
    await page.locator('button:has-text("Create token")').click();

    // Plaintext banner.
    const plaintext = page.locator("pre.token-plain");
    await plaintext.waitFor({ state: "visible", timeout: 5000 });
    const text = (await plaintext.textContent()) ?? "";
    assert.match(text.trim(), /^gwk_[0-9a-f]{64}$/);

    // List now has 4 rows.
    await page.waitForFunction(
        () => document.querySelectorAll("table.tokens-table tbody tr").length === 4,
        null,
        { timeout: 5000 },
    );

    // The new row carries the name we typed.
    const namedRow = page.locator('table.tokens-table tbody tr:has(td:text-is("e2e-test-token"))');
    assert.equal(await namedRow.count(), 1);

    await ctx.close();
});

test("create-token form rejects empty names with an inline error", async () => {
    const ctx = await seededContext();
    const page = await ctx.newPage();
    await page.goto(`${BASE}/tokens`, { waitUntil: "networkidle" });
    await page.waitForSelector("table.tokens-table tbody tr");

    // Leave the name field blank, click Create.
    await page.locator('button:has-text("Create token")').click();
    await page.waitForSelector("text=Name is required", { timeout: 3000 });

    // Still 3 rows — nothing was created.
    const rows = await page.locator("table.tokens-table tbody tr").count();
    assert.equal(rows, 3);

    await ctx.close();
});

test("revoke shows a confirmation dialog and Cancel is a no-op", async () => {
    const ctx = await seededContext();
    const page = await ctx.newPage();
    await page.goto(`${BASE}/tokens`, { waitUntil: "networkidle" });
    await page.waitForSelector("table.tokens-table tbody tr");

    // First active row → Revoke. The third row (revoked) has no Revoke
    // button, so .first() lands on the laptop row.
    await page.locator('button:has-text("Revoke")').first().click();
    const confirm = page.locator('button:has-text("Yes, revoke")').first();
    await confirm.waitFor({ state: "visible", timeout: 3000 });
    // Cancel — dialog hides (may stay in DOM but become non-visible).
    await page.locator('button:has-text("Cancel")').first().click();
    await confirm.waitFor({ state: "hidden", timeout: 3000 });

    // Counts unchanged: still 2 active, 1 revoked.
    assert.equal(
        await page.locator('table.tokens-table tbody td span:has-text("active")').count(),
        2,
    );
    assert.equal(
        await page.locator('table.tokens-table tbody td span:has-text("revoked")').count(),
        1,
    );

    await ctx.close();
});

test("confirming revoke flips the row from active to revoked", async () => {
    const ctx = await seededContext();
    const page = await ctx.newPage();
    await page.goto(`${BASE}/tokens`, { waitUntil: "networkidle" });
    await page.waitForSelector("table.tokens-table tbody tr");

    await page.locator('button:has-text("Revoke")').first().click();
    await page.locator('button:has-text("Yes, revoke")').click();

    // After confirm: refresh propagates, the count flips.
    await page.waitForFunction(
        () => {
            const revoked = document.querySelectorAll(
                'table.tokens-table tbody td span'
            );
            return Array.from(revoked).filter((n) => n.textContent === "revoked").length === 2;
        },
        null,
        { timeout: 5000 },
    );
    assert.equal(
        await page.locator('table.tokens-table tbody td span:has-text("active")').count(),
        1,
    );

    await ctx.close();
});

test("sign out clears the session", async () => {
    const ctx = await seededContext();
    const page = await ctx.newPage();
    await page.goto(`${BASE}/`, { waitUntil: "networkidle" });
    await page.waitForSelector("text=Signed in as");

    // The sign-out button is a real form POST that redirects to /.
    await page.locator('button:has-text("Sign out")').click();
    await page.waitForURL(`${BASE}/`);
    await page.waitForSelector("text=You're not signed in", { timeout: 5000 });

    await ctx.close();
});
