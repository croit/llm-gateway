# Tools + RBAC

How server-side tools work. For the list of *which* tools exist and when each
is registered, see [`tools-inventory.md`](tools-inventory.md).

This document names **symbols, not file paths or line numbers** — the crate
layout is actively being reshaped (see the crate-split work), and a doc pinned
to paths goes stale on the next move. `grep` for a symbol; it will be wherever
it lives today.

## What a tool is

A **tool** is a Rust handler the gateway runs on behalf of an LLM during a chat
completion. From the model's side it's an ordinary OpenAI function-calling
tool: it has a JSON schema, the model emits `tool_calls`, the gateway executes
them, and the result feeds the next round. The model doesn't know the tool ran
on the gateway.

This is **not** a passthrough and not an MCP broker. The gateway *is* the tool
runtime. (It *also* bridges MCP servers — see "Tool sources" — but that is one
source of tools among several, not the architecture.)

### Why server-side

- Tools reach internal systems (databases, APIs, file stores) we don't want to
  expose to every model client.
- Running them here means we control the inputs, can rate-limit, and can audit.
- Clients need no extra wiring. Any OpenAI SDK sees a normal completion.

## The `Tool` trait

```rust
pub trait Tool: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn schema(&self) -> ToolDef;
    fn run<'a>(&'a self, ctx: ToolContext, args: Value) -> ToolFuture<'a>;
    fn max_duration(&self) -> Option<std::time::Duration> { None }
}
```

- `ToolFuture<'a>` is `Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>`,
  hand-rolled rather than pulling `async_trait` in for one trait. (`async_trait`
  *is* a dependency elsewhere — `SessionDriver` needs it — but the tool trait
  doesn't use it.)
- `id()` returns `&str`, **not** `&'static str`: MCP- and ComfyUI-bridged tools
  build their ids at runtime from server / workflow names.
- `schema()` is generated per call so it can interpolate runtime context. Most
  tools return a constant.
- `max_duration()` overrides the runner's per-tool timeout. The sandbox tools
  use it — their jobs (LibreOffice, headless Chromium, cold start) legitimately
  exceed 30s.

### Tool ids are a contract

`ToolRegistry::with` asserts at registration that an id matches OpenAI's
function-name regex `^[a-zA-Z0-9_-]{1,64}$`, and panics on a duplicate.

That assertion exists because a `.` in an id (a dotted namespace like
`company.echo`) *silently* breaks against strict tool-call parsers — qwen3-coder
is the one that bit us: the parser either drops the call or rewrites the name
before emitting `tool_calls`, and the symptom looks like the model ignoring the
tool. Use `_` for namespacing. Failing at boot beats shipping a tool that only
breaks on some upstreams.

### `ToolContext`

Carries the caller's identity plus the handles a tool may need, so adding a
dependency doesn't change the trait signature:

- **Identity / RBAC** — `user_id`, `roles`, `client_ip`.
- **Storage** — `db` (the SQLite pool), `s3` (chat attachments; `None` without
  `[chat.s3]`), `crypto` (the at-rest key, for tools that read a sealed
  operator setting).
- **Conversation** — `assistant_turn_id`, `session_id`: `Some` only on the
  chat-UI path. The `/v1` proxy paths have no session, which is what
  `requires_chat_session` exists for (below). `attachment_reservations`
  serialises filename picks across tool calls that run concurrently in one
  round.
- **Optional subsystems** — `geoip`, `indexer` (RAG), `image_gen`,
  `sandbox_lease` (one container per turn, so successive `run_in_sandbox` calls
  share `/work`), `chat_feedback` (push UI onto the live SSE stream and await a
  browser reply — `get_user_location` asks for a position, `ask_user` asks a
  question; one hub per reply shape, so the two endpoints can't un-park each
  other's tool), `push` (Web Push, for `notify_user`).
- **The current model** — `model`, when the path resolved one. Carried so a
  tool that creates work to be *run later* can inherit it instead of guessing a
  pool id: `schedule_action` gives the action it writes the same model the user
  is talking to.

Two of these carry a **per-turn budget**, not just a handle, because the
context's lifetime is exactly one turn and that is the window a limit needs:
`attachment_reservations` (a mutex, so concurrent uploaders can't pick the same
filename) and `push` (a latch — `PushNotifier::claim` succeeds once, so a model
in a tool loop can't turn someone's phone into a notification feed). Neither
belongs in `server::limits`, which counts tokens and cost per user over time and
has no notion of a turn.

Every optional field is `None` where that subsystem isn't configured, and tools
are expected to degrade with a clear message rather than assume. Tools never
receive the caller's OIDC access token — acting *as* the user against a
downstream service is an explicit per-integration concern (see the per-user MCP
connectors).

There is no `tracing::Span` field: per-tool spans are created by the runner
around `run`, not threaded through the context.

## Registration

Tools are registered in `main`, into a `ToolRegistry`:

```rust
let registry = ToolRegistry::new()
    .with(tools::echo::Echo)
    .with(tools::search_web::SearchWeb)
    .with(tools::fetch_attachment::FetchAttachment::new(sandbox.clone()));
```

We do **not** auto-discover tools at runtime. Adding one means writing code,
opening a PR, and reviewing it — which is the point.

Many tools are registered conditionally (no GeoIP database → no `lookup_ip`),
so the model is never offered a tool whose every call could only answer "not
configured". The gate for each is in
[`tools-inventory.md`](tools-inventory.md).

### Presentation metadata

Two lookup tables in `catalog` decide how a tool appears on `/tools`:
`category_for` (which group) and `display_meta` (hand-written plain-language
title and description — the schema description is written for an LLM and reads
as jargon in a settings list). Both fall through gracefully: an unknown id
lands in `Category::Utility` with its schema text. That makes forgetting them
*silent*, so the inventory drift guard asserts every registered id has a real
category and hand-written copy.

Internal plumbing is kept off the toggle surfaces by `is_hidden` —
`company_echo` (the loop's smoke test) and `enable_tools` (whose toggle would
be inert, since the session allow-list force-keeps it).

## Tool sources

The runner resolves tools through `ToolSource`, not the concrete registry:

```rust
pub trait ToolSource: Send + Sync {
    fn get(&self, id: &str) -> Option<Arc<dyn Tool>>;
    fn defs_for(&self, allowed: &[String]) -> Vec<ToolDef>;
    fn ids(&self) -> Vec<String>;
    fn contains(&self, id: &str) -> bool { self.get(id).is_some() }
}
```

`ToolRegistry` implements it, and so does `CompositeToolSource`, which overlays
per-request tools on top: a user's connected MCP connectors
(`mcp__<server>__<tool>`) and the hot-reloadable ComfyUI workflow catalog
(`comfyui_<workflow>`). One seam means the buffered `/v1` loop, the streaming
`/v1` loop, and the chat-UI driver all gain per-user tools identically.

## Lazy tool disclosure (`enable_tools`)

The defining behaviour of the current design, and the thing most likely to
surprise: **tools start off**. A conversation's advertised set is

```
allowed_tools_for_session = (RBAC-granted ∩ registered)
                            ∩ ({enable_tools} ∪ per-conversation enabled)
```

Short tool lists are cheaper and models pick from them more accurately. When a
request needs a capability the model doesn't currently have, it calls
`enable_tools` with one or more **toggle keys**; that writes per-conversation
rows, and the real schemas appear from the next round on and stay for the rest
of the conversation. Calling a tool directly without enabling it first still
works — it just costs a round.

Two tools bypass the gate:

- `enable_tools` itself is force-kept (`BOOTSTRAP_TOOL_ID`), or the model could
  never turn anything on.
- `read_skill` is force-kept *when the caller has at least one permitted
  skill*, because the system prompt advertises those skills every turn; making
  the model enable the loader first would be pointless friction. With no
  permitted skills it stays lazy.

Related tools collapse onto **one** toggle key, because users reason in
capabilities rather than function names: `remember` + `recall` → `memory`; the
canvas tools (including `export_document`) → `document`; a typst template's
render / edit / read / pptx family → its render id; all `comfyui_*` →
`comfyui`; all of one MCP server's tools → `mcp__<server>`. `entry_key_for`
maps id → key, `retain_enabled` applies a disabled set.

The advertised order is deliberate: `enable_tools` first (identical across
every conversation), then the tail sorted by toggle key then id. That keeps the
serialised tool block byte-stable, so list churn doesn't invalidate the
upstream prompt cache.

## RBAC

Roles come from the OIDC roles claim (configurable — see [`auth.md`](auth.md)),
mapped onto internal role ids. A user's effective tool set is the **union over
their roles**, and grants come from several sources that are unioned:

- **Static config** — each role's `tools` list in `[[roles]]`; `["*"]` means
  everything *registered*. `*` never grants a tool that doesn't exist, and a
  granted id that isn't registered at boot is logged and ignored (fail-soft, so
  a stale config can't block startup).
- **Groups** — `/admin/groups` maps OIDC claim values onto gateway groups with
  their own tool and skill grants. Pools, RAG collections, and MCP connectors
  restrict access by group.
- **Per-user toggles** — each user turns their granted tools on and off on
  `/tools`.
- **Per-token scoping** — a `gwk_…` token can be scoped to a subset of its
  owner's tools.
- **Skills** — a role's `skills` list plus a per-skill grant editor in the UI;
  `read_skill` rides along for any role granted a skill.

```toml
[[roles]]
id = "engineering"
models = ["*"]
tools = ["search_web", "rag_search", "run_in_sandbox"]

[[roles]]
id = "admin"
models = ["*"]
tools = ["*"]               # everything registered
```

The layers compose in one direction only: RBAC (roles + groups) decides what a
user *may* use; the per-conversation, per-token, and per-user layers can only
subtract.

## Tool injection

On `POST /v1/chat/completions`:

1. Compute the caller's allowed set (roles → ids → resolvable in the
   `ToolSource`).
2. Drop anything `requires_chat_session` — the proxy paths have no chat turn,
   so advertising those would hand the model a guaranteed error instead of a
   completion. It is a single source of truth precisely so the advertise filter
   can't drift from the runtime gate; it has drifted before.
3. If the request body already carries `tools`, **union** with the allowed set,
   de-duped by `function.name`. Client-supplied tools are never executed here —
   they round-trip to the client like normal OpenAI tools, so gateway tools and
   client tools coexist in one completion.
4. Leave `tool_choice` alone when it is `"required"` or names a tool.
5. Forward upstream.

`requires_chat_session` covers two different reasons a tool needs the chat path,
and it is worth keeping them apart when deciding whether a new tool belongs
there:

- **Nowhere to put the output.** `upload_attachment`, `generate_image`,
  `export_document` — they attach something to a turn that doesn't exist on
  `/v1`.
- **Nobody to ask.** `ask_user` needs a human watching the stream, and so do
  `schedule_action` and `delete_scheduled_action`, which require an `ask_user`
  confirmation before writing. That confirmation exists because a scheduled
  action later runs **as the user**, unattended, until removed — persistence is
  what makes prompt injection there worth a human "yes", where an ordinary tool
  call isn't. A useful consequence: a scheduled run cannot create further
  scheduled actions, because the headless worker has a session but no watcher,
  so the confirmation goes unanswered and the write is refused.

The inverse case is worth stating too, since it is the easy mistake: a tool
whose *optional* argument needs a session does **not** belong here.
`render_typst` renders inline `source` anywhere and only needs a session for its
`document_id` path; marking it chat-only would remove a working capability from
`/v1` to protect an argument a proxy caller has no use for. Reusable pattern:
`crate::ask_user::confirm` gives any tool the card + rendezvous, and returns
`Confirmation::NoAnswer` off the chat path so the caller decides what "no human
here" means for its own operation.

## The tool-call loop

When a response carries `choices[*].message.tool_calls`:

```text
   ┌───────────────────────────────────────────┐
   │  classify the turn's tool_calls:          │
   │     gateway-owned = resolvable in source  │
   │     client-owned  = any other name        │
   ├───────────────────────────────────────────┤
   │  if NO gateway-owned:                     │
   │     return response to client (it drives) │
   │  elif ANY client-owned (mixed turn):      │
   │     return whole turn to client unchanged │
   │  else (gateway-owned only):               │
   │     run tools, append {role:"tool", …},   │
   │     re-POST upstream with extended msgs,  │
   │     repeat                                │
   └───────────────────────────────────────────┘
```

**Why a mixed turn yields to the client.** On the proxy path the *client* owns
the conversation history — it re-sends every message each request. We can only
run a turn fully server-side when that turn calls our tools and ours alone. If
one assistant turn calls both a gateway tool and a client tool, we can't run
ours *and* hand control back mid-turn without either dropping the client's call
or leaving it unanswered in the next upstream round (which the upstream
rejects). So we hand the entire turn back; the client runs its tool and
re-submits, and the model re-emits the gateway call on a later gateway-only
turn. Mixed turns are rare in practice — models seldom batch a gateway and a
client tool in one turn.

### Bounds

- **Rounds per turn** — `MAX_TOOL_ROUNDS` = **16**. A compile-time constant,
  not configurable, shared by all three loops (buffered `/v1`, streaming `/v1`,
  chat-UI driver) so they can't silently diverge again. Exceeding it surfaces
  as `500` with `code = "internal_error"` and the message `tool-call loop
  exhausted after N rounds`.
- **Per-tool timeout** — 30s, overridable per tool via `max_duration`.
- **Concurrency** — tool calls within one round run concurrently, bounded by a
  per-request semaphore of 4.
- **Tool-result context budget** — once cumulative `role:"tool"` content passes
  128 KB, older large results are replaced by re-callable stubs while the last
  few stay verbatim. It triggers on size only, so short conversations keep the
  full history and the prompt cache intact (clearing would invalidate the
  cached prefix).

### Streaming

When the client asked for `stream: true`:

- Each round streams the upstream SSE through live, but gateway-owned
  `tool_calls` deltas (and their `finish_reason: "tool_calls"` terminator) are
  suppressed — the client must not see calls it can't run. The accumulated
  calls execute server-side and the loop re-POSTs for the next round.
- The **final** round (the one producing no gateway-tool calls) streams
  straight through, terminator and all.
- **Mixed / client-owned turns** — the suppressed calls are re-materialised as
  one synthesized assistant delta plus a `finish_reason: "tool_calls"` chunk so
  the client receives the full turn, then the stream ends (`[DONE]`). Same
  yield-to-client rule as the buffered path.

## Returning images from a tool

A tool result is normally JSON, stringified into the `role:"tool"` message. For
image bytes that isn't enough — a tool result has no other channel for them. A
tool can instead return `tool_content_parts(vec![…])`, an envelope keyed by
`TOOL_CONTENT_PARTS_KEY`, and the driver emits `content: [ …parts… ]` (an array
of OpenAI content parts) rather than a JSON string. `fetch_attachment` and
`fetch_url` use it to hand a vision model a real `image_url` part.

The envelope is honoured only when the sentinel is the object's *sole* key, so
a tool that happens to carry a field of that name isn't misinterpreted.

## What the user sees

A caller with the `engineering` role POSTing to `/v1/chat/completions` gets a
normal OpenAI response. The model may have invoked tools zero or many times
along the way; the response carries only the final assistant message.

Audit records per round: user, model, tool ids invoked, arguments, latency,
success/failure.

## Intentionally out of scope

- **User-defined tools.** All tools are code-defined and reviewed.
- **Tool result caching.** Tools run every time they are called.
- **Sub-agent delegation / multi-agent orchestration.** The gateway is a tool
  runtime behind an OpenAI-compatible API; a client that needs agent
  orchestration builds it on its own side. Adding it here would complicate
  round bounding, cost attribution, RBAC scoping, and usage accounting, with no
  benefit to a plain `/v1/chat/completions` caller.
