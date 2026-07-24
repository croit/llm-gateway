# Strings owned by `session-core/src/render.rs` — the HTML renderers for
# the chat-style session UI (conversation bubbles, tool-call rows, the
# document canvas, and the composer). Driver-agnostic: both the gateway
# and any future consumer of this crate render through these functions.

render-edit-button = ✎ Edit
render-edit-confirm = Save and regenerate? This deletes all messages below.
render-edit-save = Save & regenerate
render-edit-cancel = Cancel

render-retry-button = ↻ Retry
render-retry-confirm = Regenerate this reply? This deletes it and everything below.

render-attachment-unavailable-title = This attachment is no longer available
render-attachment-unavailable-meta = unavailable
render-attachment-open-title = Open { $filename } · { $mime } · { $size }
render-attachment-title = { $filename } · { $mime } · { $size }
render-attachment-chip-title = { $mime } · { $size }
render-attachment-remove-aria = Remove attachment
render-attachment-remove-confirm = Remove { $filename }? This can't be undone.

# Caption on each generated media tile in a multi-media reply, so the
# reader can reference it ("turn the 2nd image into a video"). Numbered
# per media kind within the turn.
render-media-label = { $kind ->
    [image] Image { $n }
    [video] Video { $n }
    [audio] Audio { $n }
   *[other] Media { $n }
}

render-thinking-spinner = Thinking…
render-thinking-finalized = Thought for { $secs }s
render-thinking-in-progress = Thinking… ({ $secs }s)

render-tools-running = Running tools
render-tools-errored = Tool calls
render-tools-used = Used tools
render-tools-summary = { $count } calls · { $breakdown }

render-tool-status-calling = Calling
render-tool-status-used = Used
render-tool-status-error = Tool error
render-tool-input-label = Input
render-tool-output-label = Output
render-tool-output-truncated = truncated for display — full { $bytes } bytes still available to the model + persisted in the DB; displaying first { $chars } chars

render-canvas-close-title = Close
render-canvas-close-aria = Close document canvas
render-canvas-document-aria = Document
render-canvas-version-aria = Version

render-composer-attach-aria = Attach files
render-composer-attach-title = Attach files (also drop / paste)
render-composer-record-aria = Record voice message
render-composer-record-title = Record
render-composer-send = Send
render-composer-stop = Stop

render-compaction-divider = Earlier messages condensed to save context
