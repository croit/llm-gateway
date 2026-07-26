# Strings owned by `gateway/src/rama_server/pages/admin.rs` — the
# `/admin/models` page: default-model pickers plus one filterable list of every
# advertised model, each with a single consolidated editor (pricing, context
# window, reasoning style + budgets/efforts, capabilities, sampling defaults).

admin-page-title = Models — LLM Gateway
admin-heading = Models
admin-intro-prefix = Per-model settings — pricing, context window, reasoning, capabilities and sampling defaults — applied to
admin-intro-every = every
admin-intro-middle = request for this model, from any user or token, unless the caller sets the same value, which
admin-intro-always-wins = always wins
admin-intro-suffix = . Chat models, aliases and other kinds are all in one list.
admin-no-models = No models advertised yet. Once an upstream backend is reachable, it'll appear here.

# List toolbar: text filter + mutually-exclusive kind chips.
admin-filter-placeholder = Filter models…
admin-filter-all = All
admin-filter-chat = chat
admin-filter-other = other kinds
admin-filter-aliases = aliases
admin-filter-configured = configured only

# List column headers.
admin-col-model = Model
admin-col-kind = Kind
admin-col-price = Price in/out
admin-col-context = Context
admin-col-reasoning = Reasoning
admin-col-configured = Configured

# Collapsed-row values.
admin-value-default = default
admin-value-na = n/a
admin-not-configured = not configured
admin-alias-inherits = inherits target settings
admin-reasoning-auto-resolved = Auto → { $style }

# "Configured" facet badges.
admin-badge-price = PRICE
admin-badge-ctx = CTX
admin-badge-budget = BUDGET
admin-badge-caps = CAPS
admin-badge-toml = TOML

# Editor.
admin-save-model = Save model
admin-clear-overrides = Clear all overrides
admin-cancel = Cancel
admin-other-price-note = Sampling, reasoning and context don't apply to this kind — only pricing, for cost accounting.

admin-toml-placeholder-header = # Common keys (vLLM/OpenAI):
admin-toml-defaults-label = Sampling defaults (TOML)

admin-reasoning-style-label = Reasoning style
admin-reasoning-style-aria = Reasoning style
admin-reasoning-auto = Auto
admin-reasoning-none = none
admin-reasoning-qwen = Qwen (vLLM)
admin-reasoning-openai = OpenAI
admin-reasoning-glm = GLM / z.AI
admin-reasoning-anthropic = Anthropic

admin-effort-standard = Standard
admin-effort-deep = Deep
admin-effort-max = Max
admin-budget-placeholder = default
admin-budget-hint = Max thinking tokens per effort level. Blank = backend default (uncapped). Fast disables thinking.
admin-effort-default-option = (default)
admin-effort-hint = Reasoning effort per level. Blank = built-in default. Fast disables thinking.

admin-malformed-form = malformed form: { $err }
admin-missing-model-name = missing model_name field
admin-db-delete-error = db delete: { $err }
admin-invalid-toml = invalid TOML: { $err }
admin-db-upsert-error = db upsert: { $err }
admin-saved-model = saved `{ $model }` — effective immediately
admin-cleared-defaults = cleared overrides for `{ $model }`
admin-unknown-reasoning-style = unknown reasoning style `{ $style }`
admin-db-error = db: { $err }
admin-budget-not-positive = budget `{ $value }` must be a positive integer
admin-unknown-reasoning-effort = unknown reasoning effort `{ $value }`
admin-context-window-invalid = context window `{ $value }` must be a positive integer

# Per-model pricing for cost accounting (price per 1M tokens, input / output).
admin-price-label = { $cur }/{ $unit }
admin-price-unit-tokens = 1M tokens
admin-price-unit-images = image
admin-price-unit-characters = character
admin-price-unit-seconds = second
admin-price-in-label = Price in
admin-price-out-label = Price out
admin-price-in-placeholder = unpriced
admin-price-out-placeholder = unpriced
admin-price-invalid = price `{ $value }` must be a non-negative number

# Context window (drives auto-compaction).
admin-context-window-full-label = Context window (tokens)
admin-context-window-placeholder = default

# Aliases (dimmed rows).
admin-alias-chip = alias

# Per-feature default models (the model pre-selected in the chat/voice
# pickers, and the API fallback when a call omits a model).
admin-defaults-heading = Default models
admin-defaults-intro = The model used when a request names none — the pre-selection in the chat/voice pickers and the API default (unlike the Unknown-model fallbacks on the Upstreams page, which apply when a request names a model no pool serves). Blank = the first available model.
admin-defaults-chat-label = Chat
admin-defaults-voice-label = Voice (transcription)
admin-defaults-image-label = Image generation
admin-defaults-embedding-label = Embedding (RAG)
admin-defaults-first-option = First available
admin-defaults-saved = default model set to `{ $model }`
admin-defaults-cleared = default model cleared
admin-defaults-unknown-feature = unknown feature `{ $feature }`

# Model capabilities (tri-state) + fallback model refs.
admin-capabilities-heading = Capabilities
admin-cap-vision = Vision
admin-cap-tools = Tools
admin-cap-structured-output = Structured output
admin-cap-audio-input = Audio input
admin-cap-pdf-input = PDF input
admin-cap-parallel-tools = Parallel tools
admin-cap-unknown = Unknown
admin-cap-enabled = Enabled
admin-cap-disabled = Disabled
admin-cap-no-fallback = (none)
admin-cap-fallback-vision = Fallback for vision
admin-cap-fallback-tools = Fallback for tools

# Upstream topology reload ("Apply changes" button on /admin/upstreams).
admin-reloaded = reloaded { $pools } pools, { $backends } backends
admin-reload-error = reload failed: { $err }

# Web search backend (`search_web` tool). Formerly the SEARCH_PROVIDER /
# SEARXNG_URL / BRAVE_SEARCH_API_KEY environment variables.
admin-search-heading = Web search
admin-search-intro = Which backend answers the assistant's `search_web` tool. SearXNG needs only a base URL and costs nothing per query if you run your own instance; Brave needs an API key. The key is encrypted at rest.
admin-search-provider-label = Provider
admin-search-provider-searxng = SearXNG (self-hosted)
admin-search-provider-brave = Brave Search API
admin-search-searxng-url-label = SearXNG base URL
admin-search-searxng-url-placeholder = https://searxng.example.com
admin-search-brave-key-label = Brave API key
admin-search-brave-key-placeholder = leave blank to keep the current key
admin-search-brave-key-set = A key is stored (encrypted).
admin-search-brave-key-unset = No key stored.
admin-search-brave-key-clear = Remove the stored key
admin-search-save = Save web search
admin-search-saved = web-search settings saved
admin-search-unknown-provider = unknown search provider `{ $provider }`
