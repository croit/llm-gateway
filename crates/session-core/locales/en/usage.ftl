# Strings owned by `gateway/src/rama_server/pages/usage.rs` — the
# per-user `/usage` usage-statistics page and its admin-only "all
# users" scope toggle.

usage-title-all = Usage — all users — LLM Gateway
usage-title-mine = Your usage — LLM Gateway

usage-heading-all = Usage — all users
usage-heading-mine = Your usage
usage-blurb-all = Per-user and per-backend request volume and token usage across every access method. “Requests” counts upstream backend calls, so a tool-using turn (which makes several round-trips) counts as more than one.
usage-blurb-mine = Your request volume and token usage across the chat UI, the API, and scheduled actions. “Requests” counts upstream backend calls, so a tool-using turn counts as more than one.

usage-metrics-disabled-prefix = Usage metrics are disabled (
usage-metrics-disabled-suffix = ). Figures below reflect only data recorded before it was turned off.

usage-toggle-mine = Mine
usage-toggle-all = All users

usage-source-all = All sources
usage-source-api = API (/v1)
usage-source-chat = Chat UI
usage-source-scheduled = Scheduled
usage-backend-all = All backends

usage-filter-period = Period
usage-filter-source = Source
usage-filter-backend = Backend
usage-apply = Apply

usage-stat-requests-title = Requests
usage-stat-requests-desc = upstream backend calls
usage-stat-tokens-title = Tokens
usage-stat-tokens-desc = prompt + completion
usage-stat-cost-title = Cost
usage-stat-cost-desc = at configured model prices
usage-limits-heading = Your limits
usage-limit-used = { $percent }% used
usage-limit-refreshes = refreshes { $time }
usage-unpriced-warning = Spend excludes unpriced model(s): { $models }. Set prices in /admin/models to count them.
usage-stat-users-title = Users
usage-stat-users-desc = active in range
usage-stat-errors-title = Errors
usage-stat-errors-desc = status ≥ 400

usage-table-by-user = By user
usage-table-by-backend = By backend
usage-table-by-source = By source
usage-table-by-model = By model

usage-key-user = User
usage-key-backend = Backend
usage-key-source = Source
usage-key-model = Model

usage-col-requests = Requests
usage-col-tokens = Tokens
usage-col-cost = Cost
usage-col-errors = Errors

usage-no-activity = No activity in this range.

usage-table-by-token = By API token
usage-key-token = API token
usage-token-none = Chat & scheduled (no token)
usage-token-all = All tokens
usage-filter-token = Token
