# Tool context optimization — on-demand tool enablement

> Status: **design / not started**. Scope decision locked: enablement is
> **per-conversation** (see "Data model"). This doc covers the full arc
> (manual → suggested → automated); only the early phases are committed work.

## Problem

Every chat turn injects the **full** JSON schema of every enabled tool into the
request's `tools` array — name, description, and all parameter descriptions.
There is no per-message relevance routing: `runner::inject_tools`
(`crates/gateway/src/server/tools/runner.rs`) maps the user's entire
allowed-tool set through `registry::defs_for` and appends all of it, on every
turn, for the whole session.

Today that's **17 built-in tools + however many MCP tools**. As a concrete
data point, `typst_letter` alone is ~1,300 tokens of schema; a rough total for
the always-on block is **~6–10k tokens per request** (measure before acting —
see Phase 0). The model pays this whether or not the turn has anything to do
with the tools.

Mitigation that already exists: the tool list is a **stable prefix**, so the
self-hosted vLLM upstream prefix-caches it once and reuses it across turns and
conversations. So the dominant cost here is **context-window budget** and
**cold-prefill latency**, not per-token dollars (self-hosted, unmetered). Still
real, and it grows linearly as the catalog — especially MCP — expands.

## Goals

- Shrink the always-on tool block to a small core; make the rest opt-in.
- Give the user a fast, in-context way to enable tools/groups for a chat.
- Eventually auto-enable the relevant tools from the user's message, without a
  manual step and without injecting everything.
- Preserve the prefix-cache benefit (don't churn the tool block every turn).

## Non-goals

- Changing RBAC. Role grants remain the outer bound; enablement only ever
  *narrows* what a user could already access.
- Per-message tool scoping (too fiddly; per-conversation is the unit — below).
- Replacing the runtime correction loop. `InvalidArgs` feedback stays the
  mechanism that fixes malformed calls; this is about *which* tools are present.
- Hardcoded language-specific keyword/word lists for routing or matching. The
  product is multilingual; routing stays language-agnostic (embeddings, not word
  lists). See "Automated enablement."

## Background: how tools reach the model today

- `AppState::allowed_tools_for_user(roles, user_id)` (`state.rs:56`) =
  `rbac.allowed_tools(role_ids)` **minus** `user_tool_prefs::disabled_for_user`
  (`retain_enabled`). Prefs are **global per user** today.
- That set is threaded as `allowed_tools` into the driver and injected by
  `runner::inject_tools` → `registry::defs_for(allowed)` → appended to `tools`
  (merging with any client-supplied tools).
- `catalog.rs` already classifies tools into `Category { Web, Documents,
  Memory, Integrations, Utility }` with human titles — this is our group
  primitive.
- Conversations are `chat_sessions` rows (`crates/session-core/src/db.rs`),
  one per thread, scoped to `user_id`.

## Design overview

A two-tier model layered on the existing pipeline:

1. **Always-on core** — a small, high-value set injected every turn (e.g.
   memory, time, maybe web search). Stays cached.
2. **Opt-in tools/groups** — enabled **per conversation**, by the user (manual),
   by a model suggestion (confirmed), or by an automated router. Once enabled
   for a conversation, a tool **stays** enabled for that conversation's
   remaining turns (sticky — see Caching).

The injection point doesn't change; only the computation of `allowed_tools`
gains a per-conversation overlay:

```
effective = rbac_allowed
          ∩ (core ∪ conversation_enabled)      // new per-conversation overlay
          − user_disabled                       // existing global prefs
```

## Tool groups

Reuse `Category`. Proposed defaults (tune later):

| Group         | Members (examples)                                   | Default |
|---------------|------------------------------------------------------|---------|
| Memory        | remember, recall                                     | **on**  |
| Utility/Time  | current_timestamp, get_user_location, convert_currency | **on** (core) |
| Web           | search_web, fetch_url, wikipedia                     | on*     |
| Documents     | typst_* (letter, …)                                  | **off** |
| Network       | lookup_ip, dns, whois, tls, netcheck                 | **off** |
| Integrations  | mcp__*                                               | **off** |

\*Web is the judgement call — high utility but several hundred tokens. Could be
core or opt-in. (Network may warrant splitting out of `Utility` into its own
category.)

## Manual enablement (UI)

- A composer-level control (button/menu) listing groups with on/off state for
  **this conversation**, plus the same on the `/tools` page for global defaults.
- Toggling a group flips its members for the conversation; takes effect on the
  next turn.
- Affordance: when the model errors because a needed tool isn't enabled, surface
  a one-click "enable Documents and retry."

## Model-suggested enablement

Cheap bridge to automation, no new infra: the always-on system context can let
the model say *"I can draft this as a letter — enable Document tools?"* The UI
renders a confirm chip; on click, the gateway enables the group for the
conversation and re-runs. Human stays in the loop; uses the model already
running.

## Automated enablement (embedding router)

> **Rejected: language-specific keyword/word lists.** Hardcoded match lists
> (`Brief|Angebot|letter|invoice` → Documents) are explicitly out. The product
> is multilingual (German + English today, more later); per-language word lists
> are brittle and unmaintainable, and we will never want them. Routing must be
> language-agnostic.

The headline automated variant, lighter → heavier:

1. **Embedding similarity (recommended).** Embed each tool's short purpose once
   at startup; per request embed the latest user message, cosine top-k over a
   threshold → enable those (+ core). "Tool RAG." ~ms latency, scales with the
   catalog, and is naturally language-agnostic given a multilingual embedder.
2. **Tiny classifier / cheap first-pass LLM** — most flexible, most latency.

Three setup-specific requirements:

- **Multilingual is mandatory.** Users write German ("Schreibe einen Brief")
  against English tool descriptions. The embedding model must be multilingual,
  or tool stubs must be embedded bilingually — otherwise German requests won't
  match. This rules out English-only embedders.
- **Sticky enablement (protects the cache).** Auto-injecting a different set
  every turn churns the stable prefix vLLM caches. Rule: once a tool is enabled
  for a conversation it stays on, so the prefix re-stabilizes after the first
  relevant turn.
- **Miss recovery.** The model can't call a tool that wasn't injected. On a
  miss, the model signals "I need a document tool" → gateway enables + re-runs
  (a round-trip, but only on misses). The manual button is the human override
  for the same gap.

## Optional dependency & graceful degradation

**The embedding router is optional and fail-open. The system is fully correct
without any embedding model.** Manual enablement (Phase 1) is the baseline
source of truth; the router only ever *adds* auto-enablements on top. Removing
or breaking it costs UX (user enables groups manually), nothing else.

This mirrors the existing optional-capability idiom — `ToolContext` already
carries `geoip: Option<GeoIp>` and `s3: Option<S3Config>`, `None` when the
config block is absent, with dependent features degrading rather than failing.
The router is the same shape: `Option<EmbeddingRouter>`, `None` when there's no
`[embeddings]` config.

Degradation at every layer:

| Condition | Behaviour |
|-----------|-----------|
| No `[embeddings]` configured | Router never constructed; manual route only. |
| Endpoint unreachable at boot | Log a warning, run without the router; do **not** fail startup. |
| Errors / exceeds latency budget mid-request | Timeout-wrapped; on failure proceed with `core ∪ manually-enabled`, never block or error the turn. |
| Router was up earlier, down now | Tools already auto-enabled persist (sticky, in `chat_session_tools`); no retraction. |

The embedding model **need not be Qwen** — any multilingual model behind an
OpenAI-compatible `/v1/embeddings` endpoint (ideally a small dedicated
multilingual embedder, bge-m3 / multilingual-e5 class). It fits the existing
`kind`-tagged `upstream_pools` (`"chat"` / `"transcription"` → add
`"embedding"`). Tool stubs embed once at startup; per request is one cheap
message embedding.

## Data model

Per-conversation enablement needs new state (today `user_tool_prefs` is global).
Options:

- **New table `chat_session_tools(session_id, tool_key, enabled, source)`** where
  `source ∈ {manual, suggested, auto}` — keeps an audit trail of *why* a tool is
  on (useful for tuning the router). Preferred.
- Or a JSON column on `chat_sessions` (simpler, no audit/source granularity).

`source` matters: it lets us log router decisions and distinguish user intent
from automation.

## Caching impact

- Keep the **core** identical across all conversations → one shared cached
  prefix, as today.
- Per-conversation enabled tools append **after** the core, in a stable order
  (`defs_for` already sorts deterministically). Sticky enablement keeps that
  tail stable within a conversation, so only the *first* turn that enables a
  tool pays a fresh prefill for the added block.
- Net: the core is globally cached; each conversation warms its own tail once.

## Failure modes & mitigations

| Risk | Mitigation |
|------|-----------|
| Router false-negative (relevant tool absent) | miss-recovery round-trip + manual button |
| Router false-positive | bounded by top-k / threshold |
| Model calls a tool it can't see | it can't — injection is the gate; rely on recovery |
| German/other-language miss | multilingual embedder, bilingual stubs |
| Opaque routing | persist `source` + scores; log injected set per turn |
| Cache churn from per-turn changes | sticky enablement |

## Phasing

**Phase 0 — Measure.** Sum the real always-on tool-block tokens (per tool and
total) for a typical user. Confirms ROI and sets the core/opt-in line.
*Done when:* a number exists and the core set is chosen.

**Phase 1 — Groups + per-conversation manual enable.** `chat_session_tools`
table; overlay in `allowed_tools_for_user` (now conversation-aware); composer
control; default heavy groups off.
*Done when:* a user can enable Documents for one chat, it persists for that
chat, and `typst_letter` is absent from other chats' requests (verified via the
upstream tool list / `x-gateway-tool-rounds` path).

**Phase 2 — Model-suggested enablement.** System-context affordance + confirm
chip + enable-and-rerun.
*Done when:* asking for a letter with Documents off yields a suggestion chip
that, on click, enables the group and produces the letter.

**Phase 3 — Embedding auto-router.** Startup tool embeddings (multilingual),
per-request top-k, sticky, manual override, decision logging.
*Done when:* German and English letter requests auto-enable Documents without a
manual step, the choice is sticky for the conversation, misses fall back to the
recovery path, and — with `[embeddings]` unset or the endpoint down — the
gateway runs normally on the manual route with no other impact.

Phases 0–2 capture most of the win with no new model. Phase 3 is where the
embedding model earns its keep — primarily as the MCP catalog grows.

## Open questions

- Is **Web** core or opt-in? (utility vs ~few-hundred tokens)
- Embedding model: which multilingual embedder, and where does it run (existing
  GPU box vs a small CPU model)?
- Top-k / threshold defaults, and whether to cap the auto-enabled set size.
- Should `source=auto` enablement decay (drop after N turns unused) or stay
  sticky for the whole conversation? (Sticky is simpler and cache-friendlier.)
