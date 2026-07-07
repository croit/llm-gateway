# STATUS: llm-generated, unreviewed — pending native-speaker QA

connectors-page-title = 连接器 — LLM Gateway
connectors-heading = 连接器
connectors-restore-defaults-button = 恢复默认值
connectors-catalog-intro = 管理用户可在"集成"下连接的 MCP 服务器。启用连接器以使其可见。无法使用动态客户端注册的连接器(例如 Google)在启用前需要部署专用的 OAuth 客户端 ID/密钥。
connectors-empty-state = 暂无连接器。

connectors-badge-enabled = 已启用
connectors-badge-disabled = 已禁用
connectors-badge-default = 默认
connectors-badge-dcr = DCR
connectors-badge-needs-client-id = 需要客户端 ID
connectors-disable-button = 禁用
connectors-enable-disabled-title = 请先在下方添加 OAuth 客户端 ID(编辑 → OAuth 客户端 ID)
connectors-enable-button = 启用
connectors-delete-confirm = 删除此连接器?它将为所有用户移除,包括已保存的连接和令牌。此操作无法撤销。
connectors-delete-button = 删除
connectors-edit-summary = 编辑

connectors-add-summary = 添加连接器

connectors-oauth-help-token-1 = 令牌连接器:请在上方设置 MCP 服务器 URL;每位用户在"集成"中粘贴自己的 API 令牌(以
connectors-oauth-help-token-2 = 的形式发送)。无需 OAuth 客户端。

connectors-oauth-help-dcr-heading = 动态客户端注册 — 无需 OAuth 客户端
connectors-oauth-help-dcr-body = 只需在上方设置 MCP 服务器 URL。服务器会自动注册此网关(RFC 7591);随后每位用户点击"连接"并用自己的账户完成授权 — 一次登录即可覆盖服务器提供的所有服务。

connectors-oauth-help-gws-1 = 将其指向你
connectors-oauth-help-gws-self-hosted = 自托管的 Google Workspace MCP 服务器
connectors-oauth-help-gws-2 = (例如
connectors-oauth-help-gws-3 = ),以 streamable-HTTP 模式运行 — URL 以
connectors-oauth-help-gws-4 = 结尾。该服务器保存 Google OAuth 客户端并使用
connectors-oauth-help-gws-ga-apis = GA 版 Google API
connectors-oauth-help-gws-5 = (非开发者预览版)。请通过下方的环境变量在服务器上允许此网关的重定向 URI:
connectors-oauth-help-gws-footer = Google 托管的 MCP 端点(gmailmcp/calendarmcp/drivemcp.googleapis.com)有意未被使用 — 它们需要将组织注册到 Workspace 开发者预览计划中。部署方法请参见 docs/connectors.md。

connectors-oauth-help-generic-heading = 设置 OAuth 客户端
connectors-oauth-help-generic-intro = 请在你的 OAuth 客户端中注册这个确切的重定向 URI,然后在下方粘贴其客户端 ID(和密钥):
connectors-oauth-help-google-1 = Google:创建一个
connectors-oauth-help-google-link = OAuth 2.0 客户端 ID(Web 应用)
connectors-oauth-help-google-2 = ,在 Google Cloud Console 中添加上方的重定向 URI,并为该项目启用 Gmail / Google 日历 / Google 云端硬盘 API。
connectors-oauth-help-github-1 = GitHub:创建一个
connectors-oauth-help-github-link = OAuth 应用
connectors-oauth-help-github-2 = (设置 → 开发者设置 → OAuth 应用),将 Authorization 回调 URL 设为上方的重定向 URI,并复制客户端 ID 和生成的客户端密钥。
connectors-oauth-help-fallback = 请在你的服务商处创建一个 OAuth 客户端,使用此重定向 URI 以及下方设置的授权/令牌 URL。
connectors-oauth-why-1 = 为什么需要一次性的管理员操作?在 OAuth 中,客户端 ID 将
connectors-term-this-gateway = 此网关
connectors-oauth-why-2 = 标识为一个应用(由所有用户共享)— 仅每位用户的访问令牌不同。Claude Desktop 无需此步骤,因为 Anthropic 提供了绑定固定重定向 URL 的预注册应用;自托管网关使用自己的重定向 URI(如上),而 Google/GitHub 不像 Atlassian 那样支持自动注册(DCR)— 因此你只需注册一次,之后每位用户只需点击"连接"即可。
connectors-oauth-why-no-app = 完全没有 OAuth 应用?
connectors-oauth-why-3 = 将身份验证方式改为"用户提供的令牌",这样每位用户都会粘贴自己的令牌(例如 GitHub 个人访问令牌)— 凭据将直接来自用户本人,无需管理员客户端。

connectors-field-key-label = 键(稳定 ID)
connectors-field-key-placeholder = 例如 gmail
connectors-field-key-readonly-label = 键
connectors-field-name-label = 名称
connectors-field-name-placeholder = 显示名称
connectors-field-icon-label = 图标(表情符号)
connectors-field-category-label = 分类
connectors-field-category-placeholder = Google
connectors-field-description-label = 描述
connectors-field-description-placeholder = 此连接器的作用
connectors-field-url-label = MCP 服务器 URL
connectors-field-auth-label = 身份验证
connectors-auth-option-oauth = OAuth 2.1(每位用户通过服务商进行授权)
connectors-auth-option-token = 用户提供的令牌(每位用户粘贴自己的 API 令牌)
connectors-field-client-json-label = 粘贴 OAuth 客户端 JSON(可选 — 例如 Google 的"下载 JSON")
connectors-field-client-json-help = 从文件中填充客户端 ID/密钥(以及授权和令牌 URL)。或使用下方的各个字段。
connectors-field-client-id-label = OAuth 客户端 ID
connectors-field-client-id-placeholder = …apps.googleusercontent.com / GitHub OAuth 应用 ID
connectors-field-client-id-help-1 = 用于向服务商标识
connectors-field-client-id-help-2 = 此应用的公开 ID — 由管理员在服务商的 OAuth 凭据页面一次性创建(Google Cloud → 凭据,GitHub → OAuth 应用)。并非每个用户专属的密钥。若已启用 DCR,可留空。
connectors-field-client-secret-label = OAuth 客户端密钥
connectors-secret-placeholder-existing = ••••••••(留空以保留)
connectors-secret-placeholder-new = 客户端密钥(可选)
connectors-field-client-secret-help = 与客户端 ID 在同一页面签发。以加密方式存储;留空以保留现有密钥。
connectors-field-use-dcr-label = 尝试动态客户端注册(RFC 7591)
connectors-field-scopes-label = 作用域(以空格分隔)
connectors-advanced-summary = 高级:发现覆盖项
connectors-field-authorize-url-label = 授权 URL
connectors-field-token-url-label = 令牌 URL
connectors-field-registration-url-label = 注册 URL
connectors-placeholder-optional-override = 可选覆盖项
connectors-field-required-role-label = 所需角色(RBAC 门控)
connectors-placeholder-optional = 可选
connectors-save-changes-button = 保存更改
connectors-add-connector-button = 添加连接器

connectors-error-missing-fields = 键、名称和 URL 为必填项
connectors-error-bad-client-json = 无法从粘贴的 JSON 中读取 client_id — 期望的是 Google OAuth 客户端文件({"{"}"web":{"{"}"client_id":…,"client_secret":…{"}"}{"}"})。
connectors-error-sealing-secret = 密封密钥时出错:{ $error }
connectors-error-saving = 保存连接器时出错:{ $error }
connectors-error-needs-client-id = 此连接器需要 OAuth 客户端 ID 才能启用(它无法使用动态注册)。请编辑它并添加客户端 ID/密钥。
connectors-error-toggling = 切换连接器时出错:{ $error }
connectors-error-deleting = 删除连接器时出错:{ $error }
connectors-error-restoring = 恢复默认值时出错:{ $error }
