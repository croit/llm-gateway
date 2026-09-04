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

# Web Push "turn complete" opt-in card (rendered by `render_push_card`; wired
# client-side by `ui/ts/push.ts`). Device-local notification settings.
tokens-push-heading = 通知
tokens-push-description = 当您发起的回答在您离开应用时完成，在此设备上收到通知。
tokens-push-enable = 在此设备上启用
tokens-push-disable = 在此设备上停用
tokens-push-on = 此设备已开启通知。
tokens-push-off = 此设备已关闭通知。
tokens-push-denied = 此浏览器已阻止通知。请在浏览器设置中允许以启用。
tokens-push-unsupported = 此浏览器不支持通知。
tokens-push-enabled = 已在此设备上启用通知。
tokens-push-disabled = 已在此设备上停用通知。
tokens-push-error = 无法更改通知设置。

# 每个令牌的用量、模型白名单与配额（/tokens）。
tokens-usage-line = 本月：{ $requests } 次请求 · { $tokens } tokens · { $cost }
tokens-models-summary-all = 模型：全部
tokens-models-summary-restricted = 模型：已选 { $count } 个
tokens-models-help = 关闭时，此令牌跟随你自己的访问权限，包括以后新增的模型。开启时，它只能使用你勾选的模型——之后新增的模型在你于此处勾选之前都会被拒绝。
tokens-models-restrict-label = 将此令牌限制为特定模型
tokens-models-none-picked = 请至少勾选一个模型，或关闭该限制。
tokens-models-save = 保存模型
tokens-models-saved-toast = 令牌已限制为 { $count } 个模型。
tokens-models-cleared-toast = 令牌可使用你的全部模型。
tokens-limits-summary-none = 配额：无
tokens-limits-summary-some = 配额：{ $count } 条规则
tokens-limits-help = 仅针对此令牌的上限。你自己的预算仍然有效，因此这只会收紧该令牌的用量，绝不会放宽。
tokens-limits-add = 添加配额
tokens-limits-remove = 移除
tokens-limits-saved-toast = 令牌配额已保存。
tokens-limits-removed-toast = 令牌配额已移除。
tokens-limits-not-yours = 该配额不由你移除。
tokens-limits-admin-set = 该配额由管理员为此令牌设置，只能在管理员限额页面更改。
tokens-limits-admin-badge = 由管理员设置
tokens-models-admin-set = 运营方还将此令牌限制为：{ $models }。你的选择只能在此基础上进一步收紧，无法放宽。
