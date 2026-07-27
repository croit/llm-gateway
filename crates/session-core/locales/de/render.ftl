# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `session-core/src/render.rs` — the HTML renderers for
# the chat-style session UI (conversation bubbles, tool-call rows, the
# document canvas, and the composer). Driver-agnostic: both the gateway
# and any future consumer of this crate render through these functions.

render-edit-button = ✎ Bearbeiten
render-edit-confirm = Speichern und neu generieren? Dadurch werden alle Nachrichten darunter gelöscht.
render-edit-save = Speichern & neu generieren
render-edit-cancel = Abbrechen

render-retry-button = ↻ Wiederholen
render-retry-confirm = Diese Antwort neu generieren? Dadurch werden sie und alles darunter gelöscht.

render-attachment-unavailable-title = Dieser Anhang ist nicht mehr verfügbar
render-attachment-unavailable-meta = nicht verfügbar
render-attachment-open-title = { $filename } öffnen · { $mime } · { $size }
render-attachment-title = { $filename } · { $mime } · { $size }
render-attachment-chip-title = { $mime } · { $size }
render-attachment-remove-aria = Anhang entfernen
render-attachment-remove-confirm = { $filename } entfernen? Das kann nicht rückgängig gemacht werden.

# Beschriftung jeder generierten Medienkachel in einer Antwort mit mehreren
# Medien, damit man darauf verweisen kann („mach aus dem 2. Bild ein Video").
render-media-label = { $kind ->
    [image] Bild { $n }
    [video] Video { $n }
    [audio] Audio { $n }
   *[other] Medium { $n }
}

# Kopier-Schaltfläche an einem Codeblock in einer Antwort (nur Symbol,
# daher ist dies der Tooltip bzw. der zugängliche Name).
render-code-copy = Code kopieren
render-code-copied = Kopiert

render-thinking-spinner = Denkt nach…
render-thinking-finalized = { $secs }s nachgedacht
render-thinking-in-progress = Denkt nach… ({ $secs }s)

render-tools-running = Tools laufen
render-tools-errored = Tool-Aufrufe
render-tools-used = Verwendete Tools
render-tools-summary = { $count } Aufrufe · { $breakdown }

render-tool-status-calling = Wird aufgerufen
render-tool-status-used = Verwendet
render-tool-status-error = Tool-Fehler
render-tool-input-label = Eingabe
render-tool-output-label = Ausgabe
render-tool-output-truncated = für die Anzeige gekürzt — alle { $bytes } Bytes sind weiterhin für das Modell verfügbar und in der Datenbank gespeichert; die ersten { $chars } Zeichen werden angezeigt

render-canvas-hand-edited = von dir bearbeitet
render-canvas-edit-button = ✎ Bearbeiten
render-canvas-save = Als neue Version speichern
render-canvas-cancel = Abbrechen
render-canvas-edit-hint = Wird als neue Version gespeichert; der Assistent erfährt von deiner Änderung.
render-canvas-close-title = Schließen
render-canvas-close-aria = Dokumentenansicht schließen
render-canvas-document-aria = Dokument
render-canvas-version-aria = Version

render-composer-attach-aria = Dateien anhängen
render-composer-attach-title = Dateien anhängen (auch per Ablegen/Einfügen)
render-composer-record-aria = Sprachnachricht aufnehmen
render-composer-record-title = Aufnehmen
render-composer-send = Senden
render-composer-stop = Stopp

render-compaction-divider = Frühere Nachrichten zur Kontexteinsparung zusammengefasst
