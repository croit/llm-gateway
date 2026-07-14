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

# Éditeur CRUD des backends (ajout/modification/suppression de backends stockés dans la topologie en base).
backends-manage-heading = Gérer les backends
backends-manage-description = Ajoutez, modifiez ou supprimez des backends en amont. Les modifications sont enregistrées en base mais ne prennent effet qu'une fois que vous cliquez sur « Appliquer les modifications ».
backends-apply-changes = Appliquer les modifications
backends-add-heading = Ajouter un backend
backends-field-name = Nom
backends-field-base-url = URL de base
backends-field-api-key-env = Variable d'env de la clé API
backends-field-health-path = Chemin de santé
backends-field-weight = Poids
backends-field-max-inflight = Max en cours
backends-field-pool = Pool
backends-field-pool-none = (aucun)
backends-field-pool-hint = Affecte ce backend à un pool. Un backend présent dans plusieurs pools est réduit à celui choisi ici.
backends-field-models = Modèles (séparés par des virgules)
backends-field-aliases = Alias (name=target par ligne)
backends-field-probe-models = Découvrir les modèles via la sonde /models
backends-field-supports-edit = Prend en charge l'édition d'images
backends-save-backend = Enregistrer le backend
backends-add-backend = Ajouter un backend
backends-delete-backend = Supprimer
backends-error-name-required = le nom du backend est requis
backends-error-base-url-required = l'URL de base est requise
backends-saved = backend `{ $name }` enregistré — cliquez sur « Appliquer les modifications » pour recharger
backends-deleted = backend `{ $name }` supprimé — cliquez sur « Appliquer les modifications » pour recharger

backends-field-api-key = Clé API
backends-field-api-key-placeholder = Clé API (stockée chiffrée)
backends-field-api-key-keep = laisser vide pour conserver la clé actuelle
