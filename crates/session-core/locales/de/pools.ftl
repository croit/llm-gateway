# STATUS: llm-generated, unreviewed — pending native-speaker QA

pools-page-title = Upstream-Pools — LLM Gateway
pools-heading = Upstream-Pools
pools-description = Backends nach Art und Auswahlstrategie in Pools gruppieren. Änderungen werden in der Datenbank gespeichert, werden aber erst wirksam, wenn Sie auf „Änderungen anwenden“ klicken.

pools-fallbacks-heading = Fallbacks für unbekannte Modelle
pools-fallbacks-description = Ersatz, wenn eine Anfrage ein Modell benennt, das kein Pool bereitstellt (anders als der Feature-Standard auf der Modelle-Seite, der greift, wenn eine Anfrage gar kein Modell nennt). Leer = der Fehltreffer gibt 404 zurück.

pools-add-heading = Pool hinzufügen
pools-field-name = Name
pools-field-kind = Art
pools-field-strategy = Strategie
pools-field-fallback-offline = Offline-Fallback-Modell
pools-field-fallback-offline-placeholder = wird ausgeliefert, wenn jedes Backend ausgefallen ist
pools-field-models = Bereitgestellte Modelle (Positivliste, kommagetrennt)
pools-field-models-hint = Wenn gesetzt, werden von einem Backend mit /models-Probe nur diese IDs bereitgestellt — der Rest wird durchgestrichen angezeigt. Leer = alles bereitstellen, was das Backend meldet.
pools-field-allowed-groups = Erlaubte Gruppen
pools-field-allowed-groups-hint = Kommagetrennte Gateway-Gruppen, die die Modelle dieses Pools sehen + nutzen dürfen. Leer = alle. Admins haben immer Zugriff. Gruppen unter Admin → Gruppen verwalten.
pools-field-voices = Stimmen (lang=voice pro Zeile)
pools-field-backends = Backends
pools-no-backends = Noch keine Backends definiert. Fügen Sie zuerst eines auf der Seite „Backends“ hinzu.
pools-field-gdpr = GDPR-konform
pools-field-nda = NDA-abgedeckt
pools-field-enforce-limits = Ratenlimits & Kontingente durchsetzen
pools-save-pool = Pool speichern
pools-add-pool = Pool hinzufügen
pools-delete-pool = Löschen

pools-error-name-required = Pool-Name ist erforderlich
pools-error-invalid-kind = ungültige Pool-Art `{ $kind }`
pools-saved = Pool `{ $name }` gespeichert — klicken Sie auf „Änderungen anwenden“, um neu zu laden
pools-deleted = Pool `{ $name }` gelöscht — klicken Sie auf „Änderungen anwenden“, um neu zu laden
pools-fallback-saved = { $kind }-Fallback auf `{ $model }` gesetzt
pools-fallback-cleared = { $kind }-Fallback zurückgesetzt
