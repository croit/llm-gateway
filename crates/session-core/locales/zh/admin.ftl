# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/admin.rs` — the
# `/admin/models` page for server-wide per-model sampling defaults
# and reasoning-effort/budget overrides.

admin-page-title = 模型默认值 — LLM Gateway
admin-heading = 模型默认值
admin-intro-prefix = 该模型的服务器级默认采样参数，使用 TOML 格式。这些参数适用于
admin-intro-every = 任何
admin-intro-middle = 用户或令牌对该模型发起的请求 —— 除非调用方在自己的请求中设置了相同的键，此时
admin-intro-always-wins = 始终以调用方的值为准
admin-intro-suffix = 。可以把它理解为：当用户未指定自己的值时所获得的最低保障。留空 = 不使用默认值，采用后端的内置行为。
admin-no-models = 尚未发现任何聊天模型。一旦有可用的上游后端，它就会显示在这里。

admin-toml-placeholder-header = # 常用键（vLLM/OpenAI）：
admin-toml-defaults-label = TOML 默认值
admin-save = 保存

admin-reasoning-style-aria = 推理风格
admin-reasoning-auto = 推理：自动
admin-reasoning-none = 推理：无
admin-reasoning-qwen = 推理：Qwen（vLLM）
admin-reasoning-openai = 推理：OpenAI
admin-reasoning-glm = 推理：GLM / z.AI
admin-reasoning-anthropic = 推理：Anthropic

admin-effort-standard = 标准
admin-effort-deep = 深度
admin-effort-max = 最大
admin-budget-placeholder = 默认
admin-budget-hint = 每个强度级别的最大思考 token 数。留空 = 后端默认值（不设上限）。“Fast” 会禁用推理。
admin-effort-default-option = （默认）
admin-effort-hint = 每个强度级别的推理强度。留空 = 内置默认值。“Fast” 会禁用推理。
admin-save-reasoning-budget = 保存推理预算

admin-malformed-form = 表单格式有误：{ $err }
admin-missing-model-name = 缺少 model_name 字段
admin-db-delete-error = 数据库删除失败：{ $err }
admin-cleared-defaults = 已清除 `{ $model }` 的默认值
admin-invalid-toml = TOML 无效：{ $err }
admin-db-upsert-error = 数据库写入失败：{ $err }
admin-saved-defaults = 已保存 `{ $model }` 的默认值
admin-unknown-reasoning-style = 未知的推理风格 `{ $style }`
admin-db-error = 数据库错误：{ $err }
admin-saved-reasoning-style = 已保存 `{ $model }` 的推理风格
admin-budget-not-positive = 预算 `{ $value }` 必须是正整数
admin-unknown-reasoning-effort = 未知的推理强度 `{ $value }`
admin-saved-reasoning-budget = 已保存 `{ $model }` 的推理预算

admin-context-window-label = 上下文
admin-context-window-unit = 词元
admin-context-window-placeholder = 默认
admin-context-window-aria = 上下文窗口（词元）
admin-context-window-invalid = 上下文窗口 `{ $value }` 必须为正整数
admin-context-window-saved = 已为 `{ $model }` 设置上下文窗口
admin-context-window-cleared = 已清除 `{ $model }` 的上下文窗口

# 各模型的成本核算价格（每 100 万 token 的价格，输入 / 输出）。
admin-price-label = 价格（{ $cur }）
admin-price-in-placeholder = 入
admin-price-out-placeholder = 出
admin-price-in-aria = 每 100 万 token 的输入价格
admin-price-out-aria = 每 100 万 token 的输出价格
admin-price-unit = /1M
admin-price-invalid = 价格 `{ $value }` 必须为非负数
admin-price-saved = 已为 `{ $model }` 设置价格

# 各功能的默认模型（在聊天/语音选择器中预先选中，以及调用未指定模型时的 API 回退）。
admin-defaults-heading = 默认模型
admin-defaults-intro = 选择每项功能预先选中的模型。留空 = 第一个可用模型（旧行为）。
admin-defaults-chat-label = 聊天
admin-defaults-voice-label = 语音（转录）
admin-defaults-image-label = 图像生成
admin-defaults-embedding-label = 嵌入（RAG）
admin-defaults-first-option = 第一个可用
admin-defaults-saved = 默认模型已设置为 `{ $model }`
admin-defaults-cleared = 默认模型已清除
admin-defaults-unknown-feature = 未知功能 `{ $feature }`
admin-other-heading = 其他模型（定价）
admin-other-intro = 嵌入、图像、语音和转录模型。采样与推理设置不适用，但可设置每 100 万 tokens 的价格，使其用量计入成本核算与成本限额。

# 别名卡片：作为其他（真实）模型别名的模型名称。
admin-aliases-heading = 别名
admin-aliases-intro = 这些名称是其他模型的别名。它们没有自己的设置或价格——每个请求都会按其解析到的模型进行配置和计费。
admin-alias-chip = 别名

# Model capabilities (vision, tools, structured output) + fallback model refs.
admin-capabilities-heading = Capabilities
admin-cap-unknown = Unknown
admin-cap-enabled = Enabled
admin-cap-disabled = Disabled
admin-cap-structured-output = Structured output
admin-cap-no-fallback = (none)
admin-cap-fallback-vision = Fallback for vision
admin-cap-fallback-tools = Fallback for tools
admin-capabilities-saved = saved capabilities for `{ $model }`
admin-capabilities-error = failed to save capabilities: { $err }

# Upstream topology reload ("Apply changes" button).
admin-reloaded = reloaded { $pools } pools, { $backends } backends
admin-reload-error = reload failed: { $err }
