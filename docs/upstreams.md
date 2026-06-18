# Upstreams (multi-provider routing + load balancing)

The gateway routes each request to one of several upstream LLM backends based on the requested model name. **Routes are not declared statically** — the health probe parses each backend's `/models` response and the registry routes by what each upstream reports it serves. Load a model on a backend in the right kind of pool and it becomes routable automatically.

## Core abstraction

```text
request.model ──► [walk pools matching kind] ──► [pool whose backends advertise model]
              ──► [pool picker among healthy backends that have the model] ──► HTTP upstream
```

- A **`Backend`** is a single addressable upstream: base URL, optional API key, weight, `max_inflight`, plus a runtime-populated set of advertised model IDs.
- A **`Pool`** is an ordered set of backends sharing a `kind` (`chat` | `transcription` | `embedding`) and a picker strategy. Pools own:
    - A health-check loop per backend.
    - A picker strategy (`round_robin`, `least_inflight`). Default: `least_inflight`.
    - Implicit "what we serve" — the union of all backends' advertised-model sets.

`crates/gateway/src/server/upstreams/` owns the runtime: `config.rs` parses the TOML, `registry.rs` walks pools per request, `health.rs` runs the probe loop.

## Config shape

```toml
# gateway.toml
[upstream_pools.local_chat]
kind = "chat"
strategy = "least_inflight"

[[upstream_pools.local_chat.backend]]
name = "gpu-01"
base_url = "http://gpu-01.internal:8000/v1"
weight = 1
max_inflight = 16
# api_key_env = "BACKEND_GPU01_KEY"  # optional; for hosted providers

[[upstream_pools.local_chat.backend]]
name = "gpu-02"
base_url = "http://gpu-02.internal:8000/v1"
weight = 1
max_inflight = 16

[upstream_pools.local_whisper]
kind = "transcription"
strategy = "round_robin"

[[upstream_pools.local_whisper.backend]]
name = "whisper-01"
base_url = "http://whisper-01.internal:9000/v1"
```

No `[[models]]` table. Each backend's `/models` response is the source of truth for what it serves.

Secret material (`api_key_env`) is **only** sourced from env vars.

## Model discovery

Every 5 s, each backend gets a `GET <base_url>/models` probe (with the backend's bearer token, if configured). On 200 + parseable OpenAI envelope (`{"data": [{"id": ...}, ...]}`), the backend's advertised-model set is **replaced wholesale** with the names in `data[].id`. On 401 or non-parseable 200, the backend is marked alive but its model set is left as-is (so a previously-populated set survives a transient parser failure). On network error, timeout, or 5xx, the probe counts toward the unhealthy threshold.

At startup, `health::spawn` runs an initial parallel probe round and awaits it before returning, so the first request lands on a registry that already knows what each backend serves. Worst case (every backend unreachable): the gateway waits the 2 s probe timeout and starts serving with empty model sets, returning `400 invalid_request` until the looping probe populates them.

### Routing rules

When a request arrives with `model = "X"` and the handler asks for `PoolKind::Chat`:

1. Walk pools where `pool.kind == Chat`.
2. Find the first one with at least one **healthy** backend whose advertised set contains `"X"`.
3. From that pool, the picker strategy orders the candidate set; the first non-saturated backend gets an inflight slot.

If two pools of the same kind advertise the same model, the first one we iterate wins. `HashMap` iteration order isn't deterministic, so production deployments shouldn't rely on a tie-breaker — keep one pool per kind in practice.

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

- Add a model on a backend → it shows up in `/v1/models` and the chat picker within 5 s.
- Drop a model → it disappears from routing within 5 s (next probe).
- Want to verify? Check `tracing` output: every model-set change logs `advertised models updated added=[...] removed=[...] total=N`.
