# unreviewed
# Strings owned by `gateway/src/rama_server/pages/upstreams.rs` — die
# zusammengeführte Seite `/admin/upstreams` (Pools + Backends).

upstreams-page-title = Upstreams — LLM Gateway
upstreams-heading = Upstreams
upstreams-description = Pools gruppieren Backends nach Art und Auswahlstrategie. Gesundheit, Last und bereitgestellte Modelle werden live geprüft. Topologie-Änderungen werden in der Datenbank gespeichert und mit „Änderungen anwenden“ wirksam.

upstreams-add-pool = Pool
upstreams-add-backend = Backend
upstreams-cancel = Abbrechen
upstreams-edit-pool = Pool bearbeiten
upstreams-edit-backend = Backend bearbeiten
upstreams-delete-confirm = Wirklich löschen?

# Angeheftete Anwenden-Leiste (sichtbar bei nicht angewendeten Änderungen).
upstreams-apply-count = nicht angewendete Änderungen
upstreams-apply-note = — die Laufzeit-Registry liefert noch die vorherige Topologie.

# Compliance-Indikatoren im Pool-Kopf.
upstreams-comp-gdpr = DSGVO
upstreams-comp-nda = NDA
upstreams-comp-limits = Limits

# Ein Backend, das in der DB existiert, aber noch nicht in der Laufzeit-Registry.
upstreams-backend-pending = ausstehend

# Tooltip auf einem durchgestrichenen Modell-Chip: per /models-Probe erkannt,
# aber durch die Modell-Liste (Positivliste) des Pools zurückgehalten.
upstreams-model-withheld-title = Über /models erkannt, aber durch die Modell-Liste dieses Pools zurückgehalten — wird nicht bereitgestellt oder beworben.
# Eingeklapptes Pill nach den bereitgestellten Modellen: Klick zeigt die zurückgehaltenen (inaktiven) Chips.
upstreams-models-inactive-pill = +{ $count } inaktiv
upstreams-models-inactive-hide = ausblenden

upstreams-unassigned-heading = Nicht zugewiesen
upstreams-unassigned-description = Backends, die keinem Pool zugewiesen sind. Weise eines einem Pool zu, um Anfragen dorthin zu leiten.

upstreams-empty = Noch keine Pools oder Backends konfiguriert. Füge einen Pool oder ein Backend hinzu, um zu beginnen.
