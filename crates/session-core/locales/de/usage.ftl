# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/usage.rs` — the
# per-user `/usage` usage-statistics page and its admin-only "all
# users" scope toggle.

usage-title-all = Nutzung — alle Benutzer — LLM Gateway
usage-title-mine = Deine Nutzung — LLM Gateway

usage-heading-all = Nutzung — alle Benutzer
usage-heading-mine = Deine Nutzung
usage-blurb-all = Anfragevolumen und Token-Nutzung pro Benutzer und pro Backend über alle Zugriffswege hinweg. „Anfragen“ zählt Aufrufe an vorgelagerte Backends, daher zählt ein Turn mit Werkzeugnutzung (der mehrere Umläufe macht) mehr als einen.
usage-blurb-mine = Dein Anfragevolumen und deine Token-Nutzung über Chat-UI, API und geplante Aktionen hinweg. „Anfragen“ zählt Aufrufe an vorgelagerte Backends, daher zählt ein Turn mit Werkzeugnutzung mehr als einen.

usage-metrics-disabled-prefix = Nutzungsmetriken sind deaktiviert (
usage-metrics-disabled-suffix = ). Die unten angezeigten Zahlen umfassen nur Daten, die vor der Deaktivierung erfasst wurden.

usage-toggle-mine = Meine
usage-toggle-all = Alle Benutzer

usage-source-all = Alle Quellen
usage-source-api = API (/v1)
usage-source-chat = Chat-UI
usage-source-scheduled = Geplant
usage-backend-all = Alle Backends

usage-filter-period = Zeitraum
usage-filter-source = Quelle
usage-filter-backend = Backend
usage-apply = Anwenden

usage-stat-requests-title = Anfragen
usage-stat-requests-desc = Aufrufe an vorgelagerte Backends
usage-stat-tokens-title = Token
usage-stat-tokens-desc = Prompt + Vervollständigung
usage-stat-cost-title = Kosten
usage-stat-cost-desc = zu den konfigurierten Modellpreisen
usage-stat-users-title = Benutzer
usage-stat-users-desc = aktiv im Zeitraum
usage-stat-errors-title = Fehler
usage-stat-errors-desc = Status ≥ 400

usage-table-by-user = Nach Benutzer
usage-table-by-backend = Nach Backend
usage-table-by-source = Nach Quelle
usage-table-by-model = Nach Modell

usage-key-user = Benutzer
usage-key-backend = Backend
usage-key-source = Quelle
usage-key-model = Modell

usage-col-requests = Anfragen
usage-col-tokens = Token
usage-col-cost = Kosten
usage-col-errors = Fehler

usage-no-activity = Keine Aktivität in diesem Zeitraum.

usage-limits-heading = Deine Limits
usage-limit-used = { $percent } % genutzt
usage-limit-refreshes = wird { $time } zurückgesetzt
usage-unpriced-warning = Ausgaben schließen nicht bepreiste Modelle aus: { $models }. Preise in /admin/models setzen, um sie zu erfassen.
