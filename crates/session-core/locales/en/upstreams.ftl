# Strings owned by `gateway/src/rama_server/pages/upstreams.rs` — the merged
# `/admin/upstreams` page (pools + backends). Reuses many `pools-*` and
# `backends-*` keys; only the page chrome specific to the merged view lives here.

upstreams-page-title = Upstreams — LLM Gateway
upstreams-heading = Upstreams
upstreams-description = Pools group backends by kind and picker strategy. Health, load and served models are probed live. Topology edits are saved to the database and take effect on Apply changes.

upstreams-add-pool = Pool
upstreams-add-backend = Backend
upstreams-cancel = Cancel
upstreams-edit-pool = Edit pool
upstreams-edit-backend = Edit backend
upstreams-delete-confirm = Really delete?

# Sticky apply bar (shown while there are unapplied topology edits).
upstreams-apply-count = unapplied changes
upstreams-apply-note = — the runtime registry still serves the previous topology.

# Compliance indicators in the pool header.
upstreams-comp-gdpr = GDPR
upstreams-comp-nda = NDA
upstreams-comp-limits = limits

# A backend that exists in the DB but is not yet in the runtime registry.
upstreams-backend-pending = pending apply

# Tooltip on a struck-through model chip: discovered via the /models probe but
# withheld because the pool's model list (allowlist) doesn't name it.
upstreams-model-withheld-title = Discovered via /models but withheld by this pool's model list — not served or advertised.

upstreams-unassigned-heading = Unassigned
upstreams-unassigned-description = Backends not assigned to any pool. Add one to a pool to route traffic to it.

upstreams-empty = No pools or backends configured yet. Add a pool or a backend to get started.
