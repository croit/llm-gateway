# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/admin.rs` — la page
# `/admin/models`.

admin-page-title = Modèles — LLM Gateway
admin-heading = Modèles
admin-intro-prefix = Réglages par modèle — tarifs, fenêtre de contexte, raisonnement, capacités et valeurs d'échantillonnage — appliqués à
admin-intro-every = chaque
admin-intro-middle = requête pour ce modèle, quel que soit l'utilisateur ou le jeton, sauf si l'appelant définit la même valeur, laquelle
admin-intro-always-wins = l'emporte toujours
admin-intro-suffix = . Les modèles de chat, les alias et les autres types sont tous dans une seule liste.
admin-no-models = Aucun modèle annoncé pour l'instant. Dès qu'un backend en amont sera accessible, il apparaîtra ici.

admin-filter-placeholder = Filtrer les modèles…
admin-filter-all = Tous
admin-filter-chat = chat
admin-filter-other = autres types
admin-filter-aliases = alias
admin-filter-configured = configurés uniquement

admin-col-model = Modèle
admin-col-kind = Type
admin-col-price = Prix ent/sor
admin-col-context = Contexte
admin-col-reasoning = Raisonnement
admin-col-configured = Configuré

admin-value-default = défaut
admin-value-na = s/o
admin-not-configured = non configuré
admin-alias-inherits = hérite des réglages de la cible
admin-reasoning-auto-resolved = Auto → { $style }

admin-badge-price = PRIX
admin-badge-ctx = CTX
admin-badge-budget = BUDGET
admin-badge-caps = CAPS
admin-badge-toml = TOML

admin-save-model = Enregistrer le modèle
admin-clear-overrides = Effacer tous les réglages
admin-cancel = Annuler
admin-other-price-note = L'échantillonnage, le raisonnement et le contexte ne s'appliquent pas à ce type — seuls les tarifs, pour la comptabilité des coûts.

admin-toml-placeholder-header = # Clés courantes (vLLM/OpenAI) :
admin-toml-defaults-label = Valeurs d'échantillonnage (TOML)

admin-reasoning-style-label = Style de raisonnement
admin-reasoning-style-aria = Style de raisonnement
admin-reasoning-auto = Auto
admin-reasoning-none = aucun
admin-reasoning-qwen = Qwen (vLLM)
admin-reasoning-openai = OpenAI
admin-reasoning-glm = GLM / z.AI
admin-reasoning-anthropic = Anthropic

admin-effort-standard = Standard
admin-effort-deep = Approfondi
admin-effort-max = Max
admin-budget-placeholder = par défaut
admin-budget-hint = Nombre maximal de jetons de réflexion par niveau. Vide = valeur par défaut du backend (illimité). « Fast » désactive le raisonnement.
admin-effort-default-option = (par défaut)
admin-effort-hint = Effort de raisonnement par niveau. Vide = valeur par défaut intégrée. « Fast » désactive le raisonnement.

admin-malformed-form = formulaire mal formé : { $err }
admin-missing-model-name = champ model_name manquant
admin-db-delete-error = suppression en base : { $err }
admin-invalid-toml = TOML invalide : { $err }
admin-db-upsert-error = upsert en base : { $err }
admin-saved-model = `{ $model }` enregistré — effet immédiat
admin-cleared-defaults = réglages effacés pour `{ $model }`
admin-unknown-reasoning-style = style de raisonnement inconnu `{ $style }`
admin-db-error = base de données : { $err }
admin-budget-not-positive = le budget `{ $value }` doit être un entier positif
admin-unknown-reasoning-effort = effort de raisonnement inconnu `{ $value }`
admin-context-window-invalid = la fenêtre de contexte `{ $value }` doit être un entier positif

# Tarifs par modèle pour la comptabilité des coûts (prix par 1 M de jetons, entrée / sortie).
admin-price-label = { $cur }/{ $unit }
admin-price-unit-tokens = 1 M de tokens
admin-price-unit-images = image
admin-price-unit-characters = caractère
admin-price-unit-seconds = seconde
admin-price-in-label = Prix ent
admin-price-out-label = Prix sor
admin-price-in-placeholder = sans tarif
admin-price-out-placeholder = sans tarif
admin-price-invalid = le prix `{ $value }` doit être un nombre non négatif

# Fenêtre de contexte (pilote la compaction automatique).
admin-context-window-full-label = Fenêtre de contexte (jetons)
admin-context-window-placeholder = défaut

admin-alias-chip = alias

# Modèles par défaut par fonctionnalité.
admin-defaults-heading = Modèles par défaut
admin-defaults-intro = Choisissez le modèle présélectionné pour chaque fonctionnalité. Vide = le premier modèle disponible (comportement précédent).
admin-defaults-chat-label = Chat
admin-defaults-voice-label = Voix (transcription)
admin-defaults-image-label = Génération d'images
admin-defaults-embedding-label = Embedding (RAG)
admin-defaults-first-option = Premier disponible
admin-defaults-saved = modèle par défaut défini sur `{ $model }`
admin-defaults-cleared = modèle par défaut réinitialisé
admin-defaults-unknown-feature = fonctionnalité inconnue `{ $feature }`

# Capacités du modèle (tri-état) + modèles de repli.
admin-capabilities-heading = Capacités
admin-cap-vision = Vision
admin-cap-tools = Outils
admin-cap-structured-output = Sortie structurée
admin-cap-audio-input = Entrée audio
admin-cap-pdf-input = Entrée PDF
admin-cap-parallel-tools = Outils en parallèle
admin-cap-unknown = Inconnu
admin-cap-enabled = Activé
admin-cap-disabled = Désactivé
admin-cap-no-fallback = (aucun)
admin-cap-fallback-vision = Repli pour la vision
admin-cap-fallback-tools = Repli pour les outils

# Rechargement de la topologie upstream ("Apply changes" sur /admin/upstreams).
admin-reloaded = { $pools } pools, { $backends } backends rechargés
admin-reload-error = échec du rechargement : { $err }

# Backend de recherche web (outil `search_web`).
admin-search-heading = Recherche web
admin-search-intro = Quel backend répond à l'outil `search_web` de l'assistant. SearXNG ne nécessite qu'une URL de base et ne coûte rien par requête si vous hébergez votre propre instance ; Brave nécessite une clé d'API. La clé est chiffrée au repos.
admin-search-provider-label = Fournisseur
admin-search-provider-searxng = SearXNG (auto-hébergé)
admin-search-provider-brave = Brave Search API
admin-search-searxng-url-label = URL de base SearXNG
admin-search-searxng-url-placeholder = https://searxng.example.com
admin-search-brave-key-label = Clé d'API Brave
admin-search-brave-key-placeholder = laisser vide pour conserver la clé actuelle
admin-search-brave-key-set = Une clé est enregistrée (chiffrée).
admin-search-brave-key-unset = Aucune clé enregistrée.
admin-search-brave-key-clear = Supprimer la clé enregistrée
admin-search-save = Enregistrer la recherche web
admin-search-saved = paramètres de recherche web enregistrés
admin-search-unknown-provider = fournisseur de recherche inconnu `{ $provider }`
