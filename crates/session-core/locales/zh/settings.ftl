# STATUS: llm-generated, unreviewed — pending native-speaker QA
# 运维设置（/admin/settings）。卡片标题（settings-s-*）、字段标签
# （settings-f-*）及其说明（settings-f-*-help）都由
# gateway_core::server::settings::SECTIONS 中的条目推导而来：
# `sandbox.runner_url` -> `settings-f-sandbox-runner_url`。
# 源文本见 locales/en/settings.ftl。

settings-heading = 设置
settings-intro = 本网关的运维设置。它们保存在数据库中，无需配置文件——每个字段还会显示它所替代的 TOML 键名。
settings-save = 保存本节
settings-saved = 已保存，下一个请求起生效。
settings-saved-restart = 已保存。本节中部分字段需重启后才生效。
settings-save-failed = 无法保存这些设置。
settings-cleared = 已清除，重新使用内置默认值。
settings-restart-badge = 需重启
settings-restart-note = 标记为“需重启”的字段仅在网关启动时读取，修改后需重启才能生效。
settings-secret-set = 已存储——输入新值即可替换
settings-secret-unset = 未设置
settings-secret-clear = 清除

settings-no-backend-heading = 尚未配置模型后端
settings-no-backend-body = 登录已配置好，但在添加后端之前，本网关不提供任何模型。在此之前，聊天和 /v1 接口都会拒绝请求。
settings-no-backend-cta = 前往 /admin/upstreams 添加后端 →

settings-tab-chat = 聊天
settings-tab-tools = 工具
settings-tab-data = 内容与数据
settings-tab-access = 访问与用量
settings-tab-notifications = 通知
settings-show-fields = 显示另外 { $count } 项设置
settings-model-automatic = 自动 — 使用第一个可用模型
settings-model-none-configured = 尚未配置此类型的模型。请在 /admin/upstreams 添加相应的池，它就会出现在这里。
settings-model-unavailable = { $model }（已配置，但当前不可用）
settings-restart-pending-heading = 待重启
settings-restart-pending-body = 以下设置已保存，但需重启网关后才会生效：

# ─── 分区卡片 ─────────────────────────────────────────────────────────────────

settings-s-chat-ocr = 文档 OCR
settings-s-chat-ocr-blurb = 把上传的 PDF 与图片转成模型能读的文本。
settings-s-chat-compaction = 会话压缩
settings-s-chat-compaction-blurb = 概括长会话中较早的一半，使其继续能放进模型的上下文窗口。
settings-s-chat-s3 = 附件存储（S3）
settings-s-chat-s3-blurb = 聊天附件的对象存储。没有它，上传会被拒绝。
settings-s-sandbox = 代码沙箱
settings-s-sandbox-blurb = 运行模型所写代码的隔离执行器。
settings-s-comfyui = ComfyUI 图像与视频
settings-s-comfyui-blurb = 图像与视频工具背后的无界面 ComfyUI worker。
settings-s-rag = RAG 索引
settings-s-rag-blurb = 已索引来源存放在哪里，以及索引器的工作强度。
settings-s-skills = 技能
settings-s-skills-blurb = /admin/skills 背后的磁盘上的 bundle 目录。
settings-s-typst = Typst 模板
settings-s-typst-blurb = PDF 导出与文档工具背后的模板。
settings-s-geoip = GeoIP
settings-s-geoip-blurb = 客户端的粗略位置，供 get_user_location 工具使用。
settings-s-usage = 用量指标
settings-s-usage-blurb = /usage 背后的按请求计量。
settings-s-limits = 速率限制与配额
settings-s-limits-blurb = /admin/limits 中所配规则的总开关。
settings-s-feedback = 反馈组件
settings-s-feedback-blurb = 应用内反馈组件把 issue 提交到哪里。
settings-s-push = Web Push
settings-s-push-blurb = 回答完成时的通知。密钥对会自动生成并保存。
settings-s-gateway = 会话与令牌
settings-s-gateway-blurb = 浏览器登录与 API 令牌的有效期，以及管理员是否可以模拟其他用户。

# ─── 字段 ─────────────────────────────────────────────────────────────────────

settings-f-chat-ocr-enabled = 启用 OCR
settings-f-chat-ocr-enabled-help = 从上传文档中提取文本的总开关。
settings-f-chat-ocr-model = OCR 模型
settings-f-chat-ocr-model-help = 由哪个模型读取页面。它必须由 ocr 类型的池提供；保持自动则使用第一个可用的。
settings-f-chat-ocr-max_tokens = 每次请求的 token 预算
settings-f-chat-ocr-max_tokens-help = 单次 OCR 请求的 token 预算。
settings-f-chat-ocr-ngram_window = 重叠窗口
settings-f-chat-ocr-ngram_window-help = 拼接各页文本时使用的重叠量，避免内容重复。
settings-f-chat-ocr-max_bytes = 最大文档大小
settings-f-chat-ocr-max_bytes-help = 可接受的最大文档，以字节计。
settings-f-chat-ocr-max_pages = 最大页数
settings-f-chat-ocr-max_pages-help = 从单个文档中最多读取的页数。
settings-f-chat-ocr-dpi = 栅格化分辨率
settings-f-chat-ocr-dpi-help = 读取前渲染 PDF 页面所用的分辨率，以 DPI 计。
settings-f-chat-ocr-max_output_chars = 最大提取文本量
settings-f-chat-ocr-max_output_chars-help = 单个文档提取文本的上限，以字符计。
settings-f-chat-ocr-timeout_secs = 超时
settings-f-chat-ocr-timeout_secs-help = 处理单个文档的时限，以秒计。
settings-f-chat-ocr-max_concurrency = 并行页数
settings-f-chat-ocr-max_concurrency-help = 同时读取多少页。
settings-f-chat-ocr-auto_min_text_chars_per_page = 扫描件判定阈值
settings-f-chat-ocr-auto_min_text_chars_per_page-help = 每页内嵌字符少于此数时，PDF 视为扫描件并送入 OCR。

settings-f-chat-compaction-enabled = 启用压缩
settings-f-chat-compaction-enabled-help = 概括长会话的总开关。
settings-f-chat-compaction-default_context_window = 假定的上下文窗口
settings-f-chat-compaction-default_context_window-help = 对不上报上下文窗口的模型所假定的窗口大小，以 token 计。
settings-f-chat-compaction-trigger_ratio = 触发阈值
settings-f-chat-compaction-trigger_ratio-help = 触发压缩的上下文窗口占用比例（0.7 = 占用 70% 时）。
settings-f-chat-compaction-keep_recent_turns = 保留的最近轮次
settings-f-chat-compaction-keep_recent_turns-help = 会话末尾原样保留的轮次数。
settings-f-chat-compaction-min_turns_to_compact = 会话最小长度
settings-f-chat-compaction-min_turns_to_compact-help = 轮次少于此数的会话永不压缩。
settings-f-chat-compaction-summary_max_tokens = 摘要 token 预算
settings-f-chat-compaction-summary_max_tokens-help = 用于替换被压缩轮次的摘要的 token 预算。

settings-f-chat-s3-enabled = 将附件存入 S3
settings-f-chat-s3-enabled-help = 关闭时聊天附件不可用。
settings-f-chat-s3-endpoint = 端点 URL
settings-f-chat-s3-endpoint-help = 例如 https://s3.eu-central-1.amazonaws.com，或某个 MinIO 地址。
settings-f-chat-s3-region = 区域
settings-f-chat-s3-region-help = 区域名称。
settings-f-chat-s3-bucket = 存储桶
settings-f-chat-s3-bucket-help = 存放附件的存储桶。
settings-f-chat-s3-key_prefix = 键前缀
settings-f-chat-s3-key_prefix-help = 写入每个对象键时所用的前缀。
settings-f-chat-s3-access_key = Access Key ID
settings-f-chat-s3-access_key-help = 访问该存储桶所用访问密钥的标识。
settings-f-chat-s3-secret_key = Secret Access Key
settings-f-chat-s3-secret_key-help = 该访问密钥的私密部分。加密存储。

settings-f-sandbox-enabled = 启用沙箱工具
settings-f-sandbox-enabled-help = 注册让模型能运行代码的那些工具。
settings-f-sandbox-runner_url = Runner URL
settings-f-sandbox-runner_url-help = sandbox-runner 服务的基础 URL。它会执行任意代码，因此只能从网关访问。
settings-f-sandbox-timeout_secs = 超时
settings-f-sandbox-timeout_secs-help = 单次运行的 HTTP 时限，以秒计。
settings-f-sandbox-max_artifact_bytes = 最大产物大小
settings-f-sandbox-max_artifact_bytes-help = 从一次运行中接收回来的最大单个文件，以字节计。

settings-f-comfyui-enabled = 启用图像与视频工具
settings-f-comfyui-enabled-help = 注册 comfyui_* 系列工具。
settings-f-comfyui-base_url = ComfyUI URL
settings-f-comfyui-base_url-help = ComfyUI 实例的基础 URL。它没有任何认证，因此只能从网关访问。
settings-f-comfyui-content_dir = 工作流目录
settings-f-comfyui-content_dir-help = 每个工作流对应一个子目录。用 /admin/comfyui 上的重新加载按钮可免重启重新扫描。
settings-f-comfyui-timeout_secs = 超时
settings-f-comfyui-timeout_secs-help = 单次工作流运行的时限，以秒计。
settings-f-comfyui-queue_poll_interval_ms = 队列轮询间隔
settings-f-comfyui-queue_poll_interval_ms-help = 网关多久向 ComfyUI 查询一次正在运行的任务，以毫秒计。
settings-f-comfyui-max_concurrent_jobs = 并发任务数
settings-f-comfyui-max_concurrent_jobs-help = 模型可同时运行的工作流数量。

settings-f-rag-enabled = 运行索引器
settings-f-rag-enabled-help = RAG 索引与检索的总开关。
settings-f-rag-data_dir = 索引目录
settings-f-rag-data_dir-help = 索引的存放位置。必须位于持久卷上，否则每次重启都会重新索引。已有索引不会随之迁移——改指到新位置就意味着一切从头索引。
settings-f-rag-clone_concurrency = 并行索引任务
settings-f-rag-clone_concurrency-help = 同时运行多少个 git clone 和索引任务。

settings-f-skills-enabled = 加载技能 bundle
settings-f-skills-enabled-help = /admin/skills 所管理技能的总开关。
settings-f-skills-dir = Bundle 目录
settings-f-skills-dir-help = 存放技能 bundle 的目录。

settings-f-typst-enabled = 加载 Typst 模板
settings-f-typst-enabled-help = PDF 导出与文档工具的总开关。
settings-f-typst-templates_dir = 模板目录
settings-f-typst-templates_dir-help = 存放模板的目录。保存时重新扫描，因此新增模板无需重启。

settings-f-geoip-enabled = 启用 GeoIP 查询
settings-f-geoip-enabled-help = get_user_location 工具的总开关。
settings-f-geoip-db_path = 数据库文件
settings-f-geoip-db_path-help = IP2Location BIN 数据库的路径。
settings-f-geoip-update_token = 下载令牌
settings-f-geoip-update_token-help = 用于刷新数据库的 IP2Location 令牌。加密存储。

settings-f-usage-enabled = 记录用量
settings-f-usage-enabled-help = /usage 背后的按请求计量。
settings-f-usage-retention_days = 保留期
settings-f-usage-retention_days-help = 记录保留多少天。
settings-f-usage-currency = 货币
settings-f-usage-currency-help = 费用以哪种货币显示。

settings-f-limits-enabled = 强制执行限制与配额
settings-f-limits-enabled-help = 关闭时 /admin/limits 中的规则会被忽略。

settings-f-feedback-enabled = 提供反馈组件
settings-f-feedback-enabled-help = 应用内反馈按钮的总开关。
settings-f-feedback-github_owner = 仓库所有者
settings-f-feedback-github_owner-help = 拥有该 issue 跟踪器的 GitHub 用户或组织。
settings-f-feedback-github_repo = 仓库
settings-f-feedback-github_repo-help = issue 提交到哪个仓库。
settings-f-feedback-github_token = GitHub 令牌
settings-f-feedback-github_token-help = 需要 issues:write；若附带截图还需 contents:write。加密存储。
settings-f-feedback-github_api_base = API 基础 URL
settings-f-feedback-github_api_base-help = REST API 的基础 URL。使用 GitHub Enterprise 时需修改。
settings-f-feedback-labels = Issue 标签
settings-f-feedback-labels-help = 为每个提交的 issue 添加的标签。
settings-f-feedback-assets_branch = 截图分支
settings-f-feedback-assets_branch-help = 截图提交到的孤立分支。
settings-f-feedback-extraction_model = 抽取模型
settings-f-feedback-extraction_model-help = 把语音备注转成表单字段的聊天模型。
settings-f-feedback-voice_model = 转写模型
settings-f-feedback-voice_model-help = 把语音备注转成文本的模型。

settings-f-push-enabled = 发送推送通知
settings-f-push-enabled-help = 提供推送端点，并在一次回答完成时通知。
settings-f-push-contact = 运维联系方式
settings-f-push-contact-help = 推送服务可用来联系你的 mailto: 或 https: URI。

settings-f-gateway-token_ttl_days = API 令牌有效期
settings-f-gateway-token_ttl_days-help = 新签发的 gwk_… 令牌有效多少天。
settings-f-gateway-session_ttl_days = 会话空闲超时
settings-f-gateway-session_ttl_days-help = 浏览器登录的滑动空闲超时，以天计：每次请求都会向后顺延，因此这是离开多久之后需要重新登录。
settings-f-gateway-session_absolute_max_days = 会话最长存续时间
settings-f-gateway-session_absolute_max_days-help = 自登录起的硬性上限，以天计，任何活动都无法延长。它同时强制定期回到身份提供方——那是唯一会重新读取组声明的时刻。
settings-f-gateway-allow_impersonation = 允许模拟用户
settings-f-gateway-allow_impersonation-help = 允许管理员以其他用户身份进行调试。每次模拟都会被审计并显示常驻横幅；关闭时按钮隐藏，端点也会拒绝。
