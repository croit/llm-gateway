# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/admin.rs` — the
# `/admin/models` page for server-wide per-model sampling defaults
# and reasoning-effort/budget overrides.

admin-page-title = Modell-Standardwerte — LLM Gateway
admin-heading = Modell-Standardwerte
admin-intro-prefix = Serverweite Standard-Sampling-Parameter für dieses Modell, in TOML. Diese gelten für
admin-intro-every = jede
admin-intro-middle = Anfrage für dieses Modell, von jedem Benutzer oder Token — es sei denn, der Aufrufer setzt denselben Schlüssel in seiner eigenen Anfrage, was
admin-intro-always-wins = immer gewinnt
admin-intro-suffix = . Betrachte es als die Untergrenze, die jeder erhält, wenn er keine eigenen Werte angibt. Leer = keine Standardwerte, das eingebaute Verhalten des Backends gilt.
admin-no-models = Noch keine Chat-Modelle verfügbar. Sobald ein Upstream-Backend erreichbar ist, erscheint es hier.

admin-toml-placeholder-header = # Häufige Schlüssel (vLLM/OpenAI):
admin-toml-defaults-label = TOML-Standardwerte
admin-save = Speichern

admin-reasoning-style-aria = Reasoning-Stil
admin-reasoning-auto = Reasoning: Automatisch
admin-reasoning-none = Reasoning: keins
admin-reasoning-qwen = Reasoning: Qwen (vLLM)
admin-reasoning-openai = Reasoning: OpenAI
admin-reasoning-glm = Reasoning: GLM / z.AI
admin-reasoning-anthropic = Reasoning: Anthropic

admin-effort-standard = Standard
admin-effort-deep = Tief
admin-effort-max = Max
admin-budget-placeholder = Standard
admin-budget-hint = Maximale Denk-Token pro Stufe. Leer = Backend-Standard (unbegrenzt). „Fast“ deaktiviert das Reasoning.
admin-effort-default-option = (Standard)
admin-effort-hint = Reasoning-Aufwand pro Stufe. Leer = eingebauter Standard. „Fast“ deaktiviert das Reasoning.
admin-save-reasoning-budget = Reasoning-Budget speichern

admin-malformed-form = fehlerhaftes Formular: { $err }
admin-missing-model-name = Feld model_name fehlt
admin-db-delete-error = DB-Löschung: { $err }
admin-cleared-defaults = Standardwerte für `{ $model }` gelöscht
admin-invalid-toml = ungültiges TOML: { $err }
admin-db-upsert-error = DB-Upsert: { $err }
admin-saved-defaults = Standardwerte für `{ $model }` gespeichert
admin-unknown-reasoning-style = unbekannter Reasoning-Stil `{ $style }`
admin-db-error = DB: { $err }
admin-saved-reasoning-style = Reasoning-Stil für `{ $model }` gespeichert
admin-budget-not-positive = Budget `{ $value }` muss eine positive Ganzzahl sein
admin-unknown-reasoning-effort = unbekannter Reasoning-Aufwand `{ $value }`
admin-saved-reasoning-budget = Reasoning-Budget für `{ $model }` gespeichert
