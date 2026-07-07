# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `session-core/src/render.rs` — the HTML renderers for
# the chat-style session UI (conversation bubbles, tool-call rows, the
# document canvas, and the composer). Driver-agnostic: both the gateway
# and any future consumer of this crate render through these functions.

render-edit-button = ✎ 编辑
render-edit-confirm = 保存并重新生成？这将删除下方的所有消息。
render-edit-save = 保存并重新生成
render-edit-cancel = 取消

render-retry-button = ↻ 重试
render-retry-confirm = 重新生成此回复？这将删除该回复及其下方的所有内容。

render-attachment-unavailable-title = 此附件已不可用
render-attachment-unavailable-meta = 不可用
render-attachment-open-title = 打开 { $filename } · { $mime } · { $size }
render-attachment-title = { $filename } · { $mime } · { $size }
render-attachment-chip-title = { $mime } · { $size }

render-thinking-spinner = 思考中…
render-thinking-finalized = 思考了 { $secs } 秒
render-thinking-in-progress = 思考中…（{ $secs } 秒）

render-tools-running = 工具运行中
render-tools-errored = 工具调用
render-tools-used = 已使用的工具
render-tools-summary = { $count } 次调用 · { $breakdown }

render-tool-status-calling = 调用中
render-tool-status-used = 已使用
render-tool-status-error = 工具错误
render-tool-input-label = 输入
render-tool-output-label = 输出
render-tool-output-truncated = 显示已截断 — 完整的 { $bytes } 字节仍可供模型使用并保存在数据库中；此处显示前 { $chars } 个字符

render-canvas-close-title = 关闭
render-canvas-close-aria = 关闭文档面板
render-canvas-document-aria = 文档
render-canvas-version-aria = 版本

render-composer-attach-aria = 添加附件
render-composer-attach-title = 添加附件（也可拖放/粘贴）
render-composer-record-aria = 录制语音消息
render-composer-record-title = 录音
render-composer-send = 发送
render-composer-stop = 停止
