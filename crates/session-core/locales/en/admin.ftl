# Strings owned by `gateway/src/rama_server/pages/admin.rs` — the
# `/admin/models` page for server-wide per-model sampling defaults
# and reasoning-effort/budget overrides.

admin-page-title = Model defaults — LLM Gateway
admin-heading = Model defaults
admin-intro-prefix = Server-wide default sampling parameters for this model, in TOML. These apply to
admin-intro-every = every
admin-intro-middle = request for this model, from any user or token — unless the caller sets the same key in their own request, which
admin-intro-always-wins = always wins
admin-intro-suffix = . Think of it as the floor everyone gets when they don't specify their own values. Empty = no defaults, the backend's built-in behaviour applies.
admin-no-models = No chat models advertised yet. Once an upstream backend is reachable, it'll appear here.

admin-toml-placeholder-header = # Common keys (vLLM/OpenAI):
admin-toml-defaults-label = TOML defaults
admin-save = Save

admin-reasoning-style-aria = Reasoning style
admin-reasoning-auto = Reasoning: Auto
admin-reasoning-none = Reasoning: none
admin-reasoning-qwen = Reasoning: Qwen (vLLM)
admin-reasoning-openai = Reasoning: OpenAI
admin-reasoning-glm = Reasoning: GLM / z.AI
admin-reasoning-anthropic = Reasoning: Anthropic

admin-effort-standard = Standard
admin-effort-deep = Deep
admin-effort-max = Max
admin-budget-placeholder = default
admin-budget-hint = Max thinking tokens per effort level. Blank = backend default (uncapped). Fast disables thinking.
admin-effort-default-option = (default)
admin-effort-hint = Reasoning effort per level. Blank = built-in default. Fast disables thinking.
admin-save-reasoning-budget = Save reasoning budget

admin-malformed-form = malformed form: { $err }
admin-missing-model-name = missing model_name field
admin-db-delete-error = db delete: { $err }
admin-cleared-defaults = cleared defaults for `{ $model }`
admin-invalid-toml = invalid TOML: { $err }
admin-db-upsert-error = db upsert: { $err }
admin-saved-defaults = saved defaults for `{ $model }`
admin-unknown-reasoning-style = unknown reasoning style `{ $style }`
admin-db-error = db: { $err }
admin-saved-reasoning-style = saved reasoning style for `{ $model }`
admin-budget-not-positive = budget `{ $value }` must be a positive integer
admin-unknown-reasoning-effort = unknown reasoning effort `{ $value }`
admin-saved-reasoning-budget = saved reasoning budget for `{ $model }`

admin-context-window-label = Context
admin-context-window-unit = tok
admin-context-window-placeholder = default
admin-context-window-aria = Context window (tokens)
admin-context-window-invalid = context window `{ $value }` must be a positive integer
admin-context-window-saved = set context window for `{ $model }`
admin-context-window-cleared = cleared context window for `{ $model }`

# Per-model pricing for cost accounting (price per 1M tokens, input / output).
admin-price-label = Price ({ $cur })
admin-price-in-placeholder = in
admin-price-out-placeholder = out
admin-price-in-aria = Input price per 1M tokens
admin-price-out-aria = Output price per 1M tokens
admin-price-unit = /1M
admin-price-invalid = price `{ $value }` must be a non-negative number
admin-price-saved = set prices for `{ $model }`

# Pricing-only card for non-chat models (embedding / image / speech / STT).
admin-other-heading = Other models (pricing)
admin-other-intro = Embedding, image, speech, and transcription models. Sampling and reasoning settings don't apply, but set per-1M-token prices so their usage counts toward cost accounting and cost limits.

# Per-feature default models (the model pre-selected in the chat/voice
# pickers, and the API fallback when a call omits a model).
admin-defaults-heading = Default models
admin-defaults-intro = Pick the model pre-selected for each feature. Blank = the first available model (the previous behaviour).
admin-defaults-chat-label = Chat
admin-defaults-voice-label = Voice (transcription)
admin-defaults-image-label = Image generation
admin-defaults-embedding-label = Embedding (RAG)
admin-defaults-first-option = First available
admin-defaults-saved = default model set to `{ $model }`
admin-defaults-cleared = default model cleared
admin-defaults-unknown-feature = unknown feature `{ $feature }`
