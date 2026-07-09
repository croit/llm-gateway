# Strings owned by `gateway/src/rama_server/pages/webhooks.rs` — the webhooks
# management page (create form + list), its full-page edit sub-page, and the
# one-time trigger-URL reveal.

webhooks-page-title = Webhooks — LLM Gateway
webhooks-edit-page-title = Edit webhook — LLM Gateway

webhooks-heading = Webhooks
webhooks-intro = Run a prompt when an external service calls a URL. You get a secret trigger URL; whatever the caller sends in the request body is appended to your prompt, and the run opens as a new chat you can read here.
webhooks-create-submit = Create webhook
webhooks-save-submit = Save changes
webhooks-edit-heading = Edit webhook
webhooks-back = Back
webhooks-list-heading = Your webhooks
webhooks-list-empty = No webhooks yet. Create one above.

webhooks-name-label = Name
webhooks-name-placeholder = e.g. Deploy digest
webhooks-model-label = Model
webhooks-model-placeholder = Model id
webhooks-prompt-label = Prompt
webhooks-prompt-placeholder = What should the model do with the incoming payload?

webhooks-sync-toggle-label = Wait for the response (return the model's output to the caller)
webhooks-tools-toggle-label = Allow tools (run with your tools, e.g. web search, RAG, connectors)
webhooks-tools-warning = Anyone with the trigger URL can send content that the model processes with your tools, acting as you. Only enable this for a trusted caller.

webhooks-gdpr-warning = This model runs outside the EU. Don't send personal data through this webhook.
webhooks-nda-warning = This model is not cleared for NDA-restricted content. Don't send confidential data through this webhook.
webhooks-model-non-gdpr = { $model } (non-EU)
webhooks-model-nda-restricted = { $model } (NDA-restricted)
webhooks-model-non-gdpr-nda-restricted = { $model } (non-EU, NDA-restricted)

webhooks-reveal-heading = Your trigger URL
webhooks-reveal-note = Copy it now — it's shown only once. Anyone with this URL can fire the webhook. Lost it? Rotate to get a new one.
webhooks-copy = Copy

webhooks-badge-active = Active
webhooks-badge-paused = Paused
webhooks-mode-sync = Waits for response
webhooks-mode-async = Fire-and-forget
webhooks-never-fired = Never fired yet
webhooks-last-success = Last fired { $when }
webhooks-last-success-open = Last fired { $when } — open
webhooks-last-failure = Last fire failed { $when }
webhooks-last-failure-open = Last fire failed { $when } — open

webhooks-pause-title = Pause
webhooks-resume-title = Resume
webhooks-rotate-title = Rotate secret
webhooks-edit-title = Edit
webhooks-delete-title = Delete

webhooks-err-name-length = Name is required and must be 128 characters or fewer.
webhooks-err-prompt-length = Prompt is required and must be 8000 characters or fewer.
webhooks-err-pick-model = Pick a model.

webhooks-toast-created = Webhook created.
webhooks-toast-updated = Webhook updated.
webhooks-toast-paused = Webhook paused.
webhooks-toast-resumed = Webhook resumed.
webhooks-toast-rotated = Secret rotated — the old URL no longer works.
webhooks-toast-deleted = Webhook deleted.
webhooks-toast-already-gone = That webhook was already gone.
webhooks-toast-not-found = Webhook not found.
webhooks-toast-save-failed = Couldn't save the webhook.
webhooks-toast-update-failed = Couldn't update the webhook.
webhooks-toast-delete-failed = Couldn't delete the webhook.
webhooks-toast-refresh-failed = Couldn't refresh the webhook.

# --- Rerun with a different prompt ---
webhooks-rerun-link = rerun
webhooks-rerun-page-title = Rerun webhook — LLM Gateway
webhooks-rerun-heading = Rerun with a different prompt
webhooks-rerun-intro = Replay the most recent payload this webhook received, with a prompt you can edit. The run opens as a new chat.
webhooks-rerun-payload-label = Captured payload (replayed as-is)
webhooks-rerun-submit = Rerun
webhooks-rerun-no-payload = This webhook hasn't captured a payload yet — fire it once first.
webhooks-rerun-no-payload-notice = This webhook hasn't been fired yet, so there's no payload to replay. Fire it once, then come back to rerun it with a different prompt.
webhooks-toast-rerun-started = Rerun complete — opening the conversation…

# --- Run history ---
webhooks-runs-link = runs
webhooks-runs-page-title = Webhook runs — LLM Gateway
webhooks-runs-heading = Runs · { $name }
webhooks-runs-intro = The most recent fires and reruns. Open a run to read its conversation, or rerun its payload with a different prompt.
webhooks-runs-empty = No runs yet. Fire the webhook to see its history here.
webhooks-run-open = open chat
webhooks-run-rerun = rerun
webhooks-run-source-fire = fired
webhooks-run-source-rerun = rerun
webhooks-run-status-ok = ok
webhooks-run-status-error = error
webhooks-run-status-pending = running

# --- Conversation reuse ---
webhooks-reuse-toggle-label = Reuse the conversation (each fire continues the previous fire's chat)
webhooks-reuse-rounds-prefix = replaying the last
webhooks-reuse-rounds-suffix = rounds
webhooks-reuse-rounds-aria = Rounds of history to replay
