# Strings owned by `gateway/src/rama_server/pages/integrations.rs` — the
# per-user `/integrations` connector store: connect/disconnect flow,
# OAuth error copy, and per-tool permission controls.

integrations-page-title = Integrations — LLM Gateway
integrations-heading = Integrations
integrations-intro = Connect your own accounts so the assistant can act on your behalf — reading your email, calendar, files, repositories, and more. Each connection uses your own permissions and can be disconnected anytime.
integrations-empty = No connectors are available yet. An administrator can enable them under Admin → Connectors.

integrations-badge-connected = Connected
integrations-badge-needs-reconnect = Needs reconnect
integrations-badge-needs-admin-setup = Needs admin setup

integrations-reconnect-title = Re-establish the connection (re-auth / retry)
integrations-reconnect-button = Reconnect
integrations-disconnect-button = Disconnect
integrations-disconnect-confirm = Disconnect this integration? Your stored access token will be deleted.
integrations-connect-button = Connect

integrations-token-label = Your API token
integrations-token-placeholder = paste your token

integrations-tools-error-prefix = Couldn't load this connector's tools:
integrations-tools-error-hint = Check the MCP server URL / your token, then use Reconnect above.
integrations-tools-empty = This connector exposes no tools.
integrations-tools-header = Tool permissions ({ $count })
integrations-set-all-label = Set all:
integrations-mode-always = Always
integrations-mode-ask = Ask
integrations-mode-off = Off
integrations-tools-toggle = Show / hide individual tools
integrations-tool-kind-read = read
integrations-tool-kind-write = write

integrations-error-unknown-connector = unknown or disabled connector
integrations-error-forbidden-role = you don't have access to this connector
integrations-error-not-oauth = this connector does not use OAuth
integrations-error-oauth-discovery-failed = OAuth discovery failed: { $error }
integrations-error-needs-setup-no-client = this connector needs setup: no client id is configured and the provider offers no dynamic registration. Ask an admin to add an OAuth client.
integrations-error-sealing-client-secret = sealing client secret: { $error }
integrations-error-dcr-failed = dynamic client registration failed: { $error }
integrations-error-needs-setup-admin = this connector needs setup: an admin must configure an OAuth client id.
integrations-error-building-authorize-url = building authorize URL: { $error }
integrations-error-persisting-authorization = persisting authorization: { $error }
integrations-error-provider-error = provider returned an error: { $error } { $desc }
integrations-error-callback-missing = callback missing code or state
integrations-error-auth-expired = this authorization has expired or was already used — start again from Integrations
integrations-error-loading-authorization = loading authorization: { $error }
integrations-error-state-mismatch = authorization state did not match your session
integrations-error-connector-missing = the connector no longer exists
integrations-error-decrypting-client-secret = decrypting client secret: { $error }
integrations-error-connector-missing-client-id = connector is missing its OAuth client id
integrations-error-sealing-access-token = sealing access token: { $error }
integrations-error-sealing-refresh-token = sealing refresh token: { $error }
integrations-error-saving-connection = saving connection: { $error }
integrations-error-not-token-based = this connector is not token-based
integrations-error-token-required = a token is required
integrations-error-sealing-token = sealing token: { $error }
integrations-error-unknown-connector-plain = unknown connector
integrations-error-invalid-mode = invalid permission mode
integrations-error-saving-tool-permission = saving tool permission: { $error }
integrations-error-saving-permissions = saving permissions: { $error }
integrations-error-listing-tools = listing tools: { $error }
integrations-error-disconnecting = disconnecting: { $error }
integrations-error-connection-unavailable = connection unavailable
