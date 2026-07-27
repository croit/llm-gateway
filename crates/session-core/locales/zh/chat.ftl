# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/chat/mod.rs` — the
# multi-conversation chat page's server-side handlers: page title
# fallback, sidebar/effort/share/pin toasts, and the SSE-toast error
# messages the composer's fetch layer surfaces on failed actions.

chat-default-title = 聊天

chat-toast-conversation-already-gone = 该对话已被删除。
chat-toast-share-copied = 链接已复制 — 任何拥有该链接的已登录用户都可以查看此对话。
chat-toast-share-stopped = 共享已停止 — 该链接不再有效。
chat-toast-pinned = 已置顶 — 此对话现在会一直显示在顶部。
chat-toast-unpinned = 已取消置顶。
chat-toast-already-in-your-chats = 该对话已存在于你的聊天列表中。
chat-toast-effort-set = 思考强度：{ $level }

chat-mcp-bridged-description = 通过“{ $name }”集成桥接的工具。

chat-error-conversation-not-found = 未找到对话。
chat-error-message-not-found = 未找到消息。
chat-error-message-empty = 消息不能为空
chat-error-message-must-not-be-empty = 消息不能为空。
chat-error-still-streaming = 该用户仍有响应正在生成 — 请等待或点击停止。
chat-error-retry-assistant-only = 重试仅适用于助手的回复。
chat-error-edit-own-messages-only = 编辑仅适用于你自己的消息。
chat-error-pdf-export-unavailable = PDF 导出不可用：网关未安装 typst CLI
chat-error-pdf-export-failed = PDF 导出失败

chat-error-document-not-found = 未找到文档。
chat-error-document-too-large = 该文档过大，无法保存（上限 512 KB）。

chat-error-auth-required = 需要身份验证
chat-error-no-such-turn = 没有此消息
chat-error-db-error = 数据库错误
chat-error-attachments-not-configured = 聊天附件未配置
chat-error-bad-filename = 文件名无效
chat-error-attachment-not-found = 未找到
chat-error-rate-limited = 您已达到使用限制。详情及重置时间请见 /usage。
