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
