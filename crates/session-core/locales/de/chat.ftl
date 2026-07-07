# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/chat/mod.rs` — the
# multi-conversation chat page's server-side handlers: page title
# fallback, sidebar/effort/share/pin toasts, and the SSE-toast error
# messages the composer's fetch layer surfaces on failed actions.

chat-default-title = Chat

chat-toast-conversation-already-gone = Die Unterhaltung war bereits gelöscht.
chat-toast-share-copied = Link kopiert — jeder angemeldete Benutzer mit dem Link kann mitlesen.
chat-toast-share-stopped = Freigabe beendet — der Link funktioniert nicht mehr.
chat-toast-pinned = Angeheftet — diese Unterhaltung bleibt jetzt oben.
chat-toast-unpinned = Nicht mehr angeheftet.
chat-toast-already-in-your-chats = Diese Unterhaltung ist bereits in deinen Chats vorhanden.
chat-toast-effort-set = Denkaufwand: { $level }

chat-mcp-bridged-description = Tools, die über die Integration „{ $name }“ bereitgestellt werden.

chat-error-conversation-not-found = Unterhaltung nicht gefunden.
chat-error-message-not-found = Nachricht nicht gefunden.
chat-error-message-empty = Nachricht darf nicht leer sein
chat-error-message-must-not-be-empty = Nachricht darf nicht leer sein.
chat-error-still-streaming = Für diesen Benutzer läuft noch eine Antwort — bitte warten oder Stopp drücken.
chat-error-retry-assistant-only = Wiederholen gilt nur für Antworten des Assistenten.
chat-error-edit-own-messages-only = Bearbeiten gilt nur für deine eigenen Nachrichten.
chat-error-pdf-export-unavailable = PDF-Export nicht verfügbar: Die Typst-CLI ist auf dem Gateway nicht installiert
chat-error-pdf-export-failed = PDF-Export fehlgeschlagen

chat-error-auth-required = Authentifizierung erforderlich
chat-error-no-such-turn = keine solche Nachricht
chat-error-db-error = Datenbankfehler
chat-error-attachments-not-configured = Chat-Anhänge sind nicht konfiguriert
chat-error-bad-filename = ungültiger Dateiname
chat-error-attachment-not-found = nicht gefunden
