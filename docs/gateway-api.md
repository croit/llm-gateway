# Gateway HTTP API

The gateway exposes an OpenAI-compatible API so any standard SDK works against it unmodified. Every `/v1/*` endpoint requires a valid gateway bearer token (see [`auth.md`](auth.md)). The two health probes are unauthenticated.

The routes are wired in `crates/gateway/src/rama_server/router.rs`; the `/v1/*` handlers live in `crates/gateway/src/rama_server/proxy.rs`.

## Supported endpoints

| Method | Path | Auth | Notes |
|---|---|---|---|
| POST | `/v1/chat/completions`     | Bearer | Streaming + non-streaming. Server-side tool execution when the caller's token has tool grants (see [`tools-rbac.md`](tools-rbac.md)); otherwise a byte-for-byte passthrough. Routes to the `chat` pool. |
| POST | `/v1/embeddings`           | Bearer | Single + batch. Byte-dumb relay to the `embedding` pool; non-streaming. |
| POST | `/v1/images/generations`   | Bearer | JSON (`{model, prompt, size, …}`) in, OpenAI images envelope (`data[].b64_json` or `.url`) out. Byte-dumb relay to the `image` pool. |
| POST | `/v1/images/edits`         | Bearer | `multipart/form-data` (`image` + `prompt` + `model`). Byte-dumb relay to the `image` pool. |
| POST | `/v1/audio/transcriptions` | Bearer | `multipart/form-data`, Whisper-compatible. Silence-trimmed and re-framed before forwarding to the `transcription` pool. |
| POST | `/v1/audio/speech`         | Bearer | Text-to-speech (OpenAI-shaped: `{model, input, voice, response_format}`). Byte-dumb relay to the `speech` pool; audio bytes out. Returns a routing error if no `speech` backend serves the model (i.e. no `speech` pool configured). |
| GET  | `/v1/models`               | Bearer | Lists every model served by any healthy backend across all pools (chat, transcription, embedding, image, speech), de-duplicated by id. Synthesised from the registry's cached model sets — no upstream round-trip. |
| GET  | `/v1/models/{id}`          | Bearer | Retrieve a single model object, or `404 model_not_found` if no backend serves the id. `{id}` is a catch-all because model ids contain `/`. |
| GET  | `/v1/sandbox/files/{run}/{filename}` | Bearer | Downloads a file a sandbox run produced for the caller, scoped to the caller's user (see `sandbox_api`). |
| GET  | `/healthz`                 | none | Liveness. Returns `{"status":"ok"}`. |
| GET  | `/readyz`                  | none | Readiness. Returns `{"status":"ok"}`. |

`POST /v1/audio/translations` is **not** implemented — no route is registered.

> The web UI's page routes (`/`, `/chat`, `/tokens`, `/admin/*`, …) and the session-scoped `/api/v0/*` and `/auth/*` routes are separate surfaces, not part of the OpenAI-compatible API. See [`ui.md`](ui.md).

## Authentication

Every `/v1/*` call must send `Authorization: Bearer gwk_<64 hex chars>`. The gateway validates the token (SHA-256 lookup against active tokens) and resolves the caller's user before doing any work; on success it background-bumps the token's `last_used_at`. A missing, malformed, or unknown token gets a `401` with an OpenAI-shaped envelope and a `WWW-Authenticate: Bearer realm="gateway"` header:

```json
{
  "error": {
    "message": "missing or invalid bearer token",
    "type": "unauthorized",
    "code": "unauthorized"
  }
}
```

The client's own `Authorization` header is never forwarded upstream — it is dropped and the configured backend key (if any) is injected in its place. Token format and storage details are in [`auth.md`](auth.md).

There is **no per-model RBAC gate** on the proxy paths: any authenticated caller may address any model the gateway serves. RBAC applies to *tools* only — it (together with the user's `/tools` toggles and the token's per-capability switches) decides which gateway tools get advertised and injected into a chat completion. A denied tool is simply never offered; it does not produce a `403`.

## Model field and alias resolution

The `model` field may be a real model id **or an alias** (see [`upstreams.md`](upstreams.md#model-aliases)). Requests without a string `model` field get `400 invalid_request`.

When an alias — or one of the configured fallbacks — resolves to a different real model, the gateway:

- rewrites the forwarded body's `model` to the real id (upstreams don't know the alias),
- echoes the real id in the response, and
- sets an `X-Gateway-Resolved-Model: <real-id>` response header — **only** when the resolved id differs from what the client sent.

Admin-configured sampling/reasoning defaults are keyed on the *real* model id and only fill in fields the client omitted (client values always win).

## Response headers

Beyond the relayed upstream headers, the gateway may add:

| Header | When | Meaning |
|---|---|---|
| `X-Gateway-Resolved-Model` | Alias/fallback fired | The real model id that actually served the request. |
| `X-Gateway-Tool-Rounds` | Non-streaming chat completion that ran the gateway tool loop | Number of upstream rounds the tool loop took. Absent on the byte-dumb fast path and on streaming responses. |

## Streaming

`POST /v1/chat/completions` with `"stream": true` returns `text/event-stream`:

- Upstream SSE frames are relayed 1:1 — the gateway does not reframe `data:` lines. The deltas are tapped in parallel through a repetition-based loop guard; a model that collapses into a loop is cut off with a terminating error chunk and `[DONE]`, while a long-but-progressing answer streams through untouched.
- When the caller has tool grants, intermediate tool-loop rounds are executed against the upstream **non-streaming** even though the client asked for a stream; only the final round streams to the client.
- This is distinct from the page-level chat UI, which hits `POST /chat/{id}/messages` and streams **datastar-patch-elements** SSE (DOM fragments), not OpenAI SSE.

## Header handling

Hop-by-hop and identity headers are filtered in both directions. Requests drop `authorization`, `host`, `content-length`, `connection`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, `upgrade`, `expect` before forwarding. Responses drop the same set minus `authorization`/`host`/`expect`. Everything else is passed through.

## Schema

We mirror the OpenAI schema for compatibility. We do **not** invent new request/response body fields; gateway-specific signals go in headers (`X-Gateway-Resolved-Model`, `X-Gateway-Tool-Rounds`), never in the body. Handlers only read the fields they care about (`model`, `stream`, `messages`, `tools`) and pass the rest through to the upstream unmodified.

## Errors

Errors are returned in the OpenAI envelope so SDKs surface them correctly. The general helper sets `type` and `code` to the **same** value:

```json
{
  "error": {
    "message": "no healthy backend in `chat`",
    "type": "upstream_unreachable",
    "code": "upstream_unreachable"
  }
}
```

An unknown model is the one deliberate exception — it matches OpenAI's `model_not_found` shape exactly, with a distinct `type` and a `param`, so clients treat it as a request error rather than a retryable 5xx:

```json
{
  "error": {
    "message": "The model `foo` does not exist or you do not have access to it.",
    "type": "invalid_request_error",
    "param": "model",
    "code": "model_not_found"
  }
}
```

Upstream 4xx/5xx bodies are relayed verbatim (status, headers, and body), so a provider's own error reaches the client unchanged.

Status codes the gateway itself produces:

| Status | `code` | Cause |
|---|---|---|
| `400` | `invalid_request` | Malformed body, missing `model`, unparseable multipart. |
| `401` | `unauthorized` | Missing / malformed / unknown bearer token. |
| `404` | `model_not_found` | No backend in any pool serves the requested model. |
| `500` | `internal_error` | Internal failure, unparseable upstream JSON, or a tool loop that exhausted its round budget. |
| `502` | `upstream_unreachable` | A chosen backend was contacted but the transport/read failed. |
| `503` | `upstream_unreachable` | No healthy backend for the model's pool, or the pool is saturated. |

There is no built-in per-user rate limiting today.
