# Strings owned by `gateway/src/rama_server/pages/feedback.rs` — the
# feedback-widget FAB + dialog + confirm-dialog UI chrome, and the
# `/feedback*` JSON API error messages.

feedback-fab-title = Send feedback
feedback-fab-aria = Send feedback

feedback-dialog-heading = Send feedback
feedback-voice-button-title = Tap, describe the issue, tap again — we'll fill the fields below
feedback-voice-button-label = Fill in by voice
feedback-close-aria = Close

feedback-title-label = Title
feedback-title-placeholder = Short summary
feedback-description-label = Description
feedback-description-placeholder = What happened, or what would you like?
feedback-business-label = Business value
feedback-business-placeholder = Why does this matter? Who is impacted?
feedback-acceptance-label = Acceptance criteria
feedback-acceptance-placeholder = When is this done?
feedback-priority-label = Priority
feedback-priority-low = Low
feedback-priority-medium = Medium
feedback-priority-high = High

feedback-shot-label = Screenshot
feedback-shot-status-capturing = Capturing…
feedback-shot-recapture = Recapture
feedback-shot-remove = Remove

feedback-tool-rect-title = Rectangle
feedback-tool-arrow-title = Arrow
feedback-tool-pen-title = Freehand
feedback-tool-text-title = Text
feedback-tool-redact-title = Hide / redact (filled box)
feedback-color-title = Colour
feedback-color-aria = Colour
feedback-undo-title = Undo
feedback-redo-title = Redo
feedback-clear-annot-title = Clear annotations
feedback-clear-annot-label = Clear
feedback-zoom-out-title = Zoom out
feedback-zoom-reset-title = Reset zoom
feedback-zoom-in-title = Zoom in

feedback-log-browser-label = Submit browser activity log (console + network)
feedback-log-chat-label = Submit chat & tool usage log

feedback-cancel-button = Cancel
feedback-submit-button = Send feedback

feedback-confirm-heading = Are you sure?
feedback-confirm-public-p1-prefix = This feedback opens a ticket in our
feedback-confirm-public-p1-strong = public
feedback-confirm-public-p1-suffix = issue tracker. Anyone can read it.
feedback-confirm-private-p2-prefix = Please make sure your screenshot and the submitted data contain
feedback-confirm-private-p2-strong = no personal or private information
feedback-confirm-private-p2-suffix = (names, emails, tokens, customer data, …).
feedback-confirm-cancel-button = No, let me edit
feedback-confirm-ok-button = Yes, send

feedback-err-no-session = No active session
feedback-err-session-lookup-failed = Session lookup failed
feedback-err-body-read = { $error }
feedback-err-empty-transcript = Empty transcript
feedback-err-malformed-json = Malformed JSON: { $error }
feedback-err-no-chat-model = No chat model available for extraction
feedback-err-extraction-failed = Extraction failed: { $error }
feedback-err-not-configured = Feedback is not configured
feedback-err-title-required = Title is required (at least 4 characters)
feedback-err-description-required = Description is required
feedback-err-submit-failed = Could not file the issue — please try again
