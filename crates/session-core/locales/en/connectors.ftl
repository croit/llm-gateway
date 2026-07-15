# Strings owned by `gateway/src/rama_server/pages/connectors.rs` — the
# admin `/admin/connectors` page that curates the MCP connector catalog
# the per-user `/integrations` store draws from.
#
# A handful of keys are split into short fragments (e.g.
# `connectors-oauth-help-gws-1` / `-2` / `-3` …) because the source HTML
# splices plain text around inline `<strong>` / `<code>` / `<a>` tags.
# Fluent trims leading/trailing whitespace from a message value, so the
# glue space between a fragment and its neighbouring tag is a literal
# `" "` in the Rust call site, not part of the translated string —
# translators should not add leading/trailing spaces to these values.

connectors-page-title = Connectors — LLM Gateway
connectors-heading = Connectors
connectors-restore-defaults-button = Restore defaults
connectors-catalog-intro = Curate the MCP servers users can connect under Integrations. Enable a connector to make it visible. Connectors that can't use dynamic client registration (e.g. Google) need a deployment OAuth client id/secret before they can be enabled.
connectors-empty-state = No connectors yet.

connectors-badge-enabled = Enabled
connectors-badge-disabled = Disabled
connectors-badge-default = Default
connectors-badge-dcr = DCR
connectors-badge-needs-client-id = Needs client id
connectors-disable-button = Disable
connectors-enable-disabled-title = Add the OAuth client id below first (Edit → OAuth client id)
connectors-enable-button = Enable
connectors-delete-confirm = Delete this connector? It is removed for all users, along with their stored connections and tokens. This cannot be undone.
connectors-delete-button = Delete
connectors-edit-summary = Edit

connectors-add-summary = Add a connector

connectors-oauth-help-token-1 = Token connector: set the MCP server URL above; each user pastes their own API token under Integrations (sent as
connectors-oauth-help-token-2 = ). No OAuth client needed.

connectors-oauth-help-dcr-heading = Dynamic Client Registration — no OAuth client needed
connectors-oauth-help-dcr-body = Just set the MCP server URL above. The server registers this gateway automatically (RFC 7591); each user then clicks Connect and authorizes with their own account — one sign-in covers every service the server exposes.

connectors-oauth-help-gws-1 = Point this at your
connectors-oauth-help-gws-self-hosted = self-hosted Google Workspace MCP server
connectors-oauth-help-gws-2 = (e.g.
connectors-oauth-help-gws-3 = ) running in streamable-HTTP mode — URL ends in
connectors-oauth-help-gws-4 = . That server holds the Google OAuth client and uses the
connectors-oauth-help-gws-ga-apis = GA Google APIs
connectors-oauth-help-gws-5 = (no developer preview). Allow this gateway's redirect URI on the server via
connectors-oauth-help-gws-footer = Google's hosted MCP endpoints (gmailmcp/calendarmcp/drivemcp.googleapis.com) are intentionally not used — they require enrolling the org in the Workspace Developer Preview Program. See docs/connectors.md for the deploy recipe.

connectors-oauth-help-generic-heading = Setting up the OAuth client
connectors-oauth-help-generic-intro = Register this exact redirect URI with your OAuth client, then paste its client id (and secret) below:
connectors-oauth-help-google-1 = Google: create an
connectors-oauth-help-google-link = OAuth 2.0 Client ID (Web application)
connectors-oauth-help-google-2 = in Google Cloud Console, add the redirect URI above, and enable the Gmail / Google Calendar / Google Drive APIs for the project.
connectors-oauth-help-github-1 = GitHub: create an
connectors-oauth-help-github-link = OAuth App
connectors-oauth-help-github-2 = (Settings → Developer settings → OAuth Apps), set the Authorization callback URL to the redirect URI above, and copy the Client ID + a generated client secret.
connectors-oauth-help-fallback = Create an OAuth client at your provider with this redirect URI and the authorize / token URLs set below.
connectors-oauth-why-1 = Why a one-time admin step? In OAuth the client id identifies
connectors-term-this-gateway = this gateway
connectors-oauth-why-2 = as an app (shared by all users) — only the per-user access token differs. Claude Desktop skips it because Anthropic ships pre-registered apps tied to its fixed redirect URL; a self-hosted gateway uses its own redirect URI (above), and Google/GitHub don't support automatic registration (DCR) the way Atlassian does — so you register once, then every user just clicks Connect.
connectors-oauth-why-no-app = No OAuth app at all?
connectors-oauth-why-3 = Switch Authentication to “User-supplied token” and each user pastes their own token (e.g. a GitHub Personal Access Token) — credentials then come straight from the user, no admin client.

connectors-field-key-label = Key (stable id)
connectors-field-key-placeholder = e.g. gmail
connectors-field-key-readonly-label = Key
connectors-field-name-label = Name
connectors-field-name-placeholder = Display name
connectors-field-icon-label = Icon (emoji)
connectors-field-category-label = Category
connectors-field-category-placeholder = Google
connectors-field-description-label = Description
connectors-field-description-placeholder = What this connector does
connectors-field-url-label = MCP server URL
connectors-field-auth-label = Authentication
connectors-auth-option-oauth = OAuth 2.1 (each user authorizes via the provider)
connectors-auth-option-token = User-supplied token (each user pastes their own API token)
connectors-auth-option-none = None (public server, no authentication)
connectors-field-client-json-label = Paste OAuth client JSON (optional — e.g. Google’s “Download JSON”)
connectors-field-client-json-help = Fills client id / secret (and authorize + token URLs) from the file. Or use the individual fields below.
connectors-field-client-id-label = OAuth client id
connectors-field-client-id-placeholder = …apps.googleusercontent.com / GitHub OAuth App id
connectors-field-client-id-help-1 = The public id that identifies
connectors-field-client-id-help-2 = as an app to the provider — created once by an admin on the provider’s OAuth credentials page (Google Cloud → Credentials, GitHub → OAuth Apps). Not a per-user secret. Leave blank if DCR is enabled.
connectors-field-client-secret-label = OAuth client secret
connectors-secret-placeholder-existing = •••••••• (leave blank to keep)
connectors-secret-placeholder-new = client secret (optional)
connectors-field-client-secret-help = Issued alongside the client id on the same page. Stored encrypted; leave blank to keep the existing one.
connectors-field-use-dcr-label = Try dynamic client registration (RFC 7591)
connectors-field-scopes-label = Scopes (space-separated)
connectors-advanced-summary = Advanced: discovery overrides
connectors-field-authorize-url-label = Authorize URL
connectors-field-token-url-label = Token URL
connectors-field-registration-url-label = Registration URL
connectors-placeholder-optional-override = optional override
connectors-field-allowed-groups-label = Allowed groups (comma-separated)
connectors-placeholder-optional = optional
connectors-save-changes-button = Save changes
connectors-add-connector-button = Add connector

connectors-error-missing-fields = key, name and URL are required
connectors-error-bad-client-json = couldn't read a client_id from the pasted JSON — expected the Google OAuth client file ({"{"}"web":{"{"}"client_id":…,"client_secret":…{"}"}{"}"}).
connectors-error-sealing-secret = sealing secret: { $error }
connectors-error-saving = saving connector: { $error }
connectors-error-needs-client-id = this connector needs an OAuth client id before it can be enabled (it can't use dynamic registration). Edit it and add the client id/secret.
connectors-error-toggling = toggling connector: { $error }
connectors-error-deleting = deleting connector: { $error }
connectors-error-restoring = restoring defaults: { $error }
