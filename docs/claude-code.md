# Claude Code through the gateway

Claude Code speaks the **Anthropic Messages API**. The gateway serves it at
`POST /v1/messages`, translating to and from the OpenAI-compatible dialect
every configured upstream already speaks. Point Claude Code at the gateway
and it runs against whatever model you serve — with the gateway's routing,
per-token model allowlists, rate limits, quotas, usage accounting and audit
trail applying exactly as they do to every other `/v1` caller.

Nothing about Claude Code is patched or wrapped: it is configured, through
the environment variables it already supports.

## Set it up

1. **Mint a token.** Sign in to the gateway, open `/tokens`, create one.
   It looks like `gwk_…`.

2. **Alias the model names Claude Code sends.** Claude Code asks for ids like
   `claude-sonnet-4-6`; your backend serves something else. On
   `/admin/upstreams`, give the backend an alias per id you want to answer:

   | Alias | Target |
   |---|---|
   | `claude-sonnet-4-6` | your main coding model |
   | `claude-opus-4-8` | the same, or a larger one |
   | `claude-haiku-4-5` | a small, fast model — Claude Code uses it for background work |

   Aliases are also what makes a model **discoverable** (see below), so one
   piece of configuration does both jobs.

   As a safety net, the per-kind unknown-model fallback (`/admin/upstreams`)
   catches any id you didn't alias: with a chat fallback configured, a model
   name the gateway has never heard of resolves to it instead of 404ing.

3. **Point Claude Code at the gateway.**

   ```bash
   export ANTHROPIC_BASE_URL=https://gateway.example.com
   export ANTHROPIC_AUTH_TOKEN=gwk_…
   ```

   `ANTHROPIC_AUTH_TOKEN` sends the token as `Authorization: Bearer`;
   `ANTHROPIC_API_KEY` sends it as `x-api-key`. The gateway accepts both, so
   either variable works. To make it permanent, put the same two keys in the
   `env` block of `~/.claude/settings.json`.

4. **Check it.**

   ```bash
   curl -s "$ANTHROPIC_BASE_URL/v1/messages" \
     -H "Authorization: Bearer $ANTHROPIC_AUTH_TOKEN" \
     -H "anthropic-version: 2023-06-01" \
     -H "content-type: application/json" \
     -d '{"model":"claude-sonnet-4-6","max_tokens":64,
          "messages":[{"role":"user","content":"reply with OK"}]}'
   ```

   Then just run `claude`.

### Optional environment

| Variable | Why |
|---|---|
| `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` | Adds the gateway's models to the `/model` picker. Claude Code calls `GET /v1/models?limit=1000` at startup and keeps entries whose id contains `claude` or `anthropic` — which is what your aliases are for. |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` | Stops Claude Code reporting telemetry to Anthropic. Model inference already goes only to the gateway. |
| `CLAUDE_CODE_ATTRIBUTION_HEADER=0` | Drops the short attribution block Claude Code prepends to the system prompt. The gateway forwards that block to the model as ordinary prompt text; set this if you'd rather it weren't sent at all. |
| `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, … | Name gateway model ids directly instead of aliasing the Claude ones. |

## What the gateway does with a request

| Claude Code sends | The gateway does |
|---|---|
| `system` (string or block array) | Joins the text blocks into one leading `system` message |
| `tool_result` blocks in a user turn | One OpenAI `role: "tool"` message each, ahead of the remaining user content |
| `tool_use` blocks in an assistant turn | `tool_calls[]`, with the input object stringified into `arguments` |
| `thinking: {"type": "adaptive"}` | Translates to the *serving model's* reasoning parameter — `enable_thinking`, `reasoning_effort`, or a token budget, per `/admin/models`. The field itself never reaches the backend. |
| `output_config.effort` | Same, mapped onto the gateway's Fast / Standard / Deep / Max levels |
| `cache_control` markers | Dropped. Upstream prefix caching still happens; it just isn't reported, so `usage.cache_read_input_tokens` is always `0`. |
| `context_management`, `mcp_servers`, `container`, `output_config.format`, unknown future fields | Dropped, not rejected — forwarding them would `400` an OpenAI-compatible backend, and rejecting them would break on the next client release |
| Anthropic-hosted server tools (`web_search_…`, `code_execution_…`) | Skipped: they can only run on Anthropic's infrastructure |
| `image` blocks | `image_url` parts (`data:` URI for base64 sources) |
| a mid-conversation `role: "system"` message | Folded into the leading system message. Claude Code appends one (the agent-type roster) after the user turn; left in place, a chat template that requires the system message first rejects the whole request. |

Coming back, the assistant's text becomes a `text` block, `reasoning_content`
becomes a `thinking` block, and each tool call becomes a `tool_use` block with
parsed `input`. Streaming responses are re-encoded as the Anthropic event
sequence — `message_start`, content blocks, `message_delta`, `message_stop` —
with a `ping` every 15 seconds so a long tool run doesn't look like a dead
stream to the client's 300-second watchdog.

**Errors keep the upstream's wording.** Claude Code recovers from certain
backend rejections by matching on the error message and retrying with the
capability disabled; the gateway re-wraps the body in the Anthropic error
envelope but never rewords it.

## Gateway tools

Claude Code brings its own tools (Read, Bash, Edit, …) and runs them locally.
It also works with the gateway's server-side tools — web search, RAG, the
sandbox, your connected MCP integrations — because the tool loop splits a turn
between the two: gateway-owned calls run server-side and invisibly, and calls
Claude Code owns are handed back for it to execute.

This is off by default. Turn on **tool use** for the token on `/tokens` to
enable it, and pick which capabilities that token may use there. With it off,
`/v1/messages` is pure format translation.

## Limits, cost and audit

Identical to `/v1/chat/completions`, because it is the same pipeline:

- every round is one usage row (`/usage`, and `/admin/tokens` for the
  deployment-wide view), attributed to the token and its owner;
- rate limits and quotas are enforced before routing, and a breach is a `429`
  with `Retry-After` in the Anthropic error shape;
- a token's model allowlist applies to the alias *and* its target;
- pool group restrictions apply, so a token can't reach a pool its owner's
  groups don't permit.

## Token counting

`POST /v1/messages/count_tokens` is answered by the serving backend's own
tokenizer: the gateway translates the request exactly as it would for
inference, then asks vLLM's `POST /tokenize` to run the model's chat template
over the messages and tool definitions. The number is the real one, not an
estimate.

A backend that doesn't expose a tokenizer gets a `404` instead of a guess, and
Claude Code falls back to counting context from the `usage` figures on real
responses. The first `404` from a given backend is remembered, so later counts
don't pay for the round trip; only a status that means the endpoint is *absent*
(`404`/`405`/`501`) is remembered, so a momentarily busy backend doesn't
disable counting for the rest of the process.

On a **streamed** turn the prompt size isn't known until the backend reports
it, which is after the response headers have already gone out — so
`message_start` carries `usage.input_tokens: 0` and the real figure rides the
closing `message_delta`. Across a multi-round gateway-tool turn that figure is
the *last* round's prompt, not the sum of every round's: each round resends the
whole conversation, so summing would report a context several times its real
size. Output tokens do sum — every one of them was generated.

The count covers what Claude Code sent. Gateway tools that the loop would
inject (only when the token has tool use enabled) are not included: resolving
that set means calling out to your MCP connectors, which is far too much work
for a request whose job is to be cheap.

## Known limits

- **Thinking-block signatures are ours, not Anthropic's.** A `thinking` block
  the gateway emits carries `"signature": "gateway"`. Nothing verifies it: the
  request translator drops every thinking block on the way back in, so the
  round trip is closed.
- **Prompt-cache reporting is always zero.** See `cache_control` above.
- **`max_tokens` is passed through as sent.** Claude Code asks for large
  ceilings; if that exceeds what your backend allows, the backend's own error
  reaches Claude Code unchanged. Lower it with a per-model default on
  `/admin/models` if needed.
- **The repetition guard applies.** Streamed turns run through the gateway's
  degenerate-loop detector, which stops a stream when one non-trivial line
  (8+ characters) recurs a dozen times inside a 6 KB window. That is aimed at
  a model stuck in a loop, but generated output with that much verbatim
  repetition would trip it too, ending the turn with a "loop detected"
  message.
- Features that depend on a claude.ai identity (Remote Control, voice
  dictation) are unavailable while a gateway credential is set. That is a
  client-side rule, not a gateway one.

## Related

- [`docs/upstreams.md`](upstreams.md) — pools, backends, aliases, fallbacks
- [`docs/tools-rbac.md`](tools-rbac.md) — who may use which gateway tool
- [`docs/errors.md`](errors.md) — the error taxonomy behind both endpoints
