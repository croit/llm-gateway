# STATUS: llm-generated, unreviewed — pending native-speaker QA

integrations-page-title = Integrationen — LLM Gateway
integrations-heading = Integrationen
integrations-intro = Verbinden Sie Ihre eigenen Konten, damit der Assistent in Ihrem Namen handeln kann — E-Mails, Kalender, Dateien, Repositorys und mehr lesen. Jede Verbindung nutzt Ihre eigenen Berechtigungen und kann jederzeit getrennt werden.
integrations-empty = Es sind noch keine Konnektoren verfügbar. Ein Administrator kann sie unter Admin → Konnektoren aktivieren.

integrations-badge-connected = Verbunden
integrations-badge-needs-reconnect = Erneute Verbindung nötig
integrations-badge-needs-admin-setup = Admin-Einrichtung nötig

integrations-reconnect-title = Verbindung wiederherstellen (erneute Authentifizierung / Wiederholung)
integrations-reconnect-button = Erneut verbinden
integrations-disconnect-button = Trennen
integrations-disconnect-confirm = Diese Integration trennen? Ihr gespeichertes Zugriffstoken wird gelöscht.
integrations-connect-button = Verbinden

integrations-token-label = Ihr API-Token
integrations-token-placeholder = Token einfügen

integrations-tools-error-prefix = Die Werkzeuge dieses Konnektors konnten nicht geladen werden:
integrations-tools-error-hint = Prüfen Sie die MCP-Server-URL / Ihr Token und verwenden Sie dann oben „Erneut verbinden“.
integrations-tools-empty = Dieser Konnektor stellt keine Werkzeuge bereit.
integrations-tools-header = Werkzeugberechtigungen ({ $count })
integrations-set-all-label = Alle setzen:
integrations-mode-always = Immer
integrations-mode-ask = Nachfragen
integrations-mode-off = Aus
integrations-tools-toggle = Einzelne Werkzeuge anzeigen / ausblenden
integrations-tool-kind-read = lesen
integrations-tool-kind-write = schreiben

integrations-error-unknown-connector = unbekannter oder deaktivierter Konnektor
integrations-error-forbidden-role = Sie haben keinen Zugriff auf diesen Konnektor
integrations-error-not-oauth = dieser Konnektor verwendet kein OAuth
integrations-error-oauth-discovery-failed = OAuth-Discovery fehlgeschlagen: { $error }
integrations-error-needs-setup-no-client = dieser Konnektor benötigt eine Einrichtung: es ist keine Client-ID konfiguriert und der Anbieter bietet keine dynamische Registrierung. Bitten Sie einen Administrator, einen OAuth-Client hinzuzufügen.
integrations-error-sealing-client-secret = Versiegeln des Client-Secrets: { $error }
integrations-error-dcr-failed = dynamische Client-Registrierung fehlgeschlagen: { $error }
integrations-error-needs-setup-admin = dieser Konnektor benötigt eine Einrichtung: ein Administrator muss eine OAuth-Client-ID konfigurieren.
integrations-error-building-authorize-url = Erstellen der Autorisierungs-URL: { $error }
integrations-error-persisting-authorization = Speichern der Autorisierung: { $error }
integrations-error-provider-error = der Anbieter hat einen Fehler gemeldet: { $error } { $desc }
integrations-error-callback-missing = im Callback fehlt Code oder State
integrations-error-auth-expired = diese Autorisierung ist abgelaufen oder wurde bereits verwendet — starten Sie erneut über Integrationen
integrations-error-loading-authorization = Laden der Autorisierung: { $error }
integrations-error-state-mismatch = der Autorisierungsstatus stimmte nicht mit Ihrer Sitzung überein
integrations-error-connector-missing = der Konnektor existiert nicht mehr
integrations-error-decrypting-client-secret = Entschlüsseln des Client-Secrets: { $error }
integrations-error-connector-missing-client-id = dem Konnektor fehlt seine OAuth-Client-ID
integrations-error-sealing-access-token = Versiegeln des Zugriffstokens: { $error }
integrations-error-sealing-refresh-token = Versiegeln des Refresh-Tokens: { $error }
integrations-error-saving-connection = Speichern der Verbindung: { $error }
integrations-error-not-token-based = dieser Konnektor basiert nicht auf einem Token
integrations-error-token-required = ein Token ist erforderlich
integrations-error-sealing-token = Versiegeln des Tokens: { $error }
integrations-error-unknown-connector-plain = unbekannter Konnektor
integrations-error-invalid-mode = ungültiger Berechtigungsmodus
integrations-error-saving-tool-permission = Speichern der Werkzeugberechtigung: { $error }
integrations-error-saving-permissions = Speichern der Berechtigungen: { $error }
integrations-error-listing-tools = Auflisten der Werkzeuge: { $error }
integrations-error-disconnecting = Trennen: { $error }
integrations-error-connection-unavailable = Verbindung nicht verfügbar
