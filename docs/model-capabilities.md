# Model Capabilities & DB-Driven Upstream Configuration

> **Status:** Implemented (migration 0042 + admin UI + auto-learning + vision fallback)

## What changed

The upstream pool/backend topology moved from `config.toml` to the database, managed through
the admin UI. On top of that, a per-model **capability system** lets the gateway know whether
a model supports vision, tool calling, structured output, etc. — and **auto-learns** from
upstream errors when it doesn't.

## Three layers of capability awareness

### 1. Admin-configured (source of truth)

The admin sets capabilities per model via `/admin/models` → expandable "Capabilities" section:

- **Tri-state** per capability: `Unknown` (try and learn) / `Enabled` / `Disabled`
- **Fallback model** dropdowns: which model to use for image description when the primary
  model lacks vision, etc.

### 2. Auto-learning (safety net)

When the gateway forwards a request and the upstream returns a 400 that matches a known
capability-rejection pattern (e.g. GLM's `"messages.content.type is invalid"`), the
[`error_classify`](../../crates/gateway-core/src/server/upstreams/error_classify.rs) module
identifies which capability was rejected and records it in the DB via
[`mark_unsupported`](../../crates/gateway-core/src/server/db/model_defaults.rs).

Key property: **auto-learning never overwrites an admin-set `Enabled`** — the SQL uses
`CASE WHEN col = 1 THEN 1 ELSE 0 END` to preserve explicit `Some(true)` values. Only
`None` (unknown) gets flipped to `Some(false)`.

### 3. Vision fallback (transparent describe-and-inject)

When a tool result contains image content and the primary model's `vision = false` with a
`fallback_vision` model configured, the gateway:

1. Sends the image to the fallback model: *"Describe this image in detail"*
2. Replaces the `image_url` content part with a text part containing the description
3. Emits a visible info banner in the chat: *"The selected model (X) has no vision support.
   Using Y to describe the image and attaching the text description instead."*

## DB topology tables

Migration `0042_upstream_config_db.sql` created:

| Table | Purpose |
|---|---|
| `backends` | Upstream API connections (URL, key env, weight, health path) |
| `pools` | Routing groups (kind, strategy, compliance, enforce_limits) |
| `pool_backends` | Many-to-many pool ↔ backend |
| `pool_models` | Pool-level fallback model IDs |
| `backend_models` | Backend-level static model IDs |
| `backend_aliases` | Client-facing alias → real model mapping |
| `pool_voices` | Language → voice map for speech pools |
| `fallback_models` | Per-kind unknown-model fallback |

Plus 9 new columns on `model_defaults` for capabilities + fallback refs.

## Config seeding (backward compat)

On first boot after migration, if the DB has no pools and `config.toml` has `[upstream_pools]`,
the startup sequence seeds the DB from TOML (`seed_from_config`). After that, the DB is the
source of truth and the TOML sections are ignored.

## Registry hot-reload

The `UpstreamRegistry` holds its pool/backend data behind an `ArcSwap` (lock-free). The admin
clicks "Apply changes" (`POST /admin/upstreams/reload`) to:

1. Load the full topology snapshot from DB
2. Build new `Pool`/`Backend` structs
3. Atomically swap the ArcSwap
4. Spawn fresh health probes for the new backends

All 50+ call sites (`state.upstreams.route(...)`, etc.) are unchanged — the ArcSwap is
internal to the registry.

## Capability tri-state semantics

| State | Behavior |
|---|---|
| `None` (unknown) | Try the request. If upstream returns a capability-rejection 400, auto-learn (`None` → `Some(false)`). |
| `Some(true)` | Proceed normally — the model supports this capability. Auto-learning never overwrites this. |
| `Some(false)` + no fallback | Reject early with a clear error: *"This model cannot process images."* |
| `Some(false)` + fallback configured | Route the content to the fallback model transparently. Show info banner. |

## Files

| File | Role |
|---|---|
| `server/upstreams/error_classify.rs` | Pattern-matches upstream 400s → `CapabilityField` |
| `server/capabilities.rs` | Vision describe-and-inject logic |
| `server/upstreams/db_bridge.rs` | `UpstreamConfigSnapshot` ↔ config structs |
| `server/db/upstreams_config.rs` | DB CRUD + `load_snapshot` + `seed_from_config` |
| `rama_server/pages/admin.rs` | Capabilities UI + reload endpoint |
| `session-core/src/workers.rs` | `TurnUpdate::InfoMessage` for chat notifications |
