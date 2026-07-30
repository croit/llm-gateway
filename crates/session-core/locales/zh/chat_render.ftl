# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/chat/render.rs` — the
# gateway-only chat-page chrome: the header model/voice pickers, the
# compliance banners, the composer's "+" tools/integrations/skills menu,
# the "Denken" (effort/thinking) picker, and the share/export/fork
# controls. Prefixed `chat-render-` (rather than `chat-`) to avoid
# colliding with `chat/mod.rs`'s own `chat-*` keys in the sibling
# `chat.ftl`.

chat-render-canvas-toggle-title = 显示/隐藏文档画布
chat-render-canvas-toggle-label = 画布
chat-render-canvas-document-tab = 文档
chat-render-canvas-assets-tab = 文件
chat-render-canvas-assets-heading = 对话文件
chat-render-canvas-assets-count = { $count } 个文件
chat-render-canvas-assets-empty = 此对话中还没有文件。
chat-render-canvas-asset-download = 下载文件
chat-render-canvas-close-title = 关闭画布

chat-render-model-placeholder = 模型（例如 gpt-4o-mini）
chat-render-model-aria = 聊天模型
chat-render-voice-model-aria = 语音模型
chat-render-tts-voice-aria = 朗读语音
chat-render-tts-voice-default = 默认语音

chat-render-model-non-gdpr = { $id }（不符合 GDPR）
chat-render-model-confidential = { $id }（保密限制）
chat-render-model-non-gdpr-confidential = { $id }（不符合 GDPR，保密限制）

chat-render-gdpr-banner = 您正在向不符合 GDPR 的模型发送数据。请勿输入个人信息（姓名、电子邮件、地址、客户或员工数据）。
chat-render-nda-banner = 此模型不受保密协议保护。请勿发送受 NDA 保护或专有的资料。

chat-render-shared-readonly-banner = 共享对话——只读。只有创建者可以回复。
chat-render-composer-placeholder = 给模型发消息…

chat-render-new-conversation-fallback = 新对话

chat-render-feedback-title = 发送反馈

chat-render-effort-title = 思考强度
chat-render-effort-tooltip = 思考强度：越高 = 推理越多、工具调用轮次越多，但速度更慢
chat-render-effort-label-prefix = 思考：
chat-render-effort-fast = 快速
chat-render-effort-standard = 标准
chat-render-effort-deep = 深度
chat-render-effort-max = 最大

chat-render-tools-tooltip = 本次对话的工具、集成与技能
chat-render-tools-label = 工具
chat-render-tools-search-placeholder = 搜索工具…
chat-render-all-tools-label = 所有工具
chat-render-no-tools-prefix = 您的账户目前还没有可用的工具。请在
chat-render-no-tools-suffix = 页面连接一个集成。

chat-render-close = 关闭

chat-render-group-web-network = 网络
chat-render-group-attachments-documents = 附件与文档
chat-render-group-document-templates = 文档模板
chat-render-group-knowledge-base = 知识库
chat-render-group-code-sandbox = 代码与沙盒
chat-render-group-memory = 记忆
chat-render-group-integrations = 集成
chat-render-group-utility = 实用工具
chat-render-group-skills = 技能

chat-render-tool-count = { $count } 个工具

chat-render-active-count-title = 已启用的工具——点击管理
chat-render-unpin-title = 取消固定（恢复为自动）

chat-render-state-off-tip = 关闭——已阻止；对助手隐藏
chat-render-state-auto-tip = 自动——助手会在需要时自行开启
chat-render-state-on-tip = 开启——始终对助手可用

chat-render-share-label-on = 已共享 ✓
chat-render-share-label-off = 共享
chat-render-share-tooltip = 已共享的对话，任何持有链接的已登录用户都可以阅读

chat-render-fork-tooltip = 将此对话复制到您自己的聊天中，以便继续对话
chat-render-fork-label = 在我的聊天中继续

chat-render-export-tooltip = 下载此对话
chat-render-export-aria = 导出对话
chat-render-export-label = 导出
chat-render-export-pdf = PDF 文档
chat-render-export-md = Markdown (.md)
