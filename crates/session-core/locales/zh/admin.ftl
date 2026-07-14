# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/admin.rs` — `/admin/models` 页面。

admin-page-title = 模型 — LLM Gateway
admin-heading = 模型
admin-intro-prefix = 按模型的设置 —— 定价、上下文窗口、推理、能力和采样默认值 —— 适用于
admin-intro-every = 任何
admin-intro-middle = 用户或令牌对该模型发起的请求，除非调用方设置了相同的值，此时
admin-intro-always-wins = 始终以调用方的值为准
admin-intro-suffix = 。聊天模型、别名和其他类型都在同一个列表中。
admin-no-models = 尚未发现任何模型。一旦有可用的上游后端，它就会显示在这里。

admin-filter-placeholder = 筛选模型…
admin-filter-all = 全部
admin-filter-chat = 聊天
admin-filter-other = 其他类型
admin-filter-aliases = 别名
admin-filter-configured = 仅已配置

admin-col-model = 模型
admin-col-kind = 类型
admin-col-price = 价格 入/出
admin-col-context = 上下文
admin-col-reasoning = 推理
admin-col-configured = 已配置

admin-value-default = 默认
admin-value-na = 不适用
admin-not-configured = 未配置
admin-alias-inherits = 继承目标的设置
admin-reasoning-auto-resolved = 自动 → { $style }

admin-badge-price = 价格
admin-badge-ctx = 上下文
admin-badge-budget = 预算
admin-badge-caps = 能力
admin-badge-toml = TOML

admin-save-model = 保存模型
admin-clear-overrides = 清除所有覆盖
admin-cancel = 取消
admin-other-price-note = 采样、推理和上下文不适用于此类型 —— 仅价格用于成本核算。

admin-toml-placeholder-header = # 常用键（vLLM/OpenAI）：
admin-toml-defaults-label = 采样默认值（TOML）

admin-reasoning-style-label = 推理风格
admin-reasoning-style-aria = 推理风格
admin-reasoning-auto = 自动
admin-reasoning-none = 无
admin-reasoning-qwen = Qwen（vLLM）
admin-reasoning-openai = OpenAI
admin-reasoning-glm = GLM / z.AI
admin-reasoning-anthropic = Anthropic

admin-effort-standard = 标准
admin-effort-deep = 深度
admin-effort-max = 最大
admin-budget-placeholder = 默认
admin-budget-hint = 每个强度级别的最大思考 token 数。留空 = 后端默认值（不设上限）。“Fast” 会禁用推理。
admin-effort-default-option = （默认）
admin-effort-hint = 每个强度级别的推理强度。留空 = 内置默认值。“Fast” 会禁用推理。

admin-malformed-form = 表单格式有误：{ $err }
admin-missing-model-name = 缺少 model_name 字段
admin-db-delete-error = 数据库删除失败：{ $err }
admin-invalid-toml = TOML 无效：{ $err }
admin-db-upsert-error = 数据库写入失败：{ $err }
admin-saved-model = 已保存 `{ $model }` —— 立即生效
admin-cleared-defaults = 已清除 `{ $model }` 的覆盖
admin-unknown-reasoning-style = 未知的推理风格 `{ $style }`
admin-db-error = 数据库错误：{ $err }
admin-budget-not-positive = 预算 `{ $value }` 必须是正整数
admin-unknown-reasoning-effort = 未知的推理强度 `{ $value }`
admin-context-window-invalid = 上下文窗口 `{ $value }` 必须为正整数

# 各模型的成本核算价格（每 100 万 token 的价格，输入 / 输出）。
admin-price-label = { $cur }/1M
admin-price-in-label = 输入价格
admin-price-out-label = 输出价格
admin-price-in-placeholder = 未定价
admin-price-out-placeholder = 未定价
admin-price-invalid = 价格 `{ $value }` 必须为非负数

# 上下文窗口（驱动自动压缩）。
admin-context-window-full-label = 上下文窗口（词元）
admin-context-window-placeholder = 默认

admin-alias-chip = 别名

# 各功能的默认模型。
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

# 模型能力（三态）+ 回退模型。
admin-capabilities-heading = 能力
admin-cap-vision = 视觉
admin-cap-tools = 工具
admin-cap-structured-output = 结构化输出
admin-cap-audio-input = 音频输入
admin-cap-pdf-input = PDF 输入
admin-cap-parallel-tools = 并行工具
admin-cap-unknown = 未知
admin-cap-enabled = 启用
admin-cap-disabled = 禁用
admin-cap-no-fallback = （无）
admin-cap-fallback-vision = 视觉回退
admin-cap-fallback-tools = 工具回退

# 上游拓扑重新加载（/admin/upstreams 上的 “Apply changes” 按钮）。
admin-reloaded = 已重新加载 { $pools } 个 pools、{ $backends } 个 backends
admin-reload-error = 重新加载失败：{ $err }
