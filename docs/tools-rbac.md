# Tools + RBAC

## What a tool is

A **tool** is a Rust handler the gateway can run on behalf of an LLM during a chat completion. From the model's perspective it's a normal OpenAI function-calling tool: it has a JSON schema, the model emits `tool_calls`, the gateway executes them, and the result feeds back into the next round. The model doesn't know (and shouldn't know) that the tool ran on the gateway.

This is **not** a passthrough or an MCP broker. The gateway *is* the tool runtime.

## Why server-side execution

- Tools need access to internal company systems (databases, APIs, file stores) that we don't want to expose to every model client.
- Tool results count as authoritative — running them on the gateway means we control the inputs, can rate-limit, and can audit.
- Clients (any OpenAI SDK) don't need any extra wiring. They see a normal completion.

## Tool registration

A tool is a type that implements:

```rust
// crates/gateway/src/server/tools/mod.rs (sketch — final API TBD in implementation)
#[async_trait::async_trait]   // may swap for native async-in-traits once stabilized; see notes
pub trait Tool: Send + Sync + 'static {
    fn id(&self) -> &'static str;             // stable id, e.g. "company_invoice_lookup"
    fn schema(&self) -> ToolDef;              // OpenAI tool definition (name, description, parameters)
    async fn run(&self, ctx: ToolContext, args: serde_json::Value) -> Result<serde_json::Value, ToolError>;
}
```

`ToolContext` carries the caller's `user_id`, roles, and a `tracing::Span` so per-tool logs nest under the request. Tools never receive the OIDC access token — if a tool needs to act as the user against a downstream service, that integration is explicit per tool.

Tools are registered at startup in `server::tools::registry`:

```rust
let registry = ToolRegistry::default()
    .with(tools::invoice::InvoiceLookup::new(db.clone()))
    .with(tools::wiki::WikiSearch::new(http.clone()))
    .with(tools::tickets::TicketCreate::new(ticketing.clone()));
```

We do not auto-discover tools at runtime. Adding a tool means writing code, opening a PR, and reviewing it — which is the point.

## RBAC

Roles come from the OIDC `roles_claim` (configurable, see `auth.md`). We map external claim values to internal role ids in config:

```toml
[rbac]
default_role = "user"

[[rbac.mapping]]
oidc_claim = "groups"
oidc_value = "engineering"
role = "engineering"

[[rbac.mapping]]
oidc_claim = "groups"
oidc_value = "finance"
role = "finance"

[[rbac.mapping]]
oidc_claim = "groups"
oidc_value = "admin"
role = "admin"
```

Each role gets a set of tool ids and a set of allowed model patterns:

```toml
[[roles]]
id = "user"
models = ["*"]              # all models routed by [models] are allowed
tools = []

[[roles]]
id = "engineering"
models = ["*"]
tools = ["company_wiki_search", "company_repo_search", "company_tickets_create"]

[[roles]]
id = "finance"
models = ["*"]
tools = ["company_invoice_lookup", "company_wiki_search"]

[[roles]]
id = "admin"
models = ["*"]
tools = ["*"]               # all registered tools
```

A user's effective tool set is the union over their roles. `*` matches anything **registered** — it never grants access to a tool that doesn't exist.

If `tools` for a role grants an id that's not registered at startup, the gateway logs a WARN and ignores it (fail-soft, otherwise a stale config breaks startup).

## Tool injection

On `POST /v1/chat/completions`:

1. Compute the caller's allowed-tool set (`roles → ids → registered Tool` instances).
2. If the request body already has `tools`, **union** with the allowed set, de-dupe by `function.name`. Client-supplied tools are not executed by the gateway — they round-trip to the client like normal OpenAI tools. (Mixed mode: gateway-tools and client-tools coexist in the same completion.)
3. If `tool_choice` is `"required"` or specifies a tool name, leave it alone.
4. Forward to the upstream.

## Tool-call loop

When the upstream returns a non-streaming response with `choices[*].message.tool_calls`:

```text
   ┌───────────────────────────────────────────┐
   │  classify the turn's tool_calls:          │
   │     gateway-owned = id in registry        │
   │     client-owned  = any other name        │
   ├───────────────────────────────────────────┤
   │  if NO gateway-owned:                      │
   │     return response to client (it drives)  │
   │  elif ANY client-owned (mixed turn):       │
   │     return whole turn to client unchanged  │
   │  else (gateway-owned only):                │
   │     run tools, append {role:"tool", ...},  │
   │     re-POST upstream with extended msgs,    │
   │     repeat                                  │
   └───────────────────────────────────────────┘
```

**Why a mixed turn yields to the client.** On the proxy path the *client*
owns the conversation history — it re-sends every message each request. We
can only run a turn fully server-side (looping until a final answer) when
that turn calls our tools and ours alone. If a single assistant turn calls
both a gateway tool and a client tool, we can't run ours *and* hand control
back mid-turn without either dropping the client's call or leaving it
unanswered in the next upstream round (which the upstream rejects). So we
hand the entire turn back; the client runs its tool and re-submits, and the
model re-emits the gateway call on a subsequent gateway-only turn. Mixed
turns are rare in practice (models seldom batch a gateway and a client tool
in one turn).

Bounds:
- **Max rounds per request**: 10. Configurable; exceeding returns a `500` with `code = "tool_loop_exhausted"`.
- **Per-tool timeout**: 30s default, overridable per tool.
- **Concurrency**: tool calls within one round run concurrently (`futures::future::join_all`), with a per-request semaphore of 4.

### Streaming

When the *client* requested `stream: true`:
- Each round streams the upstream SSE through to the client live, but gateway-owned `tool_calls` deltas (and their `finish_reason: "tool_calls"` terminator) are suppressed — the client must not see calls it can't run. The accumulated calls are executed server-side and the loop re-POSTs for the next round.
- The **final** round (the one that produces no gateway-tool tool_calls) streams straight through, terminator and all.
- **Mixed / client-owned turns**: if a round's tool_calls include any client-owned name, the suppressed calls are re-materialised as one synthesized assistant delta + a `finish_reason: "tool_calls"` chunk so the client receives the full turn, then the stream ends (`[DONE]`). The client runs its tool and re-submits — same yield-to-client rule as the buffered path.

## What the user sees

A user with the `finance` role calling `POST /v1/chat/completions` with `{"model": "...", "messages": [...]}` gets a normal OpenAI response. The model may have invoked `company_invoice_lookup` zero or many times along the way; the response includes only the final assistant message.

Audit-log records (per round): user, model, tool ids invoked, args (hashed if sensitive), latency, success/failure.

## What's intentionally out of scope (initially)

- **User-defined tools.** All tools are code-defined.
- **Per-user (not per-role) tool grants.** Roles are the only granularity. If we need exceptions later, we add an explicit grants table.
- **Tool result caching.** Tools run every time they're called.
- **MCP bridging.** Discussed at kickoff; deferred. The trait shape doesn't preclude it — an `McpTool` could implement `Tool` later.
