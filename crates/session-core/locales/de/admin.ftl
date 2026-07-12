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

admin-context-window-label = Kontext
admin-context-window-unit = Tok
admin-context-window-placeholder = Standard
admin-context-window-aria = Kontextfenster (Tokens)
admin-context-window-invalid = Kontextfenster `{ $value }` muss eine positive Ganzzahl sein
admin-context-window-saved = Kontextfenster für `{ $model }` gesetzt
admin-context-window-cleared = Kontextfenster für `{ $model }` gelöscht

# Preise pro Modell für die Kostenabrechnung (Preis pro 1 Mio. Tokens, Eingabe / Ausgabe).
admin-price-label = Preis ({ $cur })
admin-price-in-placeholder = ein
admin-price-out-placeholder = aus
admin-price-in-aria = Eingabepreis pro 1 Mio. Tokens
admin-price-out-aria = Ausgabepreis pro 1 Mio. Tokens
admin-price-unit = /1M
admin-price-invalid = Preis `{ $value }` muss eine nicht-negative Zahl sein
admin-price-saved = Preise für `{ $model }` gesetzt

# Standardmodelle pro Funktion (im Chat/Sprach-Auswahlmenü vorausgewählt und
# als API-Fallback, wenn ein Aufruf kein Modell angibt).
admin-defaults-heading = Standardmodelle
admin-defaults-intro = Wählen Sie das Modell, das für jede Funktion vorausgewählt ist. Leer = das erste verfügbare Modell (bisheriges Verhalten).
admin-defaults-chat-label = Chat
admin-defaults-voice-label = Sprache (Transkription)
admin-defaults-image-label = Bildgenerierung
admin-defaults-embedding-label = Embedding (RAG)
admin-defaults-first-option = Erstes verfügbares
admin-defaults-saved = Standardmodell auf `{ $model }` gesetzt
admin-defaults-cleared = Standardmodell zurückgesetzt
admin-defaults-unknown-feature = unbekannte Funktion `{ $feature }`
admin-other-heading = Weitere Modelle (Preise)
admin-other-intro = Embedding-, Bild-, Sprach- und Transkriptionsmodelle. Sampling- und Reasoning-Einstellungen gelten nicht, aber setze Preise pro 1 Mio. Tokens, damit ihre Nutzung in die Kostenabrechnung und Kostenlimits einfließt.

# Alias-Karte: Modellnamen, die Aliase für ein anderes (echtes) Modell sind.
admin-aliases-heading = Aliase
admin-aliases-intro = Diese Namen sind Aliase für ein anderes Modell. Sie haben keine eigenen Einstellungen oder Preise — jede Anfrage wird als das aufgelöste Zielmodell konfiguriert und abgerechnet.
admin-alias-chip = Alias

# Modell-Fähigkeiten (Vision, Tools, Structured Output) + Fallback-Modelle.
admin-capabilities-heading = Fähigkeiten
admin-cap-unknown = Unbekannt
admin-cap-enabled = Aktiviert
admin-cap-disabled = Deaktiviert
admin-cap-structured-output = Strukturierte Ausgabe
admin-cap-no-fallback = (keiner)
admin-cap-fallback-vision = Fallback für Vision
admin-cap-fallback-tools = Fallback für Tools
admin-capabilities-saved = Fähigkeiten gespeichert für `{ $model }`
admin-capabilities-error = Fähigkeiten konnten nicht gespeichert werden: { $err }

# Upstream-Topologie neu laden ("Apply changes"-Button).
admin-reloaded = { $pools } Pools, { $backends } Backends neu geladen
admin-reload-error = Neuladen fehlgeschlagen: { $err }
