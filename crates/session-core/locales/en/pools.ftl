# Strings owned by `gateway/src/rama_server/pages/pools.rs` — the
# `/admin/pools` CRUD editor for the DB-backed upstream pool topology.

pools-page-title = Upstream pools — LLM Gateway
pools-heading = Upstream pools
pools-description = Group backends into pools by kind and picker strategy. Changes are saved to the database but only take effect once you click "Apply changes".

pools-fallbacks-heading = Unknown-model fallbacks
pools-fallbacks-description = The substitute when a request names a model no pool serves (unlike the per-feature default on the Models page, which applies when a request names nothing). Blank = the miss returns 404.

pools-add-heading = Add pool
pools-field-name = Name
pools-field-kind = Kind
pools-field-strategy = Strategy
pools-field-fallback-offline = Offline fallback model
pools-field-fallback-offline-placeholder = served when every backend is down
pools-field-models = Served models (allowlist, comma-separated)
pools-field-models-hint = When set, only these ids are served from a probing backend — the rest are shown struck-through. Blank = serve everything the backend reports.
pools-field-allowed-groups = Allowed groups
pools-field-allowed-groups-hint = Comma-separated gateway groups allowed to see + use this pool's models. Blank = everyone. Admins always have access. Manage groups in Admin → Groups.
pools-field-voices = Voices (lang=voice per line)
pools-field-offer-voices = Selectable voices (one per line, users pick)
pools-field-backends = Backends
pools-no-backends = No backends defined yet. Add one on the Backends page first.
pools-field-gdpr = GDPR compliant
pools-field-nda = NDA covered
pools-field-enforce-limits = Enforce rate limits & quotas
pools-save-pool = Save pool
pools-add-pool = Add pool
pools-delete-pool = Delete

pools-error-name-required = pool name is required
pools-error-invalid-kind = invalid pool kind `{ $kind }`
pools-saved = saved pool `{ $name }` — click "Apply changes" to reload
pools-deleted = deleted pool `{ $name }` — click "Apply changes" to reload
pools-fallback-saved = { $kind } fallback set to `{ $model }`
pools-fallback-cleared = { $kind } fallback cleared
