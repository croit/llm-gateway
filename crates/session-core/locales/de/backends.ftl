# STATUS: llm-generated, unreviewed — pending native-speaker QA

backends-page-title = Upstream-Backends — LLM Gateway
backends-heading = Upstream-Backends
backends-description-prefix = Live-Ansicht der konfigurierten Upstream-Pools — Zustand, aktuelle Auslastung im Verhältnis zur Obergrenze jedes Backends und die Modelle, die jedes davon aktuell anbietet. Nur lesend: Das Routing richtet sich ausschließlich danach, was die Backends über ihre
backends-description-suffix = Probe melden.
backends-summary = { $total } Backends · { $healthy } gesund · { $down } ausgefallen
backends-unknown-fallback-prefix = Fallback für unbekanntes Modell —
backends-empty-prefix = Keine Upstream-Pools konfiguriert. Fügen Sie einen
backends-empty-suffix = Block zur gateway.toml hinzu und starten Sie neu.

backends-fallback-offline-title = fallback_offline: wird ausgeliefert, wenn jedes Backend für ein bekanntes Modell in diesem Pool ausgefallen ist
backends-fallback-offline-badge = offline ↩ { $model }
backends-pool-empty = Keine Backends in diesem Pool.

backends-status-down = ausgefallen
backends-status-saturated = ausgelastet
backends-status-up = aktiv

backends-inflight-label = aktiv { $load }
backends-activity-summary = 15m { $m15 } · 30m { $m30 } · 60m { $m60 }
backends-no-models = keine Modelle verfügbar
backends-aliases-label = Aliase:

backends-alias-target-title = Alias → { $target }
backends-alias-disabled-label = { $name } (deaktiviert)
backends-alias-disabled-title = Bare-Alias deaktiviert — dieses Backend bedient mehrere Modelle; geben Sie ihm ein explizites Ziel (Zuordnungsformular)
backends-alias-bare-title = Alias → Modell dieses Backends

# Backend-CRUD-Editor (Backends in der DB-Topologie hinzufügen/bearbeiten/löschen).
backends-manage-heading = Backends verwalten
backends-manage-description = Upstream-Backends hinzufügen, bearbeiten oder entfernen. Änderungen werden in der Datenbank gespeichert, werden aber erst wirksam, wenn Sie auf „Änderungen anwenden“ klicken.
backends-apply-changes = Änderungen anwenden
backends-add-heading = Backend hinzufügen
backends-field-name = Name
backends-field-base-url = Basis-URL
backends-field-api-key-env = API-Schlüssel-Umgebungsvariable
backends-field-health-path = Health-Pfad
backends-field-weight = Gewichtung
backends-field-max-inflight = Max. gleichzeitig
backends-field-models = Modelle (kommagetrennt)
backends-field-aliases = Aliase (name=target pro Zeile)
backends-field-probe-models = Modelle über /models-Probe erkennen
backends-field-supports-edit = Unterstützt Bildbearbeitung
backends-save-backend = Backend speichern
backends-add-backend = Backend hinzufügen
backends-delete-backend = Löschen
backends-error-name-required = Backend-Name ist erforderlich
backends-error-base-url-required = Basis-URL ist erforderlich
backends-saved = Backend `{ $name }` gespeichert — klicken Sie auf „Änderungen anwenden“, um neu zu laden
backends-deleted = Backend `{ $name }` gelöscht — klicken Sie auf „Änderungen anwenden“, um neu zu laden

backends-field-api-key = API-Schlüssel
backends-field-api-key-placeholder = API-Schlüssel (verschlüsselt gespeichert)
backends-field-api-key-keep = leer lassen, um den aktuellen Schlüssel zu behalten
