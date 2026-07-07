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
