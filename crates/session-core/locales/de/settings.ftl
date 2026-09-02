# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Betreiber-Einstellungen (/admin/settings). Kartentitel (settings-s-*),
# Feldbeschriftungen (settings-f-*) und deren Hilfetexte (settings-f-*-help)
# werden aus den Spec-Einträgen in gateway_core::server::settings::SECTIONS
# abgeleitet: `sandbox.runner_url` -> `settings-f-sandbox-runner_url`.
# Siehe locales/en/settings.ftl für die Quelle.

settings-heading = Einstellungen
settings-intro = Betriebseinstellungen dieses Gateways. Sie liegen in der Datenbank, eine Konfigurationsdatei ist nicht nötig — jedes Feld zeigt zusätzlich den TOML-Schlüssel, den es ersetzt.
settings-save = Abschnitt speichern
settings-saved = Gespeichert. Ab der nächsten Anfrage aktiv.
settings-saved-restart = Gespeichert. Einige Felder dieses Abschnitts greifen erst nach einem Neustart.
settings-save-failed = Diese Einstellungen konnten nicht gespeichert werden.
settings-cleared = Zurückgesetzt. Es gilt wieder der Standardwert.
settings-restart-badge = Neustart
settings-restart-note = Mit „Neustart“ markierte Felder werden nur beim Start gelesen; Änderungen greifen erst nach einem Neustart.
settings-secret-set = gespeichert — neuen Wert eingeben, um ihn zu ersetzen
settings-secret-unset = nicht gesetzt
settings-secret-clear = Löschen

settings-no-backend-heading = Noch kein Modell-Backend
settings-no-backend-body = Die Anmeldung ist eingerichtet, aber dieses Gateway liefert erst Modelle, wenn ein Backend hinzugefügt ist. Bis dahin lehnen Chat und die /v1-API Anfragen ab.
settings-no-backend-cta = Backend unter /admin/upstreams hinzufügen →

settings-tab-chat = Chat
settings-tab-tools = Werkzeuge
settings-tab-data = Inhalte & Daten
settings-tab-access = Zugriff & Nutzung
settings-tab-notifications = Benachrichtigungen
settings-show-fields = { $count } weitere Einstellungen anzeigen
settings-model-automatic = Automatisch — erstes verfügbares Modell verwenden
settings-model-none-configured = Für diesen Zweck ist noch kein Modell konfiguriert. Legen Sie unter /admin/upstreams einen passenden Pool an, dann erscheint es hier.
settings-model-unavailable = { $model } (konfiguriert, aber derzeit nicht verfügbar)
settings-restart-pending-heading = Neustart ausstehend
settings-restart-pending-body = Diese Einstellungen sind gespeichert, greifen aber erst nach einem Neustart des Gateways:

# ─── Abschnittskarten ────────────────────────────────────────────────────────

settings-s-chat-ocr = Dokumenten-OCR
settings-s-chat-ocr-blurb = Hochgeladene PDFs und Bilder in Text umwandeln, den das Modell lesen kann.
settings-s-chat-compaction = Gesprächsverdichtung
settings-s-chat-compaction-blurb = Die ältere Hälfte eines langen Gesprächs zusammenfassen, damit es weiter in das Kontextfenster des Modells passt.
settings-s-chat-s3 = Anhang-Speicher (S3)
settings-s-chat-s3-blurb = Objektspeicher für Chat-Anhänge. Ohne ihn werden Uploads abgelehnt.
settings-s-sandbox = Code-Sandbox
settings-s-sandbox-blurb = Die isolierte Umgebung, die vom Modell geschriebenen Code ausführt.
settings-s-comfyui = ComfyUI Bild & Video
settings-s-comfyui-blurb = Der Headless-ComfyUI-Worker hinter den Bild- und Video-Werkzeugen.
settings-s-rag = RAG-Indexierung
settings-s-rag-blurb = Wo indexierte Quellen liegen und wie stark der Indexer arbeitet.
settings-s-skills = Skills
settings-s-skills-blurb = Das Bundle-Verzeichnis auf der Platte hinter /admin/skills.
settings-s-typst = Typst-Vorlagen
settings-s-typst-blurb = Vorlagen hinter dem PDF-Export und den Dokument-Werkzeugen.
settings-s-geoip = GeoIP
settings-s-geoip-blurb = Grobe Standortbestimmung des Clients für das Werkzeug get_user_location.
settings-s-usage = Nutzungsmetriken
settings-s-usage-blurb = Abrechnung pro Anfrage hinter /usage.
settings-s-limits = Ratenlimits & Kontingente
settings-s-limits-blurb = Hauptschalter für die Regeln unter /admin/limits.
settings-s-feedback = Feedback-Widget
settings-s-feedback-blurb = Wohin das In-App-Feedback-Widget Issues anlegt.
settings-s-push = Web Push
settings-s-push-blurb = Benachrichtigung, sobald eine Antwort fertig ist. Das Schlüsselpaar wird automatisch erzeugt und gespeichert.
settings-s-gateway = Sitzungen & Tokens
settings-s-gateway-blurb = Wie lange ein Browser-Login und ein API-Token gültig bleiben, und ob Admins sich als andere Nutzer ausgeben dürfen.

# ─── Felder ──────────────────────────────────────────────────────────────────

settings-f-chat-ocr-enabled = OCR aktivieren
settings-f-chat-ocr-enabled-help = Hauptschalter für das Auslesen von Text aus hochgeladenen Dokumenten.
settings-f-chat-ocr-model = OCR-Modell
settings-f-chat-ocr-model-help = Welches Modell die Seiten liest. Es muss von einem Pool der Art ocr bereitgestellt werden; automatisch nutzt das erste verfügbare.
settings-f-chat-ocr-max_tokens = Token-Budget pro Anfrage
settings-f-chat-ocr-max_tokens-help = Token-Budget für eine einzelne OCR-Anfrage.
settings-f-chat-ocr-ngram_window = Überlappungsfenster
settings-f-chat-ocr-ngram_window-help = Überlappung, mit der Seitentexte zusammengefügt werden, ohne Inhalte zu wiederholen.
settings-f-chat-ocr-max_bytes = Maximale Dokumentgröße
settings-f-chat-ocr-max_bytes-help = Größtes akzeptiertes Dokument, in Bytes.
settings-f-chat-ocr-max_pages = Maximale Seitenzahl
settings-f-chat-ocr-max_pages-help = Höchstens so viele Seiten werden aus einem Dokument gelesen.
settings-f-chat-ocr-dpi = Rasterauflösung
settings-f-chat-ocr-dpi-help = Auflösung, mit der PDF-Seiten vor dem Lesen gerendert werden, in DPI.
settings-f-chat-ocr-max_output_chars = Maximaler extrahierter Text
settings-f-chat-ocr-max_output_chars-help = Obergrenze für den aus einem Dokument extrahierten Text, in Zeichen.
settings-f-chat-ocr-timeout_secs = Zeitlimit
settings-f-chat-ocr-timeout_secs-help = Frist für ein Dokument, in Sekunden.
settings-f-chat-ocr-max_concurrency = Seiten parallel
settings-f-chat-ocr-max_concurrency-help = Wie viele Seiten gleichzeitig gelesen werden.
settings-f-chat-ocr-auto_min_text_chars_per_page = Schwelle für Scan-Erkennung
settings-f-chat-ocr-auto_min_text_chars_per_page-help = Unterhalb dieser Zahl eingebetteter Zeichen pro Seite gilt ein PDF als gescannt und geht an die OCR.

settings-f-chat-compaction-enabled = Verdichtung aktivieren
settings-f-chat-compaction-enabled-help = Hauptschalter für das Zusammenfassen langer Gespräche.
settings-f-chat-compaction-default_context_window = Angenommenes Kontextfenster
settings-f-chat-compaction-default_context_window-help = Kontextfenster in Token, das für ein Modell ohne eigene Angabe angenommen wird.
settings-f-chat-compaction-trigger_ratio = Auslöseschwelle
settings-f-chat-compaction-trigger_ratio-help = Anteil des Kontextfensters, der die Verdichtung auslöst (0,7 = bei 70 % Füllung).
settings-f-chat-compaction-keep_recent_turns = Beibehaltene letzte Züge
settings-f-chat-compaction-keep_recent_turns-help = Züge, die am Ende des Gesprächs wörtlich erhalten bleiben.
settings-f-chat-compaction-min_turns_to_compact = Mindestlänge des Gesprächs
settings-f-chat-compaction-min_turns_to_compact-help = Gespräche mit weniger Zügen werden nie verdichtet.
settings-f-chat-compaction-summary_max_tokens = Token-Budget der Zusammenfassung
settings-f-chat-compaction-summary_max_tokens-help = Token-Budget für die Zusammenfassung, die die verdichteten Züge ersetzt.

settings-f-chat-s3-enabled = Anhänge in S3 speichern
settings-f-chat-s3-enabled-help = Ausgeschaltet stehen Chat-Anhänge nicht zur Verfügung.
settings-f-chat-s3-endpoint = Endpunkt-URL
settings-f-chat-s3-endpoint-help = Zum Beispiel https://s3.eu-central-1.amazonaws.com oder eine MinIO-Adresse.
settings-f-chat-s3-region = Region
settings-f-chat-s3-region-help = Name der Region.
settings-f-chat-s3-bucket = Bucket
settings-f-chat-s3-bucket-help = Bucket, in dem die Anhänge liegen.
settings-f-chat-s3-key_prefix = Schlüssel-Prefix
settings-f-chat-s3-key_prefix-help = Prefix, unter dem jeder Objektschlüssel geschrieben wird.
settings-f-chat-s3-access_key = Access-Key-ID
settings-f-chat-s3-access_key-help = Kennung des Zugangsschlüssels für den Bucket.
settings-f-chat-s3-secret_key = Geheimer Access-Key
settings-f-chat-s3-secret_key-help = Geheime Hälfte dieses Zugangsschlüssels. Verschlüsselt gespeichert.

settings-f-sandbox-enabled = Sandbox-Werkzeuge aktivieren
settings-f-sandbox-enabled-help = Registriert die Werkzeuge, mit denen das Modell Code ausführen kann.
settings-f-sandbox-runner_url = Runner-URL
settings-f-sandbox-runner_url-help = Basis-URL des sandbox-runner-Dienstes. Er führt beliebigen Code aus und darf deshalb nur vom Gateway erreichbar sein.
settings-f-sandbox-timeout_secs = Zeitlimit
settings-f-sandbox-timeout_secs-help = HTTP-Frist für einen Lauf, in Sekunden.
settings-f-sandbox-max_artifact_bytes = Maximale Artefaktgröße
settings-f-sandbox-max_artifact_bytes-help = Größte einzelne Datei, die aus einem Lauf zurückgenommen wird, in Bytes.

settings-f-comfyui-enabled = Bild- & Video-Werkzeuge aktivieren
settings-f-comfyui-enabled-help = Registriert die comfyui_*-Werkzeuge.
settings-f-comfyui-base_url = ComfyUI-URL
settings-f-comfyui-base_url-help = Basis-URL der ComfyUI-Instanz. Sie hat keine Authentifizierung und darf deshalb nur vom Gateway erreichbar sein.
settings-f-comfyui-content_dir = Workflow-Verzeichnis
settings-f-comfyui-content_dir-help = Enthält ein Unterverzeichnis pro Workflow. Mit der Reload-Schaltfläche auf /admin/comfyui ohne Neustart neu einlesen.
settings-f-comfyui-timeout_secs = Zeitlimit
settings-f-comfyui-timeout_secs-help = Frist für einen Workflow-Lauf, in Sekunden.
settings-f-comfyui-queue_poll_interval_ms = Abfrageintervall der Warteschlange
settings-f-comfyui-queue_poll_interval_ms-help = Wie oft das Gateway ComfyUI nach einem laufenden Job fragt, in Millisekunden.
settings-f-comfyui-max_concurrent_jobs = Gleichzeitige Jobs
settings-f-comfyui-max_concurrent_jobs-help = Wie viele Workflows das Modell gleichzeitig laufen lassen darf.

settings-f-rag-enabled = Indexer betreiben
settings-f-rag-enabled-help = Hauptschalter für RAG-Indexierung und -Abruf.
settings-f-rag-data_dir = Index-Verzeichnis
settings-f-rag-data_dir-help = Wo Indexe liegen. Muss auf dem persistenten Volume sein, sonst wird bei jedem Neustart neu indexiert. Bestehende Indexe wandern nicht mit — zeigt dies auf einen neuen Ort, wird alles von vorn indexiert.
settings-f-rag-clone_concurrency = Parallele Index-Jobs
settings-f-rag-clone_concurrency-help = Wie viele Git-Clones und Indexierungsjobs gleichzeitig laufen.

settings-f-skills-enabled = Skill-Bundles laden
settings-f-skills-enabled-help = Hauptschalter für die unter /admin/skills verwalteten Skills.
settings-f-skills-dir = Bundle-Verzeichnis
settings-f-skills-dir-help = Verzeichnis, in dem die Skill-Bundles liegen.

settings-f-typst-enabled = Typst-Vorlagen laden
settings-f-typst-enabled-help = Hauptschalter für PDF-Export und die Dokument-Werkzeuge.
settings-f-typst-templates_dir = Vorlagen-Verzeichnis
settings-f-typst-templates_dir-help = Verzeichnis mit den Vorlagen. Wird beim Speichern neu eingelesen, eine neue Vorlage braucht also keinen Neustart.

settings-f-geoip-enabled = GeoIP-Abfragen aktivieren
settings-f-geoip-enabled-help = Hauptschalter für das Werkzeug get_user_location.
settings-f-geoip-db_path = Datenbankdatei
settings-f-geoip-db_path-help = Pfad zur IP2Location-BIN-Datenbank.
settings-f-geoip-update_token = Download-Token
settings-f-geoip-update_token-help = IP2Location-Token zum Aktualisieren der Datenbank. Verschlüsselt gespeichert.

settings-f-usage-enabled = Nutzung aufzeichnen
settings-f-usage-enabled-help = Abrechnung pro Anfrage hinter /usage.
settings-f-usage-retention_days = Aufbewahrung
settings-f-usage-retention_days-help = Wie viele Tage die Datensätze aufbewahrt werden.
settings-f-usage-currency = Währung
settings-f-usage-currency-help = Währung, in der Kosten ausgewiesen werden.

settings-f-limits-enabled = Limits und Kontingente durchsetzen
settings-f-limits-enabled-help = Ausgeschaltet werden die Regeln unter /admin/limits ignoriert.

settings-f-feedback-enabled = Feedback-Widget anbieten
settings-f-feedback-enabled-help = Hauptschalter für die Feedback-Schaltfläche in der App.
settings-f-feedback-github_owner = Repository-Inhaber
settings-f-feedback-github_owner-help = GitHub-Nutzer oder -Organisation, dem der Issue-Tracker gehört.
settings-f-feedback-github_repo = Repository
settings-f-feedback-github_repo-help = Name des Repositories, in dem Issues angelegt werden.
settings-f-feedback-github_token = GitHub-Token
settings-f-feedback-github_token-help = Braucht issues:write, für Screenshots zusätzlich contents:write. Verschlüsselt gespeichert.
settings-f-feedback-github_api_base = API-Basis-URL
settings-f-feedback-github_api_base-help = Basis-URL der REST-API. Für GitHub Enterprise anpassen.
settings-f-feedback-labels = Issue-Labels
settings-f-feedback-labels-help = Labels, die an jedes angelegte Issue vergeben werden.
settings-f-feedback-assets_branch = Screenshot-Branch
settings-f-feedback-assets_branch-help = Verwaister Branch, in den Screenshots committet werden.
settings-f-feedback-extraction_model = Auswertungsmodell
settings-f-feedback-extraction_model-help = Chat-Modell, das aus einer Sprachnotiz die Formularfelder macht.
settings-f-feedback-voice_model = Transkriptionsmodell
settings-f-feedback-voice_model-help = Modell, das die Sprachnotiz in Text umwandelt.

settings-f-push-enabled = Push-Benachrichtigungen senden
settings-f-push-enabled-help = Stellt die Push-Endpunkte bereit und benachrichtigt, sobald eine Antwort fertig ist.
settings-f-push-contact = Betreiber-Kontakt
settings-f-push-contact-help = Eine mailto:- oder https:-URI, über die der Push-Dienst dich erreichen kann.

settings-f-gateway-token_ttl_days = Lebensdauer von API-Tokens
settings-f-gateway-token_ttl_days-help = Wie viele Tage ein neu erzeugtes gwk_…-Token gültig bleibt.
settings-f-gateway-session_ttl_days = Leerlauf-Zeitlimit der Sitzung
settings-f-gateway-session_ttl_days-help = Gleitendes Leerlauf-Limit für ein Browser-Login, in Tagen: jede Anfrage schiebt es weiter, es ist also die Zeit, die jemand wegbleiben darf, bevor er sich neu anmelden muss.
settings-f-gateway-session_absolute_max_days = Maximales Sitzungsalter
settings-f-gateway-session_absolute_max_days-help = Harte Obergrenze in Tagen ab der Anmeldung, die keine Aktivität verlängert. Sie erzwingt außerdem regelmäßig den Weg über den Identity-Provider — der einzige Moment, in dem Gruppen-Claims neu gelesen werden.
settings-f-gateway-allow_impersonation = Identitätsübernahme erlauben
settings-f-gateway-allow_impersonation-help = Erlaubt Admins, zur Fehlersuche als anderer Nutzer zu handeln. Jede Übernahme wird protokolliert und zeigt ein dauerhaftes Banner; ausgeschaltet sind die Schaltflächen verborgen und der Endpunkt lehnt ab.
