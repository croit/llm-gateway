# Upstreams (multi-provider routing + load balancing)

The gateway routes each request to one of several upstream LLM backends based on the requested model name. **Routes are not declared statically** — the health probe parses each backend's `/models` response and the registry routes by what each upstream reports it serves. Load a model on a backend in the right kind of pool and it becomes routable automatically.

## Core abstraction

```text
request.model ──► [walk pools matching kind] ──► [pool whose backends advertise model]
              ──► [pool picker among healthy backends that have the model] ──► HTTP upstream
```

- A **`Backend`** is a single addressable upstream: base URL, optional API key, weight, `max_inflight`, plus a runtime-populated set of advertised model IDs.
- A **`Pool`** is an ordered set of backends sharing a `kind` (`chat` | `transcription` | `embedding` | `image` | `speech` | `ocr`) and a picker strategy. Pools own:
    - A health-check loop per backend.
    - A picker strategy (`round_robin`, `least_inflight`). Default: `least_inflight`.
    - Implicit "what we serve" — the union of all backends' advertised-model sets.

`crates/gateway-core/src/server/upstreams/` owns the runtime: the topology is loaded from the database (edited in the UI at `/admin/upstreams`), `registry.rs` walks pools per request, and `health.rs` runs the probe loop.

## Configuring pools & backends

Pools, backends, and per-model settings are configured **in the admin UI at `/admin/upstreams`** — the only supported path; there is no config-file topology. This section describes the fields you set there and how they behave; the [operator workflow](#operator-workflow) below has the click-path.

A **pool** has a name, a `kind` (`chat` | `transcription` | `embedding` | `image` | `speech` | `ocr`), a picker `strategy` (`least_inflight` — recommended — or `round_robin`), optional GDPR/NDA compliance flags, a rate-limit-exemption toggle, and an optional offline-fallback model. An `ocr` pool is reserved for internal document parsing and is not a general-purpose chat endpoint.

A **backend** belongs to one pool and carries a name, base URL, an API key (entered once, stored encrypted; an env-var name can be given as a fallback), weight, max in-flight, health path, client-facing aliases, and two capability flags:

- **Discover models from probe** (`probe_models`, default on). When off, the health probe is a pure liveness check and never overwrites the backend's configured model list. Turn it off for image/speech backends whose `/models` returns a *chat* catalog (z.AI's general endpoint, OpenAI) — otherwise the probe replaces the real model ids, makes them unroutable, and pollutes `/v1/models`. Such backends instead get an explicit model list (on the backend, or on the pool).
- **Supports image editing** (`supports_edit`, default off). Marks an image backend as capable of editing. The `edit_image` tool is only registered when some image backend sets this, and editing is additionally refused against a backend whose pool is non-GDPR (it would ship existing user images off-site).

A **speech** pool also takes an optional voice map — one voice id per spoken language (lowercase ISO-639-1), plus a default used when no language matches; voice mode resolves the voice from the language the STT detected. Unlike other kinds, a speech pool has **no unknown-model fallback** — a mistyped model or voice just surfaces the backend's own error. The chat UI's voice mode appears only when both a speech pool and a transcription model exist (see [`ui.md`](ui.md)).

An **ocr** pool is used internally for document parsing and is not exposed through
the public chat model list. Its backend is an internal document-aware OCR
sidecar, not the raw vLLM OpenAI endpoint. The sidecar may use the official
`infer.py --pdf` wrapper; it owns PDF rasterization and sends the model's
required image requests and `vllm_xargs` values itself.

There is no static model table: each backend's `/models` response is the source of truth for what it serves. API keys are stored encrypted at rest; the optional env-var fallback is the only place key material comes from the environment.

For aliases and the two fallback mechanisms, see [Model aliases](#model-aliases) and [Fallback models](#fallback-models) below.

## Model discovery

Every 5 s, each backend gets a `GET <base_url>/models` probe (with the backend's bearer token, if configured). On 200 + parseable OpenAI envelope (`{"data": [{"id": ...}, ...]}`), the backend's advertised-model set is **replaced wholesale** with the names in `data[].id`. On 401 or non-parseable 200, the backend is marked alive but its model set is left as-is (so a previously-populated set survives a transient parser failure). On network error, timeout, or 5xx, the probe counts toward the unhealthy threshold.

At startup, `health::spawn` runs an initial parallel probe round and awaits it before returning, so the first request lands on a registry that already knows what each backend serves. Worst case (every backend unreachable): the gateway waits the 2 s probe timeout and starts serving with empty model sets, returning `400 invalid_request` until the looping probe populates them.

### Routing rules

When a request arrives with `model = "X"` and the handler asks for `PoolKind::Chat`:

1. Walk pools where `pool.kind == Chat`.
2. Find the first one with at least one **healthy** backend whose advertised set contains `"X"`.
3. From that pool, the picker strategy orders the candidate set; the first non-saturated backend gets an inflight slot.

If two pools of the same kind advertise the same model, the first one we iterate wins. `HashMap` iteration order isn't deterministic, so production deployments shouldn't rely on a tie-breaker — keep one pool per kind in practice.

## Model aliases

An **alias** is a stable, client-facing name that routes to a real model, decoupling *what clients ask for* from *which model is actually loaded*. The problem it solves: without aliases a client hardcodes `Qwen/Qwen2.5-72B-Instruct`; the day you load `Qwen/Qwen3-235B` instead, the old id vanishes and every connected client `404`s until it's reconfigured. Point clients at the alias `qwen`, swap the loaded model, keep the alias — and nothing downstream changes.

Aliases are set **per backend**, in the Add/Edit backend form's **Aliases** field (one `name=target` per line). The same alias on several backends forms a **routing group**: a request for the alias load-balances across all of them via the normal pool picker, exactly as if they all advertised one shared model id. Both the alias *and* the real id are routable, and both appear in `/v1/models` — asking for the real id still pins that exact model.

There are two forms, and you pick **one per backend**:

- **Bare name** (the GPU norm) — a name with no target (leave the `=target` off, e.g. just `qwen`). It binds to the one model that backend serves, so it needs no target.
- **`name=target`** — required on a **multi-model** backend (e.g. a cloud provider serving many models behind one base URL), where a bare name couldn't tell which model it means. For example `smart=glm-4.6` and `cheap=glm-4.5-air` on two lines.

Both forms combine freely *across* backends into one group — a bare `qwen` on a GPU box and `qwen=glm-4.6` on a cloud box share the same `qwen` group. A real model id always wins over an alias of the same spelling.

When a request routes through an alias the gateway **rewrites the outgoing request body's `model` field to the resolved real id** — upstreams only know their own model ids, never the alias. The response therefore reports the real model that ran, and an `X-Gateway-Resolved-Model` response header records what the alias resolved to (only when it differs from what the client sent). Admin sampling/reasoning defaults key on the **real id**, so aliases inherit them automatically — configure defaults once, under the real model name.

### Alias validation

- **At registry build — refuse to start.** Statically-detectable conflicts: an alias name that collides with a real model id in the topology, or a `name=target` alias whose target isn't in that backend's configured `models`. The gateway logs a clear line and exits non-zero.
- **At runtime — log + disable.** A backend's real model set is discovered from its `/models` probe, so some conflicts can't be known at boot. If a *bare* alias lands on a backend that the probe reveals serves more than one model, it's ambiguous: the gateway logs an `ERROR`, disables just that alias binding (it stops resolving and drops out of `/v1/models`), and keeps routing the backend's real ids by their own names. Detection runs when the probe updates the model set — on the transition only, not every probe.

## Fallback models

Two independent safety nets for two different failures. Both are optional; leaving them unset reproduces the previous behavior exactly (`404` / `503`).

- **Unknown-model fallback (per kind).** When a request names a model that is neither a real id nor any alias, substitute a configured default *for that request kind*. Answers "the client asked for something we've never heard of" — a typo, or a model that got renamed. Unset ⇒ `404 model_not_found`. Set these in the **Unknown-model fallbacks** editor on `/admin/upstreams` (one auto-saving picker per kind).
- **Offline fallback (per pool).** When a model *is* known but no healthy backend can currently serve it, spill to a backup model — typically a different tier (local GPUs down → a cloud model). Answers "we know this model, our capacity for it is just down right now." Unset ⇒ `503`. Set it in the pool editor's **offline fallback** field.

For example: an unknown chat model routes to `qwen` and an unknown embedding model to `text-embedding-3-small` (leave a kind's picker empty to keep returning `404`); a chat pool whose replicas all go down spills to `glm-4.6`.

A fallback target is **re-resolved through the normal path**, so it may itself be an alias/group and lands on whatever healthy pool serves it. Fallback is a **single hop**: if the fallback target is *also* unavailable, the gateway returns the original `404`/`503` rather than chaining — no loops. Saturation (a healthy model whose backends are all at `max_inflight`) is **not** a fallback trigger — that stays a `503`, so a request never silently downgrades to a weaker model under mere load. Note the RAG embedding path deliberately does **not** apply `fallback_offline`: embeddings from a different model aren't comparable and would corrupt the index.

### Resolution order

The alias group is itself the first line of resilience: with `qwen` on both `gpu-a` and `gpu-b`, one backend failing still routes to the other — `fallback_offline` only fires when the *whole* group is down.

```mermaid
flowchart TD
    A["request: model = M, kind = K"] --> B{"M is a real id, or an alias<br/>on a backend of kind K?"}
    B -- "yes — a healthy backend serves it" --> C["acquire in-flight slot<br/>rewrite body model → resolved real id<br/><b>forward upstream</b>"]
    B -- "no healthy backend" --> D{"is M known to a pool of kind K?<br/>(i.e. all its replicas are down)"}
    D -- "yes — known but offline" --> E{"pool.fallback_offline set?"}
    D -- "no — M is unknown" --> F{"[fallback].K set?"}
    E -- yes --> G["re-resolve the fallback target<br/>(single hop — no chaining)"]
    E -- no --> H["503 — no healthy backend"]
    F -- yes --> G
    F -- no --> I["404 model_not_found"]
    G --> C
```

## Health checks

The same probe drives liveness *and* discovery. Three consecutive failures mark a backend `unhealthy`; one success returns to `healthy`. Unhealthy backends are skipped both for routing and for discovery (their previous model set lingers but doesn't contribute matches because the registry filters by `is_healthy()`).

For backends that don't speak OpenAI-compatible `/models`, override `health_path` per backend. The probe will still mark liveness from the HTTP status, but won't be able to register any model IDs — those backends won't appear in routing decisions unless the upstream serves OpenAI-style on the override path.

## Picking strategies

- `round_robin`: per-pool atomic counter, mod len. Skips unhealthy + non-advertising backends.
- `least_inflight` (default): track in-flight count per backend (incr on dispatch, decr on response close, including streamed responses); pick the lowest. Adapts automatically to slow backends without accurate weights.

## In-flight accounting + back-pressure

A backend's `max_inflight` is a hard cap. When all backends in a pool that advertise the requested model are at cap, the gateway returns `503` (logged at WARN). We don't queue server-side; clients re-drive.

## Streaming caveat

For streaming requests, "in-flight" lasts until the response body is fully drained. Accounting goes through the `Acquired` RAII guard returned by `acquire_for(model, kind)`.

## Transcription (Whisper-style)

`POST /v1/audio/transcriptions` accepts `multipart/form-data` with `file`, `model`, optional `language`, `prompt`, `response_format`, `temperature`. The gateway:
- Verifies auth + RBAC against the `model` field.
- Routes via `acquire_for(model, PoolKind::Transcription)` — same routing layer, same discovery path as chat.
- VAD-trims the audio and forwards the multipart body to the upstream.
- Returns the upstream response as-is.

We do **not** transcode audio in the gateway — upstreams handle the formats they support.

## Operator workflow

- Add, edit, or remove pools and backends at `/admin/upstreams`, then click **Apply changes** to reload the runtime registry (a sticky bar counts unapplied edits). Topology edits are saved to the database and take effect without a restart.
- Add a model on a backend → it shows up in `/v1/models` and the chat picker within 5 s.
- Drop a model → it disappears from routing within 5 s (next probe).
- Want to verify? Check `tracing` output: every model-set change logs `advertised models updated added=[...] removed=[...] total=N`.
