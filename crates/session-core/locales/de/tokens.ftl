# STATUS: llm-generated, unreviewed — pending native-speaker QA

tokens-page-title = API-Tokens — LLM Gateway
tokens-page-heading = API-Tokens
tokens-intro = Bearer-Tokens für die OpenAI-kompatible API. Der Klartext wird nur bei der Erstellung angezeigt — bewahren Sie ihn sicher auf.

tokens-create-heading = Token erstellen
tokens-create-description = Erstellen Sie einen neuen Bearer-Token für die OpenAI-kompatible API.
tokens-name-label = Name
tokens-name-placeholder = z. B. laptop, ci-runner
tokens-ttl-label = TTL (Tage)
tokens-create-submit = Token erstellen

tokens-list-heading = Ihre Tokens
tokens-list-empty = Noch keine Tokens vorhanden. Erstellen Sie oben eines.

tokens-badge-revoked = widerrufen
tokens-badge-active = aktiv
tokens-remove-button = Entfernen
tokens-rotate-button = Erneuern
tokens-rotate-title = Ein neues Secret für dieses Token ausstellen (Name und Einstellungen bleiben erhalten)
tokens-revoke-button = Widerrufen

tokens-row-meta = erstellt { $created } · zuletzt verwendet { $last_used } · läuft ab { $expires }
tokens-last-used-never = nie

tokens-tool-use-aria = Werkzeugnutzung
tokens-tool-use-label = Werkzeugnutzung
tokens-tool-use-description = Erlaubt diesem Token, Gateway-Werkzeuge (Websuche, RAG, …) aufzurufen.
tokens-capabilities-summary = Fähigkeiten

tokens-mcp-allow-aria = „Ask“-MCP-Werkzeuge über die API erlauben
tokens-mcp-allow-label = „Ask“-MCP-Werkzeuge über die API erlauben
tokens-mcp-allow-description = Verbindungs-Werkzeuge, die eine Bestätigung erfordern, können über die API nicht nachfragen; die Aktivierung führt sie ohne Rückfrage aus.

tokens-minted-heading = Token erstellt
tokens-minted-copy-warning = Kopieren Sie den Wert jetzt — Sie können ihn danach nicht mehr einsehen.
tokens-copy-aria = Token kopieren
tokens-copy-title = Token kopieren
tokens-minted-name = Name: { $name }

tokens-account-heading = Konto
tokens-signed-in-as = Angemeldet als { $email }
tokens-account-user-id-label = Benutzer-ID
tokens-account-oidc-label = OIDC-Rollen
tokens-account-rbac-label = RBAC-Rollen-IDs
tokens-roles-none = keine
tokens-roles-none-granted = keine vergeben

tokens-malformed-form = ungültiges Formular: { $err }
tokens-name-length = Der Token-Name muss 1..=128 Zeichen lang sein.
tokens-store-failed = Speichern des Tokens fehlgeschlagen.
tokens-created-toast = Token erstellt.

tokens-revoked-not-found = Widerrufenes Token nicht gefunden.
tokens-revoked-toast = Token widerrufen.
tokens-already-revoked = Token wurde bereits widerrufen.
tokens-revoke-failed = Widerruf fehlgeschlagen.

tokens-load-failed = Token konnte nicht geladen werden.
tokens-not-found-or-revoked = Token nicht gefunden oder bereits widerrufen.
tokens-rotated-not-found = Erneuertes Token nicht gefunden.
tokens-rotated-toast = Token erneuert — kopieren Sie den neuen Wert.
tokens-rotate-failed = Erneuern fehlgeschlagen.

tokens-removed-toast = Token entfernt.
tokens-still-active = Token ist noch aktiv — widerrufen Sie es zuerst.
tokens-remove-failed = Entfernen fehlgeschlagen.

tokens-not-found = Token nicht gefunden.
tokens-update-failed = Token konnte nicht aktualisiert werden.
tokens-tool-use-enabled-toast = Werkzeugnutzung für dieses Token aktiviert.
tokens-tool-use-disabled-toast = Werkzeugnutzung für dieses Token deaktiviert.
tokens-mcp-ask-enabled-toast = „Ask“-MCP-Werkzeuge über die API für dieses Token aktiviert.
tokens-mcp-ask-disabled-toast = „Ask“-MCP-Werkzeuge über die API für dieses Token deaktiviert.

tokens-unknown-tool = Unbekanntes Werkzeug.
tokens-save-pref-failed = Einstellung konnte nicht gespeichert werden.
tokens-capability-enabled-toast = { $name } für dieses Token aktiviert.
tokens-capability-disabled-toast = { $name } für dieses Token deaktiviert.

# Web Push "turn complete" opt-in card (rendered by `render_push_card`; wired
# client-side by `ui/ts/push.ts`). Device-local notification settings.
tokens-push-heading = Benachrichtigungen
tokens-push-description = Erhalten Sie auf diesem Gerät eine Benachrichtigung, wenn eine von Ihnen gestartete Antwort fertig ist, während Sie nicht in der App sind.
tokens-push-enable = Auf diesem Gerät aktivieren
tokens-push-disable = Auf diesem Gerät deaktivieren
tokens-push-on = Benachrichtigungen sind für dieses Gerät aktiviert.
tokens-push-off = Benachrichtigungen sind für dieses Gerät deaktiviert.
tokens-push-denied = Dieser Browser hat Benachrichtigungen blockiert. Erlauben Sie sie in den Browsereinstellungen, um sie zu aktivieren.
tokens-push-unsupported = Dieser Browser unterstützt keine Benachrichtigungen.
tokens-push-enabled = Benachrichtigungen auf diesem Gerät aktiviert.
tokens-push-disabled = Benachrichtigungen auf diesem Gerät deaktiviert.
tokens-push-error = Benachrichtigungseinstellungen konnten nicht geändert werden.
