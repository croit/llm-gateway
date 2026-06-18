// API-level checks that don't need a browser — just plain fetch against the
// running gateway. These are quick smoke tests for the public HTTP surface.

import { test, before } from "node:test";
import assert from "node:assert";

import { BASE, gatewayIsUp } from "./helpers.mjs";

before(async () => {
    assert.ok(
        await gatewayIsUp(),
        `gateway is not reachable at ${BASE}; run \`mise run dev\` in another terminal`,
    );
});

test("/healthz returns 200 ok", async () => {
    const r = await fetch(`${BASE}/healthz`);
    assert.equal(r.status, 200);
    assert.equal((await r.text()).trim(), "ok");
});

test("/readyz returns 200", async () => {
    const r = await fetch(`${BASE}/readyz`);
    assert.equal(r.status, 200);
});

test("/api/v0/me returns 401 with the OpenAI-shaped envelope when anonymous", async () => {
    const r = await fetch(`${BASE}/api/v0/me`);
    assert.equal(r.status, 401);
    const body = await r.json();
    assert.equal(body.error.code, "unauthorized");
    assert.equal(body.error.type, "invalid_request_error");
});

test("/api/v0/tokens returns 401 when anonymous", async () => {
    const r = await fetch(`${BASE}/api/v0/tokens`);
    assert.equal(r.status, 401);
});

test("/v1/chat/completions returns 401 with no bearer", async () => {
    const r = await fetch(`${BASE}/v1/chat/completions`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ model: "x", messages: [] }),
    });
    assert.equal(r.status, 401);
});

test("/v1/chat/completions returns 401 with a malformed bearer", async () => {
    const r = await fetch(`${BASE}/v1/chat/completions`, {
        method: "POST",
        headers: {
            "content-type": "application/json",
            authorization: "Bearer not-a-real-token",
        },
        body: JSON.stringify({ model: "x", messages: [] }),
    });
    assert.equal(r.status, 401);
});

test("unknown route returns 404", async () => {
    const r = await fetch(`${BASE}/this-route-does-not-exist`);
    assert.equal(r.status, 404);
});
