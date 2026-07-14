# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/admin.rs` — die Seite
# `/admin/models`: Standardmodell-Auswahl plus eine filterbare Liste aller
# angebotenen Modelle mit einem einzigen konsolidierten Editor (Preise,
# Kontextfenster, Reasoning-Stil + Budgets/Aufwand, Fähigkeiten, Sampling).

admin-page-title = Modelle — LLM Gateway
admin-heading = Modelle
admin-intro-prefix = Einstellungen pro Modell — Preise, Kontextfenster, Reasoning, Fähigkeiten und Sampling-Standardwerte — angewendet auf
admin-intro-every = jede
admin-intro-middle = Anfrage für dieses Modell, von jedem Benutzer oder Token, es sei denn, der Aufrufer setzt denselben Wert, was
admin-intro-always-wins = immer gewinnt
admin-intro-suffix = . Chat-Modelle, Aliase und andere Arten sind alle in einer Liste.
admin-no-models = Noch keine Modelle verfügbar. Sobald ein Upstream-Backend erreichbar ist, erscheint es hier.

# Listen-Werkzeugleiste: Textfilter + sich gegenseitig ausschließende Chips.
admin-filter-placeholder = Modelle filtern…
admin-filter-all = Alle
admin-filter-chat = chat
admin-filter-other = andere Arten
admin-filter-aliases = Aliase
admin-filter-configured = nur konfigurierte

# Spaltenüberschriften der Liste.
admin-col-model = Modell
admin-col-kind = Art
admin-col-price = Preis ein/aus
admin-col-context = Kontext
admin-col-reasoning = Reasoning
admin-col-configured = Konfiguriert

# Werte in eingeklappten Zeilen.
admin-value-default = Standard
admin-value-na = n/v
admin-not-configured = nicht konfiguriert
admin-alias-inherits = erbt Einstellungen des Ziels
admin-reasoning-auto-resolved = Auto → { $style }

# „Konfiguriert“-Facetten-Badges.
admin-badge-price = PREIS
admin-badge-ctx = KTX
admin-badge-budget = BUDGET
admin-badge-caps = FÄHIG
admin-badge-toml = TOML

# Editor.
admin-save-model = Modell speichern
admin-clear-overrides = Alle Overrides löschen
admin-cancel = Abbrechen
admin-other-price-note = Sampling, Reasoning und Kontext gelten für diese Art nicht — nur Preise, für die Kostenabrechnung.

admin-toml-placeholder-header = # Häufige Schlüssel (vLLM/OpenAI):
admin-toml-defaults-label = Sampling-Standardwerte (TOML)

admin-reasoning-style-label = Reasoning-Stil
admin-reasoning-style-aria = Reasoning-Stil
admin-reasoning-auto = Automatisch
admin-reasoning-none = keins
admin-reasoning-qwen = Qwen (vLLM)
admin-reasoning-openai = OpenAI
admin-reasoning-glm = GLM / z.AI
admin-reasoning-anthropic = Anthropic

admin-effort-standard = Standard
admin-effort-deep = Tief
admin-effort-max = Max
admin-budget-placeholder = Standard
admin-budget-hint = Maximale Denk-Token pro Stufe. Leer = Backend-Standard (unbegrenzt). „Fast“ deaktiviert das Reasoning.
admin-effort-default-option = (Standard)
admin-effort-hint = Reasoning-Aufwand pro Stufe. Leer = eingebauter Standard. „Fast“ deaktiviert das Reasoning.

admin-malformed-form = fehlerhaftes Formular: { $err }
admin-missing-model-name = Feld model_name fehlt
admin-db-delete-error = DB-Löschung: { $err }
admin-invalid-toml = ungültiges TOML: { $err }
admin-db-upsert-error = DB-Upsert: { $err }
admin-saved-model = `{ $model }` gespeichert — sofort wirksam
admin-cleared-defaults = Overrides für `{ $model }` gelöscht
admin-unknown-reasoning-style = unbekannter Reasoning-Stil `{ $style }`
admin-db-error = DB: { $err }
admin-budget-not-positive = Budget `{ $value }` muss eine positive Ganzzahl sein
admin-unknown-reasoning-effort = unbekannter Reasoning-Aufwand `{ $value }`
admin-context-window-invalid = Kontextfenster `{ $value }` muss eine positive Ganzzahl sein

# Preise pro Modell für die Kostenabrechnung (Preis pro 1 Mio. Tokens, Eingabe / Ausgabe).
admin-price-label = { $cur }/1M
admin-price-in-label = Preis ein
admin-price-out-label = Preis aus
admin-price-in-placeholder = kein Preis
admin-price-out-placeholder = kein Preis
admin-price-invalid = Preis `{ $value }` muss eine nicht-negative Zahl sein

# Kontextfenster (steuert die Auto-Kompaktierung).
admin-context-window-full-label = Kontextfenster (Tokens)
admin-context-window-placeholder = Standard

# Aliase (abgeblendete Zeilen).
admin-alias-chip = Alias

# Standardmodelle pro Funktion (im Chat/Sprach-Auswahlmenü vorausgewählt und
# als API-Fallback, wenn ein Aufruf kein Modell angibt).
admin-defaults-heading = Standardmodelle
admin-defaults-intro = Das Modell, das genutzt wird, wenn eine Anfrage keines nennt — die Vorauswahl in den Chat-/Voice-Pickern und der API-Standard (anders als die Fallbacks für unbekannte Modelle auf der Upstreams-Seite, die greifen, wenn eine Anfrage ein Modell nennt, das kein Pool bereitstellt). Leer = das erste verfügbare Modell.
admin-defaults-chat-label = Chat
admin-defaults-voice-label = Sprache (Transkription)
admin-defaults-image-label = Bildgenerierung
admin-defaults-embedding-label = Embedding (RAG)
admin-defaults-first-option = Erstes verfügbares
admin-defaults-saved = Standardmodell auf `{ $model }` gesetzt
admin-defaults-cleared = Standardmodell zurückgesetzt
admin-defaults-unknown-feature = unbekannte Funktion `{ $feature }`

# Modell-Fähigkeiten (tri-state) + Fallback-Modelle.
admin-capabilities-heading = Fähigkeiten
admin-cap-vision = Vision
admin-cap-tools = Tools
admin-cap-structured-output = Strukturierte Ausgabe
admin-cap-audio-input = Audio-Eingabe
admin-cap-pdf-input = PDF-Eingabe
admin-cap-parallel-tools = Parallele Tools
admin-cap-unknown = Unbekannt
admin-cap-enabled = Aktiviert
admin-cap-disabled = Deaktiviert
admin-cap-no-fallback = (keiner)
admin-cap-fallback-vision = Fallback für Vision
admin-cap-fallback-tools = Fallback für Tools

# Upstream-Topologie neu laden ("Apply changes"-Button auf /admin/upstreams).
admin-reloaded = { $pools } Pools, { $backends } Backends neu geladen
admin-reload-error = Neuladen fehlgeschlagen: { $err }
