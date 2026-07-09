# STATUS: llm-generated, unreviewed — pending native-speaker QA

webhooks-page-title = Webhooks — LLM Gateway
webhooks-edit-page-title = Webhook bearbeiten — LLM Gateway

webhooks-heading = Webhooks
webhooks-intro = Führe einen Prompt aus, wenn ein externer Dienst eine URL aufruft. Du erhältst eine geheime Trigger-URL; der Inhalt, den der Aufrufer im Anfrage-Body sendet, wird an deinen Prompt angehängt, und der Lauf öffnet sich als neuer Chat, den du hier lesen kannst.
webhooks-create-submit = Webhook erstellen
webhooks-save-submit = Änderungen speichern
webhooks-edit-heading = Webhook bearbeiten
webhooks-back = Zurück
webhooks-list-heading = Deine Webhooks
webhooks-list-empty = Noch keine Webhooks. Erstelle oben einen.

webhooks-name-label = Name
webhooks-name-placeholder = z. B. Deploy-Zusammenfassung
webhooks-model-label = Modell
webhooks-model-placeholder = Modell-ID
webhooks-prompt-label = Prompt
webhooks-prompt-placeholder = Was soll das Modell mit den eingehenden Daten tun?

webhooks-sync-toggle-label = Auf die Antwort warten (Modellausgabe an den Aufrufer zurückgeben)
webhooks-tools-toggle-label = Tools erlauben (mit deinen Tools ausführen, z. B. Websuche, RAG, Connectors)
webhooks-tools-warning = Jeder mit der Trigger-URL kann Inhalte senden, die das Modell mit deinen Tools und in deinem Namen verarbeitet. Aktiviere dies nur für vertrauenswürdige Aufrufer.

webhooks-gdpr-warning = Dieses Modell läuft außerhalb der EU. Sende keine personenbezogenen Daten über diesen Webhook.
webhooks-nda-warning = Dieses Modell ist nicht für NDA-beschränkte Inhalte freigegeben. Sende keine vertraulichen Daten über diesen Webhook.
webhooks-model-non-gdpr = { $model } (außerhalb der EU)
webhooks-model-nda-restricted = { $model } (NDA-beschränkt)
webhooks-model-non-gdpr-nda-restricted = { $model } (außerhalb der EU, NDA-beschränkt)

webhooks-reveal-heading = Deine Trigger-URL
webhooks-reveal-note = Kopiere sie jetzt — sie wird nur einmal angezeigt. Jeder mit dieser URL kann den Webhook auslösen. Verloren? Erzeuge über „Rotieren" eine neue.
webhooks-copy = Kopieren

webhooks-badge-active = Aktiv
webhooks-badge-paused = Pausiert
webhooks-mode-sync = Wartet auf Antwort
webhooks-mode-async = Feuern und vergessen
webhooks-never-fired = Noch nie ausgelöst
webhooks-last-success = Zuletzt ausgelöst { $when }
webhooks-last-success-open = Zuletzt ausgelöst { $when } — öffnen
webhooks-last-failure = Letzte Auslösung fehlgeschlagen { $when }
webhooks-last-failure-open = Letzte Auslösung fehlgeschlagen { $when } — öffnen

webhooks-pause-title = Pausieren
webhooks-resume-title = Fortsetzen
webhooks-rotate-title = Secret rotieren
webhooks-edit-title = Bearbeiten
webhooks-delete-title = Löschen

webhooks-err-name-length = Name ist erforderlich und darf höchstens 128 Zeichen lang sein.
webhooks-err-prompt-length = Prompt ist erforderlich und darf höchstens 8000 Zeichen lang sein.
webhooks-err-pick-model = Wähle ein Modell.

webhooks-toast-created = Webhook erstellt.
webhooks-toast-updated = Webhook aktualisiert.
webhooks-toast-paused = Webhook pausiert.
webhooks-toast-resumed = Webhook fortgesetzt.
webhooks-toast-rotated = Secret rotiert — die alte URL funktioniert nicht mehr.
webhooks-toast-deleted = Webhook gelöscht.
webhooks-toast-already-gone = Dieser Webhook war bereits entfernt.
webhooks-toast-not-found = Webhook nicht gefunden.
webhooks-toast-save-failed = Webhook konnte nicht gespeichert werden.
webhooks-toast-update-failed = Webhook konnte nicht aktualisiert werden.
webhooks-toast-delete-failed = Webhook konnte nicht gelöscht werden.
webhooks-toast-refresh-failed = Webhook konnte nicht aktualisiert werden.

# --- Mit anderem Prompt erneut ausführen ---
webhooks-rerun-link = erneut ausführen
webhooks-rerun-page-title = Webhook erneut ausführen — LLM Gateway
webhooks-rerun-heading = Mit anderem Prompt erneut ausführen
webhooks-rerun-intro = Spiele die zuletzt empfangene Nutzlast dieses Webhooks erneut ab, mit einem von dir bearbeitbaren Prompt. Der Lauf öffnet sich als neuer Chat.
webhooks-rerun-payload-label = Erfasste Nutzlast (wird unverändert erneut gesendet)
webhooks-rerun-submit = Erneut ausführen
webhooks-rerun-no-payload = Dieser Webhook hat noch keine Nutzlast erfasst — löse ihn zuerst einmal aus.
webhooks-rerun-no-payload-notice = Dieser Webhook wurde noch nicht ausgelöst, daher gibt es keine Nutzlast zum Wiederholen. Löse ihn einmal aus und komme dann zurück, um ihn mit einem anderen Prompt erneut auszuführen.
webhooks-toast-rerun-started = Erneute Ausführung abgeschlossen — Konversation wird geöffnet…

# --- Ausführungshistorie ---
webhooks-runs-link = Ausführungen
webhooks-runs-page-title = Webhook-Ausführungen — LLM Gateway
webhooks-runs-heading = Ausführungen · { $name }
webhooks-runs-intro = Die letzten Auslösungen und erneuten Ausführungen. Öffne eine Ausführung, um ihre Konversation zu lesen, oder führe ihre Nutzlast mit einem anderen Prompt erneut aus.
webhooks-runs-empty = Noch keine Ausführungen. Löse den Webhook aus, um hier die Historie zu sehen.
webhooks-run-open = Chat öffnen
webhooks-run-rerun = erneut ausführen
webhooks-run-source-fire = ausgelöst
webhooks-run-source-rerun = erneut
webhooks-run-status-ok = ok
webhooks-run-status-error = Fehler
webhooks-run-status-pending = läuft

# --- Konversation wiederverwenden ---
webhooks-reuse-toggle-label = Konversation wiederverwenden (jede Auslösung setzt den vorherigen Chat fort)
webhooks-reuse-rounds-prefix = die letzten
webhooks-reuse-rounds-suffix = Runden wiederholen
webhooks-reuse-rounds-aria = Anzahl der wiederzugebenden Verlaufsrunden
