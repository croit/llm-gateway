# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/chat/render.rs` — the
# gateway-only chat-page chrome: the header model/voice pickers, the
# compliance banners, the composer's "+" tools/integrations/skills menu,
# the "Denken" (effort/thinking) picker, and the share/export/fork
# controls. Prefixed `chat-render-` (rather than `chat-`) to avoid
# colliding with `chat/mod.rs`'s own `chat-*` keys in the sibling
# `chat.ftl`.

chat-render-canvas-toggle-title = Dokumentenansicht ein-/ausblenden
chat-render-canvas-toggle-label = Canvas
chat-render-canvas-document-tab = Dokument
chat-render-canvas-assets-tab = Dateien
chat-render-canvas-assets-heading = Dateien dieser Unterhaltung
chat-render-canvas-assets-count = { $count ->
    [one] { $count } Datei
   *[other] { $count } Dateien
}
chat-render-canvas-assets-empty = Dieser Unterhaltung wurden noch keine Dateien hinzugefügt.
chat-render-canvas-asset-download = Datei herunterladen
chat-render-canvas-close-title = Canvas schließen

chat-render-model-placeholder = Modell (z. B. gpt-4o-mini)
chat-render-model-aria = Chat-Modell
chat-render-voice-model-aria = Sprachmodell
chat-render-tts-voice-aria = Stimme der Sprachausgabe
chat-render-tts-voice-default = Standardstimme

chat-render-model-non-gdpr = { $id } (nicht DSGVO-konform)
chat-render-model-confidential = { $id } (vertraulichkeitsbeschränkt)
chat-render-model-non-gdpr-confidential = { $id } (nicht DSGVO-konform, vertraulichkeitsbeschränkt)

chat-render-gdpr-banner = Du sendest Daten an ein nicht DSGVO-konformes Modell. Gib keine personenbezogenen Daten ein (Namen, E-Mail-Adressen, Adressen, Kunden- oder Mitarbeiterdaten).
chat-render-nda-banner = Dieses Modell ist nicht durch eine Vertraulichkeitsvereinbarung abgedeckt. Sende keine NDA-geschützten oder vertraulichen Inhalte.

chat-render-shared-readonly-banner = Freigegebener Chat — nur lesbar. Nur der Ersteller kann antworten.
chat-render-composer-placeholder = Nachricht an das Modell…

chat-render-new-conversation-fallback = Neue Unterhaltung

chat-render-feedback-title = Feedback senden

chat-render-effort-title = Denkaufwand
chat-render-effort-tooltip = Denkaufwand: höher = mehr Reasoning und mehr Tool-Runden, aber langsamer
chat-render-effort-label-prefix = Denkaufwand:
chat-render-effort-fast = Schnell
chat-render-effort-standard = Standard
chat-render-effort-deep = Tief
chat-render-effort-max = Maximal

chat-render-tools-tooltip = Tools, Integrationen & Skills für diese Unterhaltung
chat-render-tools-label = Tools
chat-render-tools-search-placeholder = Tools durchsuchen…
chat-render-all-tools-label = Alle Tools
chat-render-no-tools-prefix = Für dein Konto sind noch keine Tools verfügbar. Verbinde eine Integration unter
chat-render-no-tools-suffix = .

chat-render-close = Schließen

chat-render-group-web-network = Web & Netzwerk
chat-render-group-attachments-documents = Anhänge & Dokumente
chat-render-group-document-templates = Dokumentvorlagen
chat-render-group-knowledge-base = Wissensdatenbank
chat-render-group-code-sandbox = Code & Sandbox
chat-render-group-memory = Speicher
chat-render-group-integrations = Integrationen
chat-render-group-utility = Dienstprogramme
chat-render-group-skills = Skills

chat-render-tool-count = { $count ->
    [one] { $count } Tool
   *[other] { $count } Tools
}

chat-render-active-count-title = Aktive Tools — zum Verwalten tippen
chat-render-unpin-title = Loslösen (zurück auf Automatisch)

chat-render-state-off-tip = Aus — blockiert; für den Assistenten unsichtbar
chat-render-state-auto-tip = Automatisch — der Assistent aktiviert es bei Bedarf selbst
chat-render-state-on-tip = An — für den Assistenten immer verfügbar

chat-render-share-label-on = Freigegeben ✓
chat-render-share-label-off = Freigeben
chat-render-share-tooltip = Freigegebene Chats können von jedem angemeldeten Benutzer mit dem Link gelesen werden

chat-render-fork-tooltip = Diese Unterhaltung in deine eigenen Chats kopieren, um weiterzuchatten
chat-render-fork-label = In meinen Chats fortsetzen

chat-render-export-tooltip = Diese Unterhaltung herunterladen
chat-render-export-aria = Unterhaltung exportieren
chat-render-export-label = Exportieren
chat-render-export-pdf = PDF-Dokument
chat-render-export-md = Markdown (.md)
