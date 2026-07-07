# Strings owned by `gateway/src/rama_server/pages/backends.rs` — the
# read-only `/admin/backends` operator view of the upstream pools.

backends-page-title = Upstream backends — LLM Gateway
backends-heading = Upstream backends
backends-description-prefix = Live view of the configured upstream pools — health, in-flight load against each backend's cap, and the models each one currently advertises. Read-only: routing is driven entirely by what the backends report on their
backends-description-suffix = probe.
backends-summary = { $total } backends · { $healthy } healthy · { $down } down
backends-unknown-fallback-prefix = Unknown-model fallback —
backends-empty-prefix = No upstream pools configured. Add an
backends-empty-suffix = block to gateway.toml and restart.

backends-fallback-offline-title = fallback_offline: served when every backend for a known model in this pool is down
backends-fallback-offline-badge = offline ↩ { $model }
backends-pool-empty = No backends in this pool.

backends-status-down = down
backends-status-saturated = saturated
backends-status-up = up

backends-inflight-label = inflight { $load }
backends-activity-summary = 15m { $m15 } · 30m { $m30 } · 60m { $m60 }
backends-no-models = no models advertised
backends-aliases-label = aliases:

backends-alias-target-title = alias → { $target }
backends-alias-disabled-label = { $name } (disabled)
backends-alias-disabled-title = bare alias disabled — this backend serves multiple models; give it an explicit target (map form)
backends-alias-bare-title = alias → this backend's model
