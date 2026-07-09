# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Admin-Editor für Ratenlimits / Kontingente (/admin/limits).
limits-heading = Ratenlimits & Kontingente
limits-intro = Begrenzen Sie, wie viele Anfragen, wie viele Token oder wie viel Ausgaben ein Aufrufer über ein gleitendes Zeitfenster nutzen darf. Regeln werden von der spezifischsten zur allgemeinsten aufgelöst: Die eigene Regel eines Benutzers gewinnt, sonst die großzügigste seiner Rollen, sonst die globale Vorgabe. Ohne Regeln ist jeder unbegrenzt. Nur abgerechnete Pools zählen (selbst gehostete Pools mit enforce_limits = false sind ausgenommen), und das gesamte Budget eines Benutzers wird über seine API-Tokens, den Chat und geplante Ausführungen hinweg geteilt.
limits-add-heading = Limit hinzufügen oder aktualisieren
limits-field-subject = Gilt für
limits-field-subject-id = Rolle / Benutzer
limits-field-subject-id-ph = Rollen-ID oder Benutzer-E-Mail
limits-field-model = Modell
limits-field-model-ph = alle Modelle
limits-field-dimension = Limit
limits-field-window = Pro
limits-field-value = Wert
limits-add-submit = Limit speichern
limits-subject-global = Alle (Vorgabe)
limits-subject-role = Rolle
limits-subject-user = Benutzer
limits-dim-requests = Anfragen
limits-dim-tokens = Token
limits-dim-cost = Kosten ({ $cur })
limits-dim-cost-short = Kosten
limits-win-hour = Stunde
limits-win-day = Tag
limits-win-week = Woche
limits-win-month = Monat
limits-col-subject = Gilt für
limits-col-scope = Modell
limits-col-limit = Limit
limits-col-window = Zeitfenster
limits-col-value = Wert
limits-col-actions = Aktionen
limits-none = Keine Limits konfiguriert — jeder ist unbegrenzt.
limits-all-models = alle Modelle
limits-delete = Löschen
limits-saved = Limit für { $subject } gespeichert
limits-deleted = Limit entfernt
limits-invalid-value = Wert `{ $value }` muss eine nicht-negative Zahl sein
limits-unknown-role = unbekannte Rolle `{ $role }`
limits-unknown-user = kein Benutzer passt zu `{ $user }`
limits-missing-subject-id = Rollen-ID oder Benutzer-E-Mail eingeben
