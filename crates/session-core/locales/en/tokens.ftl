# Strings owned by `gateway/src/rama_server/pages/tokens.rs` — the
# /tokens page: token list, create form, per-token tool/MCP controls,
# minted-secret banner, and the account summary card at the foot of
# the page.

tokens-page-title = API tokens — LLM Gateway
tokens-page-heading = API tokens
tokens-intro = Bearer tokens for the OpenAI-compatible API. The plaintext is shown only at creation time — store it somewhere safe.

tokens-create-heading = Create token
tokens-create-description = Mint a new bearer token for the OpenAI-compatible API.
tokens-name-label = Name
tokens-name-placeholder = e.g. laptop, ci-runner
tokens-ttl-label = TTL (days)
tokens-create-submit = Create token

tokens-list-heading = Your tokens
tokens-list-empty = No tokens yet. Create one above.

tokens-badge-revoked = revoked
tokens-badge-active = active
tokens-remove-button = Remove
tokens-rotate-button = Rotate
tokens-rotate-title = Issue a new secret for this token (keeps its name and settings)
tokens-revoke-button = Revoke

tokens-row-meta = created { $created } · last used { $last_used } · expires { $expires }
tokens-last-used-never = never

tokens-tool-use-aria = Tool use
tokens-tool-use-label = Tool use
tokens-tool-use-description = Let this token call gateway tools (web search, RAG, …).
tokens-capabilities-summary = Capabilities

tokens-mcp-allow-aria = Allow ask-mode MCP tools over API
tokens-mcp-allow-label = Allow “ask” MCP tools over API
tokens-mcp-allow-description = Approval-required connector tools can't prompt over the API; enabling runs them without asking.

tokens-minted-heading = Token created
tokens-minted-copy-warning = Copy the value now — you won't be able to see it again.
tokens-copy-aria = Copy token
tokens-copy-title = Copy token
tokens-minted-name = Name: { $name }

tokens-account-heading = Account
tokens-signed-in-as = Signed in as { $email }
tokens-account-user-id-label = User ID
tokens-account-oidc-label = OIDC roles
tokens-account-rbac-label = RBAC role IDs
tokens-roles-none = none
tokens-roles-none-granted = none granted

tokens-malformed-form = malformed form: { $err }
tokens-name-length = Token name must be 1..=128 characters.
tokens-store-failed = Storing token failed.
tokens-created-toast = Token created.

tokens-revoked-not-found = Revoked token not found.
tokens-revoked-toast = Token revoked.
tokens-already-revoked = Token was already revoked.
tokens-revoke-failed = Revoke failed.

tokens-load-failed = Could not load token.
tokens-not-found-or-revoked = Token not found or already revoked.
tokens-rotated-not-found = Rotated token not found.
tokens-rotated-toast = Token rotated — copy the new value.
tokens-rotate-failed = Rotate failed.

tokens-removed-toast = Token removed.
tokens-still-active = Token is still active — revoke it first.
tokens-remove-failed = Remove failed.

tokens-not-found = Token not found.
tokens-update-failed = Could not update token.
tokens-tool-use-enabled-toast = Tool use enabled for this token.
tokens-tool-use-disabled-toast = Tool use disabled for this token.
tokens-mcp-ask-enabled-toast = Ask-mode MCP tools over API enabled for this token.
tokens-mcp-ask-disabled-toast = Ask-mode MCP tools over API disabled for this token.

tokens-unknown-tool = Unknown tool.
tokens-save-pref-failed = Could not save preference.
tokens-capability-enabled-toast = { $name } enabled for this token.
tokens-capability-disabled-toast = { $name } disabled for this token.

# Web Push "turn complete" opt-in card (rendered by `render_push_card`; wired
# client-side by `ui/ts/push.ts`). Device-local notification settings.
tokens-push-heading = Notifications
tokens-push-description = Get a notification on this device when an assistant turn you started finishes while you're away from the app.
tokens-push-enable = Enable on this device
tokens-push-disable = Disable on this device
tokens-push-on = Notifications are on for this device.
tokens-push-off = Notifications are off for this device.
tokens-push-denied = This browser has blocked notifications. Allow them in your browser settings to enable.
tokens-push-unsupported = This browser doesn't support notifications.
tokens-push-enabled = Notifications enabled on this device.
tokens-push-disabled = Notifications disabled on this device.
tokens-push-error = Could not change notification settings.
