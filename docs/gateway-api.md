# Gateway HTTP API

The gateway exposes an OpenAI-compatible API so any standard SDK works against it unmodified. Everything under `/v1/*` requires a valid gateway bearer token (see `auth.md`).

## Supported endpoints

| Method | Path | Status | Notes |
|---|---|---|---|
| POST | `/v1/chat/completions`     | Phase 2 (passthrough) → 4 (multi-provider) → 5 (tools) | Streaming + non-streaming |
| POST | `/v1/audio/transcriptions` | Phase 4 | multipart/form-data, Whisper-compatible |
| POST | `/v1/audio/translations`   | Not implemented | No route registered yet |
| POST | `/v1/embeddings`           | Implemented | Single + batch; byte-dumb relay to the `embedding` pool, non-streaming |
| GET  | `/v1/models`               | Phase 2 | Lists every model across all pools (chat, transcription, embedding); clients select by id |
| GET  | `/healthz`                 | Phase 1 | Liveness; no auth |
| GET  | `/readyz`                  | Phase 1 | Readiness — checks DB + at least one upstream healthy |

## Schema

We mirror the OpenAI schema exactly for compatibility. Types live in `crates/shared/src/openai/*.rs`. We do **not** invent new request/response fields; gateway-specific extensions go in headers (e.g. `X-Gateway-Tool-Calls: 3`) or in a separate diagnostic endpoint, never in the body.

For evolution: when OpenAI adds fields, we accept them with `#[serde(other)]` / `#[serde(flatten)]` patterns and pass them through to upstreams unmodified. Our handlers only need to read the fields they care about (`model`, `stream`, `messages`, `tools`).

## Streaming

`POST /v1/chat/completions` with `"stream": true` returns `text/event-stream`:

- The rama handler returns a `Body::from_stream(...)` built around the reqwest streaming response. Each upstream chunk is forwarded as-is on the wire — we don't reframe `data:` lines on the fast path because reqwest's `bytes_stream` already emits SSE-shaped chunks and we want byte-perfect parity with whatever the backend sends.
- The tool-call loop (see `tools-rbac.md`) requires non-streaming upstream calls during intermediate rounds even when the *client* requested streaming. The final round streams.
- Distinct from the proxy stream: the page-level chat UI hits `POST /chat/stream` which speaks **datastar-patch-elements** SSE (not OpenAI SSE) and re-renders fragments of the conversation DOM as deltas arrive. The wire shape there is verified by `tests/chat_stream.rs`.

## Request lifecycle

Auth → RBAC → tool injection → routing → forwarding → (tool-loop?) → response.

The full step-by-step is in `architecture.md` under "Request flow".

## Errors

We return OpenAI-shaped errors so SDKs surface them correctly:

```json
{
  "error": {
    "message": "Model not available to this user.",
    "type": "permission_denied",
    "code": "rbac_model_denied"
  }
}
```

The gateway's own error types live in `shared::Error`. The rama-side conversion to an HTTP response lives in `gateway::server::api::error` (`fn into_response`) — never duplicate the mapping per-handler.

Status codes:
- `400` — malformed request, unknown model.
- `401` — missing/invalid bearer token.
- `403` — RBAC denial (model or tool).
- `404` — unknown endpoint.
- `429` — rate limit (per-user, planned).
- `502` — upstream returned an error or was unreachable.
- `503` — no healthy upstream for the requested model.

## Diagnostic endpoints (planned)

Not OpenAI-compatible, used by the web UI and operators:

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/internal/upstreams`     | admin | Per-upstream health + last error |
| GET | `/internal/tools`         | admin | Registered tools + which roles can see them |
| GET | `/internal/me`            | bearer | Caller identity, roles, allowed tools, allowed models |
| GET | `/internal/audit`         | admin | Recent calls (paginated) |

Admin is gated by membership in the `admin` role (mapped from the OIDC `roles_claim`).
