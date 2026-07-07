# Strings owned by `gateway/src/rama_server/pages/scheduled.rs` — the
# scheduled-actions management page (builder form + list) and its full-page
# edit sub-page.

scheduled-page-title = Scheduled actions — LLM Gateway
scheduled-edit-page-title = Edit scheduled action — LLM Gateway

scheduled-heading = Scheduled actions
scheduled-intro = Run a prompt automatically on a schedule. Each run opens as a new chat you can read here — pick a model, write the prompt, and choose when it should run.
scheduled-create-submit = Create scheduled action
scheduled-list-heading = Your scheduled actions
scheduled-list-empty = No scheduled actions yet. Create one above.

scheduled-back = Back
scheduled-edit-heading = Edit scheduled action
scheduled-save-submit = Save changes

scheduled-name-label = Name
scheduled-name-placeholder = e.g. Daily news digest
scheduled-model-label = Model
scheduled-model-placeholder = model id (e.g. gpt-4o-mini)
scheduled-gdpr-warning = This model is not GDPR-compliant. Scheduled runs will send your prompt to it automatically — avoid personal data.
scheduled-nda-warning = This model is not covered by a confidentiality agreement. Don't schedule NDA-protected or proprietary material to it.
scheduled-prompt-label = Prompt
scheduled-prompt-placeholder = What should the model do each time it runs?
scheduled-tools-toggle-label = Allow tools (web search, RAG, attachments) — same as in chat
scheduled-reuse-toggle-label = Reuse the previous run's chat — each run continues the same conversation
scheduled-reuse-rounds-prefix = send last
scheduled-reuse-rounds-aria = Rounds of history to replay
scheduled-reuse-rounds-suffix = rounds

scheduled-builder-heading = Schedule
scheduled-mode-hourly = Hourly
scheduled-mode-daily = Daily
scheduled-mode-weekly = Weekly
scheduled-mode-monthly = Monthly
scheduled-mode-advanced = Advanced
scheduled-weekday-mon = Mon
scheduled-weekday-tue = Tue
scheduled-weekday-wed = Wed
scheduled-weekday-thu = Thu
scheduled-weekday-fri = Fri
scheduled-weekday-sat = Sat
scheduled-weekday-sun = Sun
scheduled-on-day-label = On day
scheduled-of-every-month = of every month
scheduled-at-label = At
scheduled-hour-aria = Hour
scheduled-minute-aria = Minute
scheduled-of-every-hour = of every hour
scheduled-timezone-label = Timezone
scheduled-timezone-placeholder = Europe/Berlin
scheduled-cron-label = Cron expression
scheduled-cron-help = Five fields: minute hour day-of-month month day-of-week.

scheduled-no-upcoming-runs = No upcoming runs.
scheduled-next-runs-prefix = Next runs:{ " " }

scheduled-err-pick-weekday = Pick at least one weekday.
scheduled-err-enter-cron = Enter a cron expression.
scheduled-err-unknown-schedule-type = Unknown schedule type `{ $kind }`.
scheduled-field-minute = minute
scheduled-field-hour = hour
scheduled-field-day-of-month = day of month
scheduled-err-enter-field = Enter a { $field }.
scheduled-err-invalid-field = Invalid { $field }: { $value }.
scheduled-err-field-range = { $field } must be { $min }–{ $max }.
scheduled-err-name-length = Name must be 1–128 characters.
scheduled-err-prompt-length = Prompt must be 1–8000 characters.
scheduled-err-pick-model = Pick a model.
scheduled-err-unknown-timezone = Unknown timezone `{ $tz }`.

scheduled-model-non-gdpr = { $model } (non-GDPR)
scheduled-model-nda-restricted = { $model } (confidential-restricted)
scheduled-model-non-gdpr-nda-restricted = { $model } (non-GDPR, confidential-restricted)

scheduled-toast-save-failed = Could not save the schedule.
scheduled-toast-created = Scheduled action created.
scheduled-toast-updated = Schedule updated.
scheduled-toast-not-found = No such scheduled action.
scheduled-toast-update-failed = Could not update the schedule.
scheduled-toast-resumed = Schedule resumed.
scheduled-toast-paused = Schedule paused.
scheduled-toast-refresh-failed = Could not refresh the schedule.
scheduled-toast-deleted = Scheduled action deleted.
scheduled-toast-already-gone = Already gone.
scheduled-toast-delete-failed = Could not delete the schedule.

scheduled-badge-active = active
scheduled-badge-paused = paused
scheduled-status-paused = Paused
scheduled-next-run = Next run: { $when }
scheduled-no-upcoming-run = No upcoming run
scheduled-last-success = Last: ✓ { $when }
scheduled-last-success-open = Last: ✓ { $when } — open
scheduled-last-failure = Last: ✗ { $when }
scheduled-last-failure-open = Last: ✗ { $when } — open
scheduled-pause-title = Pause
scheduled-resume-title = Resume
scheduled-edit-title = Edit
scheduled-delete-title = Delete
