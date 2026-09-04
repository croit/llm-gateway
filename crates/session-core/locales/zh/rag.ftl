# STATUS: llm-generated, unreviewed — pending native-speaker QA

rag-page-title = RAG 集合 — LLM Gateway
rag-heading = RAG 集合
rag-description-prefix = 网关已索引的代码库。该
rag-description-suffix = 工具会检索这些集合以回答有关代码的问题。
rag-collections-heading = 已配置的集合
rag-empty-list = 尚无集合。请在上方创建一个。

# Toasts — collection CRUD
rag-toast-malformed-form = 表单格式错误：{ $err }
rag-toast-name-exists = 名为 `{ $name }` 的集合已存在
rag-toast-create-failed = 无法创建集合
rag-toast-indexing-queued = `{ $name }` @ `{ $ref }` 的索引任务已排队。
rag-toast-created-aggregate = 已创建 `{ $name }`（聚合）。请在下方添加源仓库以进行索引。
rag-toast-collection-not-found = 未找到集合
rag-toast-collection-not-found-cap = 未找到集合。
rag-toast-load-collection-failed = 无法加载集合
rag-toast-load-collection-failed-cap = 无法加载集合。
rag-toast-name-length = 名称长度必须为 1..=64 个字符。
rag-toast-git-url-required = 需要 Git URL。
rag-toast-embedding-model-required = 需要 Embedding 模型。
rag-toast-chunk-size-range = 分块大小必须在 (0, 8000] 范围内。
rag-toast-chunk-overlap-range = 分块重叠必须在 [0, chunk_size) 范围内。
rag-toast-save-failed = 保存集合失败。
rag-toast-vanished = 保存后集合消失了。
rag-toast-saved-reload-failed = 已保存，但重新加载失败。
rag-toast-saved = 已保存 `{ $name }`。
rag-toast-collection-removed = 集合已移除。
rag-toast-collection-already-gone = 集合已不存在。
rag-toast-delete-failed = 删除失败。

# Toasts — refs / sources
rag-toast-reindex-queue-failed = 无法安排重新索引
rag-toast-reindex-queued-count = 已安排 { $count } 个 ref 的重新索引。
rag-toast-ref-required = 需要 ref（分支/标签/提交）。
rag-toast-ref-exists = ref `{ $ref }` 在此集合中已存在
rag-toast-add-ref-failed = 无法添加 ref
rag-toast-indexing-queued-ref = `{ $ref }` 的索引任务已排队。
rag-toast-no-source-urls = 未找到源 URL。
rag-toast-bulk-queued-skipped = 已排队 { $added } 个源；已跳过 { $skipped } 个重复项。
rag-toast-bulk-queued = 已排队 { $added } 个源的索引任务。
rag-toast-ref-not-found = 未找到 ref
rag-toast-reindex-queued-ref = 已安排 `{ $ref }` 的重新索引。
rag-toast-set-primary-failed = 无法设为主要
rag-toast-now-default = `{ $ref }` 现为默认 ref。
rag-toast-delete-ref-failed = 无法删除 ref
rag-toast-ref-removed = 已移除 ref `{ $ref }`。
rag-toast-load-log-failed = 无法加载日志
rag-toast-git-url-required-aggregate = 聚合源需要 Git URL。
rag-toast-update-source-failed = 无法更新源
rag-toast-source-updated = 源已更新。

# Status badges
rag-status-pending = 待处理
rag-status-cloning = 克隆中
rag-status-indexing = 索引中
rag-status-ready = 就绪
rag-status-error = 错误

# Collection row
rag-pat-set = 已设置 PAT
rag-pat-none = 无 PAT
rag-meta-aggregate = { $count } 个源 · { $hint }
rag-meta-versioned = { $url } · { $hint }
rag-badge-aggregate = 聚合
rag-embed-prefix = embed：
rag-button-edit = 编辑
rag-button-delete-collection = 删除集合
rag-placeholder-source-git-url = https://github.com/org/repo.git
rag-placeholder-ref-default = ref（默认：集合的设置）
rag-button-add-source = 添加源
rag-placeholder-branch-tag-commit = 分支、标签或提交
rag-button-add-ref = 添加 ref
rag-placeholder-bulk-sources = 批量添加 — 每行一个仓库，可选 @ref：
    https://github.com/proxmox/pve-manager.git
    https://github.com/proxmox/qemu-server.git @master
rag-button-add-bulk = 批量添加源

# Ref / source row
rag-badge-primary = 主要
rag-ref-indexed-line = 索引于 { $date } · { $commit }
rag-never = 从未
rag-button-log = 日志
rag-button-reindex = 重新索引
rag-button-set-primary = 设为主要
rag-button-remove = 移除

# Indexing log
rag-log-info = 信息
rag-log-warn = 警告
rag-log-error = 错误
rag-log-heading = 索引日志
rag-log-empty = 尚未记录任何索引事件。索引器处理此 ref 后，首次运行的记录会显示在这里。

# Inline per-source editor
rag-label-git-url-source = Git URL（此源）
rag-label-git-url-inherit = Git URL（留空 = 继承集合设置）
rag-placeholder-git-url = https://example.com/org/repo.git
rag-label-branch-tag = 分支 / 标签
rag-button-save-source = 保存源
rag-button-cancel = 取消

# Create-collection form
rag-create-heading = 索引新集合
rag-create-description = 索引器会克隆仓库，将每个文件分块，并通过配置的 Embedding 模型生成嵌入向量。PAT 以明文存储（网关运行在受信任的基础设施上）。
rag-label-name = 名称
rag-placeholder-name = 例如 gateway-repo
rag-label-description-optional = 描述（可选）
rag-placeholder-description = 简短、易读
rag-label-git-url-versioned = Git URL（仅版本化）
rag-label-pat-optional = 个人访问令牌（可选）
rag-placeholder-pat = 用于私有仓库
rag-label-include-globs-full = 包含通配符（用逗号或换行分隔）
rag-placeholder-include-globs = *.rs, *.md
rag-label-exclude-globs = 排除通配符
rag-placeholder-exclude-globs = target/, node_modules/
rag-label-chunk-size = 分块大小
rag-label-chunk-overlap = 分块重叠
rag-create-aggregate-help = 聚合（多源）：将多个仓库作为一个整体语料库进行搜索。将 Git URL 留空，创建后再添加各个源仓库。分支 / 标签将成为新增源的默认 ref。
rag-button-queue-indexing = 排队索引

# Edit-collection form
rag-edit-heading = 正在编辑 { $name }
rag-label-description = 描述
rag-label-pat = 个人访问令牌
rag-badge-pat-set = 当前已设置
rag-badge-pat-none = 未存储
rag-placeholder-pat-keep = 留空以保留现有值
rag-label-clear-pat = 移除已存储的 PAT（不再进行身份验证）
rag-label-include-globs = 包含通配符
rag-button-save-changes = 保存更改

# Embedding model field
rag-label-embedding-model = Embedding 模型
rag-placeholder-embedding-model-none = 未配置 Embedding 池 — 请输入模型 ID
rag-option-choose-embedding-model = 选择 Embedding 模型…
rag-suffix-not-advertised = （不再提供）

rag-label-allowed-groups = åè®¸çç»
rag-hint-allowed-groups = åè®¸ååºåæç´¢æ­¤éåçç½å³ç»ï¼ç¨éå·åéï¼ãçç©º = æææ¥æ RAG å·¥å·çäººãç®¡çåå§ç»ææéã

# 来源选择器和提供方凭据（rag_source.rs）。各字段标签由提供方自身给出，
# 不做翻译。
rag-label-source-kind = 来源
rag-source-git-label = Git 仓库
rag-source-git-help = 克隆仓库并索引其中的文件。原有行为。
rag-source-secret-stored = 已保存
rag-source-secret-placeholder = 留空以保留已保存的值
rag-source-secret-clear = 清除已保存的值
rag-source-unknown-kind = 未知的来源类型。
rag-source-test-button = 测试连接
rag-source-test-ok = 已以 `{ $account }` 连接。所配置文件夹下有 { $entries } 个项目。
rag-source-test-ok-plain = 已连接。所配置文件夹下有 { $entries } 个项目。
rag-source-test-failed = 无法访问来源：{ $error }
rag-source-test-git = 请选择要测试的远程来源。Git 仓库会在索引时检查。
rag-source-detected = 已检测到：{ $server }

rag-label-profile = 文档字段
rag-option-profile-none = 无 — 仅索引文本
rag-profile-help = 从每个文档中提取字段（供应商、日期、金额、项目），以便筛选、排序和汇总。每个文档需一次模型调用；代码或纯文本集合请保持“无”。

# 提取配置编辑器（/rag/profiles，rag_profiles.rs）
rag-profile-page-title = 提取配置 — LLM Gateway
rag-profile-heading = 提取配置
rag-profile-description = 决定从集合中每个文档里提取什么：正是这些字段让“X 最近的一张发票”或“我们花了多少钱”变得可回答。在 RAG 页面为集合指定配置。
rag-profile-create-heading = 新建配置
rag-profile-list-heading = 配置
rag-profile-empty = 尚无配置。
rag-profile-builtin = 内置
rag-profile-version = v{ $version }
rag-profile-summary = { $count } 个字段
rag-profile-label-name = 名称
rag-profile-label-description = 说明
rag-profile-label-prompt = 提取指令
rag-profile-label-fields = 字段（JSON）
rag-profile-prompt-placeholder = 描述模型正在读什么，以及日期和金额应如何规范化。
rag-profile-fields-help = 每个字段一个对象：key、label、type（text | number | date | enum）、description，以及可选的 filterable / sortable。enum 还需要 "values"。说明会展示给模型，请写准确。
rag-profile-edit-warning = 保存会提升该配置的版本并清空其提取缓存。使用该配置的集合需要重新索引才能应用新字段。
rag-profile-button-create = 创建配置
rag-profile-button-save = 保存
rag-profile-button-delete = 删除
rag-profile-link = 编辑提取配置
rag-profile-toast-created = 已创建配置 `{ $name }`。
rag-profile-toast-saved = 已保存 `{ $name }`。
rag-profile-toast-saved-reindex = 已保存 `{ $name }`。重新索引以生效：{ $collections }。
rag-profile-toast-deleted = 配置已删除。
rag-profile-toast-name-exists = 已存在名为 `{ $name }` 的配置
rag-profile-toast-name-length = 名称长度必须为 1 至 64 个字符。
rag-profile-toast-name-charset = 名称只能包含字母、数字、`-` 和 `_`。
rag-profile-toast-prompt-required = 必须填写提取指令。
rag-profile-toast-fields-invalid = 字段不是有效的 JSON：{ $err }
rag-profile-toast-fields-empty = 配置至少需要一个字段。
rag-profile-toast-field-key-required = 每个字段都需要 key。
rag-profile-toast-field-duplicate = 字段 key `{ $key }` 重复。
rag-profile-toast-enum-values = 字段 `{ $key }` 是 enum，需要提供 "values" 列表。
rag-profile-toast-in-use = 仍被使用：{ $collections }。请先为它们指定其他配置。
rag-profile-toast-builtin = 内置配置无法删除。请改为编辑或复制。
rag-profile-toast-save-failed = 保存配置失败。

# 同步钩子 —— 触发单个集合重新同步的入站请求。
rag-toast-sync-token = 同步 URL（仅显示一次，不会保存）：{ $url }
rag-toast-sync-token-cleared = 同步 URL 已停用。
rag-button-sync-token = 同步 URL
rag-button-sync-token-rotate = 新的同步 URL
rag-button-sync-token-clear = 停用同步 URL
rag-badge-sync-hook = 同步钩子

# Browser consent for an OAuth source (Google Drive).
rag-source-consent-save-first = 请先保存包含客户端 ID 和密钥的集合，然后再连接以授予访问权限。
rag-source-consent-connected = 已连接
rag-source-consent-not-connected = 未连接
rag-source-consent-connect = 连接
rag-source-consent-reconnect = 重新连接
rag-source-consent-help = 所有能搜索此集合的人都会看到所连接账号可见的文件。
rag-oauth-lookup-failed = 无法读取该集合。
rag-oauth-not-oauth = 此来源类型不通过浏览器连接。
rag-oauth-no-client = 请先在集合中保存 OAuth 客户端 ID 和密钥。
rag-oauth-bad-authorize-url = 无法构建提供方的授权 URL。
rag-oauth-start-failed = 无法开始授权。
rag-oauth-callback-missing = 提供方的响应缺少 code 或 state。
rag-oauth-expired = 该授权已过期或已被使用，请重新开始。
rag-oauth-provider-refused = 提供方拒绝了授权：{ $error }
rag-oauth-exchange-failed = 交换授权码失败：{ $error }
rag-oauth-no-refresh-token = 提供方未返回刷新令牌，网关将无法在无人值守时继续索引。请在提供方账号中撤销网关的访问权限后重新连接。
rag-oauth-store-failed = 无法保存凭据。
rag-badge-no-files = 未索引任何文件
rag-ref-files = { $files } 个文件
