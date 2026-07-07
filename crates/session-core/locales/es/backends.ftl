# STATUS: llm-generated, unreviewed — pending native-speaker QA

backends-page-title = Backends de origen — LLM Gateway
backends-heading = Backends de origen
backends-description-prefix = Vista en vivo de los pools de origen configurados — estado, carga en curso frente al límite de cada backend y los modelos que cada uno ofrece actualmente. Solo lectura: el enrutamiento depende por completo de lo que los backends informan en su
backends-description-suffix = sonda.
backends-summary = { $total } backends · { $healthy } saludables · { $down } caídos
backends-unknown-fallback-prefix = Alternativa para modelo desconocido —
backends-empty-prefix = No hay pools de origen configurados. Añada un bloque
backends-empty-suffix = a gateway.toml y reinicie.

backends-fallback-offline-title = fallback_offline: se usa cuando todos los backends de un modelo conocido en este pool están caídos
backends-fallback-offline-badge = fuera de línea ↩ { $model }
backends-pool-empty = No hay backends en este pool.

backends-status-down = caído
backends-status-saturated = saturado
backends-status-up = activo

backends-inflight-label = en curso { $load }
backends-activity-summary = 15m { $m15 } · 30m { $m30 } · 60m { $m60 }
backends-no-models = no se anuncian modelos
backends-aliases-label = alias:

backends-alias-target-title = alias → { $target }
backends-alias-disabled-label = { $name } (desactivado)
backends-alias-disabled-title = alias simple desactivado — este backend sirve varios modelos; asígnele un destino explícito (formulario de mapeo)
backends-alias-bare-title = alias → modelo de este backend
