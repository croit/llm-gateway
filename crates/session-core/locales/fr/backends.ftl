# STATUS: llm-generated, unreviewed — pending native-speaker QA

backends-page-title = Backends en amont — LLM Gateway
backends-heading = Backends en amont
backends-description-prefix = Vue en direct des pools en amont configurés — état, charge en cours par rapport à la limite de chaque backend, et les modèles que chacun propose actuellement. Lecture seule : le routage dépend entièrement de ce que les backends signalent via leur
backends-description-suffix = sonde.
backends-summary = { $total } backends · { $healthy } opérationnels · { $down } hors service
backends-unknown-fallback-prefix = Repli pour modèle inconnu —
backends-empty-prefix = Aucun pool en amont configuré. Ajoutez un bloc
backends-empty-suffix = à gateway.toml et redémarrez.

backends-fallback-offline-title = fallback_offline : utilisé lorsque tous les backends d'un modèle connu de ce pool sont hors service
backends-fallback-offline-badge = hors ligne ↩ { $model }
backends-pool-empty = Aucun backend dans ce pool.

backends-status-down = hors service
backends-status-saturated = saturé
backends-status-up = actif

backends-inflight-label = en cours { $load }
backends-activity-summary = 15 min { $m15 } · 30 min { $m30 } · 60 min { $m60 }
backends-no-models = aucun modèle proposé
backends-aliases-label = alias :

backends-alias-target-title = alias → { $target }
backends-alias-disabled-label = { $name } (désactivé)
backends-alias-disabled-title = alias simple désactivé — ce backend propose plusieurs modèles ; indiquez-lui une cible explicite (formulaire de correspondance)
backends-alias-bare-title = alias → modèle de ce backend
