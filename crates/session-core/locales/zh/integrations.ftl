# STATUS: llm-generated, unreviewed — pending native-speaker QA

integrations-page-title = 集成 — LLM Gateway
integrations-heading = 集成
integrations-intro = 连接您自己的账户，让助手可以代表您执行操作——读取您的邮件、日历、文件、代码仓库等。每个连接都使用您自己的权限，随时可以断开。
integrations-empty = 目前还没有可用的连接器。管理员可以在“管理 → 连接器”中启用它们。

integrations-badge-connected = 已连接
integrations-badge-needs-reconnect = 需要重新连接
integrations-badge-needs-admin-setup = 需要管理员设置

integrations-reconnect-title = 重新建立连接（重新授权 / 重试）
integrations-reconnect-button = 重新连接
integrations-disconnect-button = 断开连接
integrations-disconnect-confirm = 断开此集成？您存储的访问令牌将被删除。
integrations-connect-button = 连接

integrations-token-label = 您的 API 令牌
integrations-token-placeholder = 粘贴您的令牌

integrations-tools-error-prefix = 无法加载此连接器的工具：
integrations-tools-error-hint = 请检查 MCP 服务器 URL / 您的令牌，然后使用上方的“重新连接”。
integrations-tools-error-hint-reauth = 您的授权已失效 — 请使用上方的“重新连接”重新登录。
integrations-tools-empty = 此连接器不提供任何工具。
integrations-tools-header = 工具权限（{ $count }）
integrations-set-all-label = 全部设置：
integrations-mode-always = 始终
integrations-mode-ask = 询问
integrations-mode-off = 关闭
integrations-tools-toggle = 显示 / 隐藏各个工具
integrations-tool-kind-read = 读取
integrations-tool-kind-write = 写入

integrations-error-unknown-connector = 未知或已禁用的连接器
integrations-error-forbidden-role = 您无权访问此连接器
integrations-error-not-oauth = 此连接器不使用 OAuth
integrations-error-oauth-discovery-failed = OAuth 发现失败：{ $error }
integrations-error-needs-setup-no-client = 此连接器需要设置：未配置客户端 ID，且该提供方不支持动态注册。请让管理员添加 OAuth 客户端。
integrations-error-sealing-client-secret = 封存客户端密钥失败：{ $error }
integrations-error-dcr-failed = 动态客户端注册失败：{ $error }
integrations-error-needs-setup-admin = 此连接器需要设置：管理员必须配置 OAuth 客户端 ID。
integrations-error-building-authorize-url = 构建授权 URL 失败：{ $error }
integrations-error-persisting-authorization = 保存授权失败：{ $error }
integrations-error-provider-error = 提供方返回了错误：{ $error } { $desc }
integrations-error-callback-missing = 回调缺少 code 或 state
integrations-error-auth-expired = 此授权已过期或已被使用——请从“集成”页面重新开始
integrations-error-loading-authorization = 加载授权失败：{ $error }
integrations-error-state-mismatch = 授权状态与您的会话不匹配
integrations-error-connector-missing = 该连接器已不存在
integrations-error-decrypting-client-secret = 解密客户端密钥失败：{ $error }
integrations-error-connector-missing-client-id = 该连接器缺少其 OAuth 客户端 ID
integrations-error-sealing-access-token = 封存访问令牌失败：{ $error }
integrations-error-sealing-refresh-token = 封存刷新令牌失败：{ $error }
integrations-error-saving-connection = 保存连接失败：{ $error }
integrations-error-not-token-based = 此连接器不基于令牌
integrations-error-token-required = 需要提供令牌
integrations-error-sealing-token = 封存令牌失败：{ $error }
integrations-error-unknown-connector-plain = 未知连接器
integrations-error-invalid-mode = 无效的权限模式
integrations-error-saving-tool-permission = 保存工具权限失败：{ $error }
integrations-error-saving-permissions = 保存权限失败：{ $error }
integrations-error-listing-tools = 列出工具失败：{ $error }
integrations-error-disconnecting = 断开连接失败：{ $error }
integrations-error-connection-unavailable = 连接不可用
