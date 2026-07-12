# Strings owned by `gateway/src/rama_server/pages/pools.rs` — the
# `/admin/pools` CRUD editor for the DB-backed upstream pool topology.

pools-page-title = Upstream pools — LLM Gateway
pools-heading = Upstream pools
pools-description = Group backends into pools by kind and picker strategy. Changes are saved to the database but only take effect once you click "Apply changes".

pools-fallbacks-heading = Unknown-model fallbacks
pools-fallbacks-description = When a request names a model the gateway has never heard of, substitute this model for that kind. Blank = the miss returns 404.

pools-add-heading = Add pool
pools-field-name = Name
pools-field-kind = Kind
pools-field-strategy = Strategy
pools-field-fallback-offline = Offline fallback model
pools-field-fallback-offline-placeholder = served when every backend is down
pools-field-models = Models (comma-separated)
pools-field-voices = Voices (lang=voice per line)
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
