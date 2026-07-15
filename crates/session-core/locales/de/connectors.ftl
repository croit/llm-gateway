# STATUS: llm-generated, unreviewed — pending native-speaker QA

connectors-page-title = Konnektoren — LLM Gateway
connectors-heading = Konnektoren
connectors-restore-defaults-button = Standardwerte wiederherstellen
connectors-catalog-intro = Kuratiere die MCP-Server, mit denen sich Nutzer unter „Integrationen“ verbinden können. Aktiviere einen Konnektor, damit er sichtbar wird. Konnektoren, die keine dynamische Client-Registrierung nutzen können (z. B. Google), benötigen eine Deployment-OAuth-Client-ID/-Secret, bevor sie aktiviert werden können.
connectors-empty-state = Noch keine Konnektoren.

connectors-badge-enabled = Aktiviert
connectors-badge-disabled = Deaktiviert
connectors-badge-default = Standard
connectors-badge-dcr = DCR
connectors-badge-needs-client-id = Client-ID erforderlich
connectors-disable-button = Deaktivieren
connectors-enable-disabled-title = Zuerst unten die OAuth-Client-ID hinzufügen (Bearbeiten → OAuth-Client-ID)
connectors-enable-button = Aktivieren
connectors-delete-confirm = Diesen Konnektor löschen? Er wird für alle Nutzer entfernt, einschließlich ihrer gespeicherten Verbindungen und Tokens. Dies kann nicht rückgängig gemacht werden.
connectors-delete-button = Löschen
connectors-edit-summary = Bearbeiten

connectors-add-summary = Konnektor hinzufügen

connectors-oauth-help-token-1 = Token-Konnektor: Lege oben die MCP-Server-URL fest; jeder Nutzer fügt unter „Integrationen“ sein eigenes API-Token ein (gesendet als
connectors-oauth-help-token-2 = ). Kein OAuth-Client erforderlich.

connectors-oauth-help-dcr-heading = Dynamische Client-Registrierung — kein OAuth-Client erforderlich
connectors-oauth-help-dcr-body = Lege einfach oben die MCP-Server-URL fest. Der Server registriert dieses Gateway automatisch (RFC 7591); jeder Nutzer klickt dann auf „Verbinden“ und autorisiert mit seinem eigenen Konto — eine einzige Anmeldung deckt jeden Dienst ab, den der Server bereitstellt.

connectors-oauth-help-gws-1 = Richte dies auf deinen
connectors-oauth-help-gws-self-hosted = selbst gehosteten Google-Workspace-MCP-Server
connectors-oauth-help-gws-2 = (z. B.
connectors-oauth-help-gws-3 = ) im Streamable-HTTP-Modus — URL endet auf
connectors-oauth-help-gws-4 = . Dieser Server verwaltet den Google-OAuth-Client und nutzt die
connectors-oauth-help-gws-ga-apis = GA-Google-APIs
connectors-oauth-help-gws-5 = (keine Developer Preview). Erlaube die Redirect-URI dieses Gateways auf dem Server über
connectors-oauth-help-gws-footer = Googles gehostete MCP-Endpunkte (gmailmcp/calendarmcp/drivemcp.googleapis.com) werden absichtlich nicht verwendet — sie erfordern die Aufnahme der Organisation in das Workspace Developer Preview Program. Siehe docs/connectors.md für das Deploy-Rezept.

connectors-oauth-help-generic-heading = Einrichten des OAuth-Clients
connectors-oauth-help-generic-intro = Registriere diese exakte Redirect-URI bei deinem OAuth-Client und füge dann dessen Client-ID (und Secret) unten ein:
connectors-oauth-help-google-1 = Google: Erstelle eine
connectors-oauth-help-google-link = OAuth-2.0-Client-ID (Webanwendung)
connectors-oauth-help-google-2 = in der Google Cloud Console, füge die obige Redirect-URI hinzu und aktiviere die Gmail-/Google-Kalender-/Google-Drive-APIs für das Projekt.
connectors-oauth-help-github-1 = GitHub: Erstelle eine
connectors-oauth-help-github-link = OAuth-App
connectors-oauth-help-github-2 = (Einstellungen → Entwicklereinstellungen → OAuth-Apps), setze die Authorization-Callback-URL auf die obige Redirect-URI und kopiere die Client-ID sowie ein neu erzeugtes Client-Secret.
connectors-oauth-help-fallback = Erstelle bei deinem Anbieter einen OAuth-Client mit dieser Redirect-URI und den unten festgelegten Authorize-/Token-URLs.
connectors-oauth-why-1 = Warum ein einmaliger Admin-Schritt? Bei OAuth identifiziert die Client-ID
connectors-term-this-gateway = dieses Gateway
connectors-oauth-why-2 = als App (von allen Nutzern gemeinsam genutzt) — nur das Zugriffstoken unterscheidet sich pro Nutzer. Claude Desktop überspringt das, weil Anthropic vorregistrierte Apps mit fester Redirect-URL ausliefert; ein selbst gehostetes Gateway nutzt seine eigene Redirect-URI (oben), und Google/GitHub unterstützen keine automatische Registrierung (DCR) wie Atlassian — du registrierst also einmal, danach klickt jeder Nutzer nur noch auf „Verbinden“.
connectors-oauth-why-no-app = Gar keine OAuth-App?
connectors-oauth-why-3 = Stelle die Authentifizierung auf „Nutzer-eigenes Token“ um, dann fügt jeder Nutzer sein eigenes Token ein (z. B. ein persönliches GitHub-Zugriffstoken) — die Zugangsdaten kommen dann direkt vom Nutzer, kein Admin-Client nötig.

connectors-field-key-label = Key (stabile ID)
connectors-field-key-placeholder = z. B. gmail
connectors-field-key-readonly-label = Key
connectors-field-name-label = Name
connectors-field-name-placeholder = Anzeigename
connectors-field-icon-label = Symbol (Emoji)
connectors-field-category-label = Kategorie
connectors-field-category-placeholder = Google
connectors-field-description-label = Beschreibung
connectors-field-description-placeholder = Was dieser Konnektor macht
connectors-field-url-label = MCP-Server-URL
connectors-field-auth-label = Authentifizierung
connectors-auth-option-oauth = OAuth 2.1 (jeder Nutzer autorisiert über den Anbieter)
connectors-auth-option-token = Nutzer-eigenes Token (jeder Nutzer fügt sein eigenes API-Token ein)
connectors-auth-option-none = Keine (öffentlicher Server, keine Authentifizierung)
connectors-field-client-json-label = OAuth-Client-JSON einfügen (optional — z. B. Googles „Download JSON“)
connectors-field-client-json-help = Füllt Client-ID/-Secret (sowie Authorize- und Token-URLs) aus der Datei. Oder nutze die einzelnen Felder unten.
connectors-field-client-id-label = OAuth-Client-ID
connectors-field-client-id-placeholder = …apps.googleusercontent.com / GitHub-OAuth-App-ID
connectors-field-client-id-help-1 = Die öffentliche ID, die
connectors-field-client-id-help-2 = als App gegenüber dem Anbieter identifiziert — einmalig von einem Admin auf der OAuth-Credentials-Seite des Anbieters erstellt (Google Cloud → Anmeldedaten, GitHub → OAuth-Apps). Kein nutzerspezifisches Secret. Leer lassen, wenn DCR aktiviert ist.
connectors-field-client-secret-label = OAuth-Client-Secret
connectors-secret-placeholder-existing = •••••••• (leer lassen, um es beizubehalten)
connectors-secret-placeholder-new = Client-Secret (optional)
connectors-field-client-secret-help = Wird zusammen mit der Client-ID auf derselben Seite ausgestellt. Verschlüsselt gespeichert; leer lassen, um das bestehende beizubehalten.
connectors-field-use-dcr-label = Dynamische Client-Registrierung versuchen (RFC 7591)
connectors-field-scopes-label = Scopes (durch Leerzeichen getrennt)
connectors-advanced-summary = Erweitert: Discovery-Overrides
connectors-field-authorize-url-label = Authorize-URL
connectors-field-token-url-label = Token-URL
connectors-field-registration-url-label = Registration-URL
connectors-placeholder-optional-override = optionale Überschreibung
connectors-field-allowed-groups-label = Erlaubte Gruppen (kommagetrennt)
connectors-placeholder-optional = optional
connectors-save-changes-button = Änderungen speichern
connectors-add-connector-button = Konnektor hinzufügen

connectors-error-missing-fields = Key, Name und URL sind erforderlich
connectors-error-bad-client-json = Der client_id konnte nicht aus dem eingefügten JSON gelesen werden — erwartet wurde die Google-OAuth-Client-Datei ({"{"}"web":{"{"}"client_id":…,"client_secret":…{"}"}{"}"}).
connectors-error-sealing-secret = Secret versiegeln: { $error }
connectors-error-saving = Speichern des Konnektors: { $error }
connectors-error-needs-client-id = Dieser Konnektor benötigt eine OAuth-Client-ID, bevor er aktiviert werden kann (er kann keine dynamische Registrierung nutzen). Bearbeite ihn und füge die Client-ID/das Secret hinzu.
connectors-error-toggling = Umschalten des Konnektors: { $error }
connectors-error-deleting = Löschen des Konnektors: { $error }
connectors-error-restoring = Wiederherstellen der Standardwerte: { $error }
