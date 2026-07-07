# STATUS: llm-generated, unreviewed — pending native-speaker QA

tokens-page-title = API 令牌 — LLM Gateway
tokens-page-heading = API 令牌
tokens-intro = 用于兼容 OpenAI 的 API 的 Bearer 令牌。明文仅在创建时显示一次——请妥善保存。

tokens-create-heading = 创建令牌
tokens-create-description = 为兼容 OpenAI 的 API 生成一个新的 Bearer 令牌。
tokens-name-label = 名称
tokens-name-placeholder = 例如 laptop、ci-runner
tokens-ttl-label = 有效期（天）
tokens-create-submit = 创建令牌

tokens-list-heading = 您的令牌
tokens-list-empty = 暂无令牌。请在上方创建一个。

tokens-badge-revoked = 已吊销
tokens-badge-active = 生效中
tokens-remove-button = 移除
tokens-rotate-button = 轮换
tokens-rotate-title = 为此令牌签发新密钥（保留其名称和设置）
tokens-revoke-button = 吊销

tokens-row-meta = 创建于 { $created } · 最近使用 { $last_used } · 过期于 { $expires }
tokens-last-used-never = 从未使用

tokens-tool-use-aria = 工具使用
tokens-tool-use-label = 工具使用
tokens-tool-use-description = 允许此令牌调用网关工具（网络搜索、RAG 等）。
tokens-capabilities-summary = 能力

tokens-mcp-allow-aria = 允许通过 API 使用“ask”模式的 MCP 工具
tokens-mcp-allow-label = 允许通过 API 使用“ask”模式的 MCP 工具
tokens-mcp-allow-description = 需要批准的连接器工具无法通过 API 请求确认；启用后将不经询问直接运行它们。

tokens-minted-heading = 令牌已创建
tokens-minted-copy-warning = 请立即复制该值——之后将无法再次查看。
tokens-copy-aria = 复制令牌
tokens-copy-title = 复制令牌
tokens-minted-name = 名称：{ $name }

tokens-account-heading = 账户
tokens-signed-in-as = 已登录为 { $email }
tokens-account-user-id-label = 用户 ID
tokens-account-oidc-label = OIDC 角色
tokens-account-rbac-label = RBAC 角色 ID
tokens-roles-none = 无
tokens-roles-none-granted = 未授予任何角色

tokens-malformed-form = 表单格式错误：{ $err }
tokens-name-length = 令牌名称长度必须在 1 到 128 个字符之间。
tokens-store-failed = 保存令牌失败。
tokens-created-toast = 令牌已创建。

tokens-revoked-not-found = 未找到已吊销的令牌。
tokens-revoked-toast = 令牌已吊销。
tokens-already-revoked = 该令牌已被吊销。
tokens-revoke-failed = 吊销失败。

tokens-load-failed = 无法加载令牌。
tokens-not-found-or-revoked = 未找到令牌，或该令牌已被吊销。
tokens-rotated-not-found = 未找到已轮换的令牌。
tokens-rotated-toast = 令牌已轮换——请复制新的值。
tokens-rotate-failed = 轮换失败。

tokens-removed-toast = 令牌已移除。
tokens-still-active = 该令牌仍处于生效状态——请先将其吊销。
tokens-remove-failed = 移除失败。

tokens-not-found = 未找到令牌。
tokens-update-failed = 无法更新令牌。
tokens-tool-use-enabled-toast = 已为此令牌启用工具使用。
tokens-tool-use-disabled-toast = 已为此令牌禁用工具使用。
tokens-mcp-ask-enabled-toast = 已为此令牌启用通过 API 使用“ask”模式的 MCP 工具。
tokens-mcp-ask-disabled-toast = 已为此令牌禁用通过 API 使用“ask”模式的 MCP 工具。

tokens-unknown-tool = 未知工具。
tokens-save-pref-failed = 无法保存偏好设置。
tokens-capability-enabled-toast = 已为此令牌启用 { $name }。
tokens-capability-disabled-toast = 已为此令牌禁用 { $name }。
