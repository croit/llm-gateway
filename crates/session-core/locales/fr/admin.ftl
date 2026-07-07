# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/admin.rs` — the
# `/admin/models` page for server-wide per-model sampling defaults
# and reasoning-effort/budget overrides.

admin-page-title = Paramètres par défaut du modèle — LLM Gateway
admin-heading = Paramètres par défaut du modèle
admin-intro-prefix = Paramètres d'échantillonnage par défaut pour ce modèle, à l'échelle du serveur, en TOML. Ils s'appliquent à
admin-intro-every = chaque
admin-intro-middle = requête pour ce modèle, quel que soit l'utilisateur ou le jeton — sauf si l'appelant définit la même clé dans sa propre requête, laquelle
admin-intro-always-wins = l'emporte toujours
admin-intro-suffix = . Considérez cela comme le plancher que chacun obtient s'il ne précise pas ses propres valeurs. Vide = aucune valeur par défaut, le comportement intégré du backend s'applique.
admin-no-models = Aucun modèle de chat annoncé pour l'instant. Dès qu'un backend en amont sera accessible, il apparaîtra ici.

admin-toml-placeholder-header = # Clés courantes (vLLM/OpenAI) :
admin-toml-defaults-label = Valeurs par défaut TOML
admin-save = Enregistrer

admin-reasoning-style-aria = Style de raisonnement
admin-reasoning-auto = Raisonnement : Auto
admin-reasoning-none = Raisonnement : aucun
admin-reasoning-qwen = Raisonnement : Qwen (vLLM)
admin-reasoning-openai = Raisonnement : OpenAI
admin-reasoning-glm = Raisonnement : GLM / z.AI
admin-reasoning-anthropic = Raisonnement : Anthropic

admin-effort-standard = Standard
admin-effort-deep = Approfondi
admin-effort-max = Max
admin-budget-placeholder = par défaut
admin-budget-hint = Nombre maximal de jetons de réflexion par niveau. Vide = valeur par défaut du backend (illimité). « Fast » désactive le raisonnement.
admin-effort-default-option = (par défaut)
admin-effort-hint = Effort de raisonnement par niveau. Vide = valeur par défaut intégrée. « Fast » désactive le raisonnement.
admin-save-reasoning-budget = Enregistrer le budget de raisonnement

admin-malformed-form = formulaire mal formé : { $err }
admin-missing-model-name = champ model_name manquant
admin-db-delete-error = suppression en base : { $err }
admin-cleared-defaults = valeurs par défaut effacées pour `{ $model }`
admin-invalid-toml = TOML invalide : { $err }
admin-db-upsert-error = upsert en base : { $err }
admin-saved-defaults = valeurs par défaut enregistrées pour `{ $model }`
admin-unknown-reasoning-style = style de raisonnement inconnu `{ $style }`
admin-db-error = base de données : { $err }
admin-saved-reasoning-style = style de raisonnement enregistré pour `{ $model }`
admin-budget-not-positive = le budget `{ $value }` doit être un entier positif
admin-unknown-reasoning-effort = effort de raisonnement inconnu `{ $value }`
admin-saved-reasoning-budget = budget de raisonnement enregistré pour `{ $model }`
