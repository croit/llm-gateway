# STATUS: llm-generated, unreviewed — pending native-speaker QA

rag-page-title = RAG-Sammlungen — LLM Gateway
rag-heading = RAG-Sammlungen
rag-description-prefix = Codebasen, die das Gateway indexiert hat. Das Werkzeug
rag-description-suffix = greift auf diese Sammlungen zu, um Fragen zum Code zu beantworten.
rag-collections-heading = Konfigurierte Sammlungen
rag-empty-list = Noch keine Sammlungen. Erstellen Sie oben eine.

# Toasts — collection CRUD
rag-toast-malformed-form = fehlerhaftes Formular: { $err }
rag-toast-name-exists = eine Sammlung namens `{ $name }` existiert bereits
rag-toast-create-failed = Sammlung konnte nicht erstellt werden
rag-toast-indexing-queued = Indexierung von `{ $name }` @ `{ $ref }` wurde eingeplant.
rag-toast-created-aggregate = `{ $name }` (Aggregat) erstellt. Fügen Sie unten Quell-Repos hinzu, um sie zu indexieren.
rag-toast-collection-not-found = Sammlung nicht gefunden
rag-toast-collection-not-found-cap = Sammlung nicht gefunden.
rag-toast-load-collection-failed = Sammlung konnte nicht geladen werden
rag-toast-load-collection-failed-cap = Sammlung konnte nicht geladen werden.
rag-toast-name-length = Name muss 1..=64 Zeichen lang sein.
rag-toast-git-url-required = Git-URL ist erforderlich.
rag-toast-embedding-model-required = Embedding-Modell ist erforderlich.
rag-toast-chunk-size-range = Chunk-Größe muss in (0, 8000] liegen.
rag-toast-chunk-overlap-range = Chunk-Überlappung muss in [0, chunk_size) liegen.
rag-toast-save-failed = Speichern der Sammlung fehlgeschlagen.
rag-toast-vanished = Sammlung ist nach dem Speichern verschwunden.
rag-toast-saved-reload-failed = Gespeichert, aber Neuladen fehlgeschlagen.
rag-toast-saved = `{ $name }` gespeichert.
rag-toast-collection-removed = Sammlung entfernt.
rag-toast-collection-already-gone = Sammlung bereits weg.
rag-toast-delete-failed = Löschen fehlgeschlagen.

# Toasts — refs / sources
rag-toast-reindex-queue-failed = Neuindexierung konnte nicht eingeplant werden
rag-toast-reindex-queued-count = Neuindexierung von { $count } Ref(s) eingeplant.
rag-toast-ref-required = Ref (Branch/Tag/Commit) ist erforderlich.
rag-toast-ref-exists = Ref `{ $ref }` existiert bereits für diese Sammlung
rag-toast-add-ref-failed = Ref konnte nicht hinzugefügt werden
rag-toast-indexing-queued-ref = Indexierung von `{ $ref }` eingeplant.
rag-toast-no-source-urls = Keine Quell-URLs gefunden.
rag-toast-bulk-queued-skipped = { $added } Quelle(n) eingeplant; { $skipped } Duplikat(e) übersprungen.
rag-toast-bulk-queued = Indexierung von { $added } Quelle(n) eingeplant.
rag-toast-ref-not-found = Ref nicht gefunden
rag-toast-reindex-queued-ref = Neuindexierung von `{ $ref }` eingeplant.
rag-toast-set-primary-failed = Primär-Ref konnte nicht gesetzt werden
rag-toast-now-default = `{ $ref }` ist jetzt der Standard-Ref.
rag-toast-delete-ref-failed = Ref konnte nicht gelöscht werden
rag-toast-ref-removed = Ref `{ $ref }` entfernt.
rag-toast-load-log-failed = Protokoll konnte nicht geladen werden
rag-toast-git-url-required-aggregate = Git-URL ist für eine Aggregat-Quelle erforderlich.
rag-toast-update-source-failed = Quelle konnte nicht aktualisiert werden
rag-toast-source-updated = Quelle aktualisiert.

# Status badges
rag-status-pending = ausstehend
rag-status-cloning = klonen
rag-status-indexing = indexieren
rag-status-ready = bereit
rag-status-error = Fehler

# Collection row
rag-pat-set = PAT gesetzt
rag-pat-none = kein PAT
rag-meta-aggregate = { $count } Quelle(n) · { $hint }
rag-meta-versioned = { $url } · { $hint }
rag-badge-aggregate = Aggregat
rag-embed-prefix = Embed:
rag-button-edit = Bearbeiten
rag-button-delete-collection = Sammlung löschen
rag-placeholder-source-git-url = https://github.com/org/repo.git
rag-placeholder-ref-default = Ref (Standard: der Sammlung)
rag-button-add-source = Quelle hinzufügen
rag-placeholder-branch-tag-commit = Branch, Tag oder Commit
rag-button-add-ref = Ref hinzufügen
rag-placeholder-bulk-sources = Massenhinzufügung — ein Repo pro Zeile, optional @ref:
    https://github.com/proxmox/pve-manager.git
    https://github.com/proxmox/qemu-server.git @master
rag-button-add-bulk = Quellen hinzufügen (Masse)

# Ref / source row
rag-badge-primary = primär
rag-ref-indexed-line = indexiert { $date } · { $commit }
rag-never = nie
rag-button-log = Protokoll
rag-button-reindex = Neu indexieren
rag-button-set-primary = Als primär festlegen
rag-button-remove = Entfernen

# Indexing log
rag-log-info = Info
rag-log-warn = Warnung
rag-log-error = Fehler
rag-log-heading = Indexierungsprotokoll
rag-log-empty = Noch keine Indexierungsereignisse aufgezeichnet. Der erste Lauf protokolliert hier, sobald der Indexer diesen Ref aufgreift.

# Inline per-source editor
rag-label-git-url-source = Git-URL (diese Quelle)
rag-label-git-url-inherit = Git-URL (leer = von Sammlung übernehmen)
rag-placeholder-git-url = https://example.com/org/repo.git
rag-label-branch-tag = Branch / Tag
rag-button-save-source = Quelle speichern
rag-button-cancel = Abbrechen

# Create-collection form
rag-create-heading = Neue Sammlung indexieren
rag-create-description = Der Indexer klont das Repo, zerlegt jede Datei in Chunks und embeddet sie mit dem konfigurierten Embedding-Modell. PATs werden im Klartext gespeichert (das Gateway läuft auf vertrauenswürdiger Infrastruktur).
rag-label-name = Name
rag-placeholder-name = z. B. gateway-repo
rag-label-description-optional = Beschreibung (optional)
rag-placeholder-description = kurz, gut lesbar
rag-label-git-url-versioned = Git-URL (nur versioniert)
rag-label-pat-optional = Persönlicher Zugriffstoken (optional)
rag-placeholder-pat = für private Repos
rag-label-include-globs-full = Include-Globs (kommagetrennt oder zeilenweise)
rag-placeholder-include-globs = *.rs, *.md
rag-label-exclude-globs = Exclude-Globs
rag-placeholder-exclude-globs = target/, node_modules/
rag-label-chunk-size = Chunk-Größe
rag-label-chunk-overlap = Chunk-Überlappung
rag-label-allowed-groups = Erlaubte Gruppen
rag-hint-allowed-groups = Kommagetrennte Gateway-Gruppen, die diese Collection auflisten + durchsuchen dürfen. Leer = alle mit den RAG-Tools. Admins haben immer Zugriff.
rag-create-aggregate-help = Aggregat (Multi-Quelle): durchsucht viele Repos als einen Korpus. Lassen Sie die Git-URL leer und fügen Sie nach dem Erstellen jedes Quell-Repo hinzu. Branch / Tag wird zum Standard-Ref für hinzugefügte Quellen.
rag-button-queue-indexing = Indexierung einplanen

# Edit-collection form
rag-edit-heading = Bearbeite { $name }
rag-label-description = Beschreibung
rag-label-pat = Persönlicher Zugriffstoken
rag-badge-pat-set = aktuell gesetzt
rag-badge-pat-none = keiner gespeichert
rag-placeholder-pat-keep = leer lassen, um bestehenden zu behalten
rag-label-clear-pat = Gespeicherten PAT entfernen (nicht mehr authentifizieren)
rag-label-include-globs = Include-Globs
rag-button-save-changes = Änderungen speichern

# Embedding model field
rag-label-embedding-model = Embedding-Modell
rag-placeholder-embedding-model-none = keine Embedding-Pools konfiguriert — Modell-ID eingeben
rag-option-choose-embedding-model = Embedding-Modell wählen…
rag-suffix-not-advertised = (nicht mehr verfügbar)

# Quellenauswahl + Zugangsdaten der Anbieter (rag_source.rs). Die
# Feldbeschriftungen der Anbieter stammen vom Anbieter selbst und werden
# nicht übersetzt.
rag-label-source-kind = Quelle
rag-source-git-label = Git-Repository
rag-source-git-help = Klont ein Repository und indexiert dessen Dateien. Das bisherige Verhalten.
rag-source-secret-stored = gespeichert
rag-source-secret-placeholder = leer lassen, um den gespeicherten Wert zu behalten
rag-source-secret-clear = Gespeicherten Wert löschen
rag-source-unknown-kind = Unbekannte Quellenart.
rag-source-test-button = Verbindung testen
rag-source-test-ok = Verbunden als `{ $account }`. { $entries } Eintrag/Einträge im konfigurierten Ordner.
rag-source-test-ok-plain = Verbunden. { $entries } Eintrag/Einträge im konfigurierten Ordner.
rag-source-test-failed = Quelle nicht erreichbar: { $error }
rag-source-test-git = Wähle eine entfernte Quelle zum Testen. Git-Repositories werden beim Indexieren geprüft.
rag-source-detected = Erkannt: { $server }

rag-label-profile = Dokumentfelder
rag-option-profile-none = Keine — nur Text indexieren
rag-profile-help = Extrahiert Felder (Lieferant, Datum, Betrag, Projekt) aus jedem Dokument, damit sie gefiltert, sortiert und summiert werden können. Kostet einen Modellaufruf pro Dokument; für Code- oder reine Textsammlungen "Keine" lassen.

# Editor für Extraktionsprofile (/rag/profiles, rag_profiles.rs)
rag-profile-page-title = Extraktionsprofile — LLM Gateway
rag-profile-heading = Extraktionsprofile
rag-profile-description = Was aus jedem Dokument einer Sammlung extrahiert wird: die Felder, mit denen "die letzte Rechnung von X" oder "wie viel haben wir ausgegeben" überhaupt beantwortbar werden. Ein Profil wird einer Sammlung auf der RAG-Seite zugewiesen.
rag-profile-create-heading = Neues Profil
rag-profile-list-heading = Profile
rag-profile-empty = Noch keine Profile.
rag-profile-builtin = mitgeliefert
rag-profile-version = v{ $version }
rag-profile-summary = { $count } Feld(er)
rag-profile-label-name = Name
rag-profile-label-description = Beschreibung
rag-profile-label-prompt = Extraktionsanweisungen
rag-profile-label-fields = Felder (JSON)
rag-profile-prompt-placeholder = Beschreibe, was das Modell liest und wie Datums- und Betragsangaben zu normalisieren sind.
rag-profile-fields-help = Ein Objekt pro Feld: key, label, type (text | number | date | enum), description sowie optional filterable / sortable. Ein enum braucht zusätzlich "values". Die Beschreibung sieht das Modell — also präzise formulieren.
rag-profile-edit-warning = Beim Speichern wird die Profilversion erhöht und der Extraktions-Cache verworfen. Sammlungen, die dieses Profil nutzen, müssen neu indexiert werden.
rag-profile-button-create = Profil anlegen
rag-profile-button-save = Speichern
rag-profile-button-delete = Löschen
rag-profile-link = Extraktionsprofile bearbeiten
rag-profile-toast-created = Profil `{ $name }` angelegt.
rag-profile-toast-saved = `{ $name }` gespeichert.
rag-profile-toast-saved-reindex = `{ $name }` gespeichert. Zum Anwenden neu indexieren: { $collections }.
rag-profile-toast-deleted = Profil gelöscht.
rag-profile-toast-name-exists = ein Profil namens `{ $name }` existiert bereits
rag-profile-toast-name-length = Der Name muss 1 bis 64 Zeichen lang sein.
rag-profile-toast-name-charset = Der Name darf nur Buchstaben, Ziffern, `-` und `_` enthalten.
rag-profile-toast-prompt-required = Extraktionsanweisungen sind erforderlich.
rag-profile-toast-fields-invalid = Felder sind kein gültiges JSON: { $err }
rag-profile-toast-fields-empty = Ein Profil braucht mindestens ein Feld.
rag-profile-toast-field-key-required = Jedes Feld braucht einen key.
rag-profile-toast-field-duplicate = Doppelter Feld-key `{ $key }`.
rag-profile-toast-enum-values = Feld `{ $key }` ist ein enum und braucht eine "values"-Liste.
rag-profile-toast-in-use = Wird noch verwendet von: { $collections }. Weise diesen zuerst ein anderes Profil zu.
rag-profile-toast-builtin = Mitgelieferte Profile können nicht gelöscht werden. Bearbeite oder kopiere sie stattdessen.
rag-profile-toast-save-failed = Speichern des Profils fehlgeschlagen.

# Sync-Hook — ein eingehender Auslöser, der eine Sammlung neu synchronisiert.
rag-toast-sync-token = Sync-URL (wird einmalig angezeigt und nicht gespeichert): { $url }
rag-toast-sync-token-cleared = Sync-URL deaktiviert.
rag-button-sync-token = Sync-URL
rag-button-sync-token-rotate = Neue Sync-URL
rag-button-sync-token-clear = Sync-URL deaktivieren
rag-badge-sync-hook = Sync-Hook

# Browser consent for an OAuth source (Google Drive).
rag-source-consent-save-first = Sammlung zuerst mit Client-ID und Secret speichern, dann verbinden, um Zugriff zu erteilen.
rag-source-consent-connected = verbunden
rag-source-consent-not-connected = nicht verbunden
rag-source-consent-connect = Verbinden
rag-source-consent-reconnect = Neu verbinden
rag-source-consent-help = Alle, die diese Sammlung durchsuchen können, sehen die Dateien des verbundenen Kontos.
rag-oauth-lookup-failed = Die Sammlung konnte nicht gelesen werden.
rag-oauth-not-oauth = Diese Quellenart wird nicht im Browser verbunden.
rag-oauth-no-client = Speichern Sie zuerst OAuth-Client-ID und Secret an der Sammlung.
rag-oauth-bad-authorize-url = Die Autorisierungs-URL des Anbieters konnte nicht gebildet werden.
rag-oauth-start-failed = Die Autorisierung konnte nicht gestartet werden.
rag-oauth-callback-missing = In der Antwort des Anbieters fehlten Code oder State.
rag-oauth-expired = Diese Autorisierung ist abgelaufen oder wurde bereits verwendet. Bitte erneut starten.
rag-oauth-provider-refused = Der Anbieter hat die Autorisierung abgelehnt: { $error }
rag-oauth-exchange-failed = Der Tausch des Autorisierungscodes ist fehlgeschlagen: { $error }
rag-oauth-no-refresh-token = Der Anbieter hat kein Refresh-Token geliefert; unbeaufsichtigtes Indexieren wäre damit nicht möglich. Entziehen Sie dem Gateway im Anbieterkonto den Zugriff und verbinden Sie erneut.
rag-oauth-store-failed = Die Zugangsdaten konnten nicht gespeichert werden.
rag-badge-no-files = keine Dateien indexiert
rag-ref-files = { $files } Dateien
