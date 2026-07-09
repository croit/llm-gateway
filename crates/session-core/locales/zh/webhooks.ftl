# STATUS: llm-generated, unreviewed — pending native-speaker QA

webhooks-page-title = Webhook — LLM Gateway
webhooks-edit-page-title = 编辑 Webhook — LLM Gateway

webhooks-heading = Webhook
webhooks-intro = 当外部服务调用某个 URL 时运行提示词。你会获得一个保密的触发 URL；调用方在请求正文中发送的内容会追加到你的提示词后，运行结果会作为一个新的对话打开，你可以在这里阅读。
webhooks-create-submit = 创建 Webhook
webhooks-save-submit = 保存更改
webhooks-edit-heading = 编辑 Webhook
webhooks-back = 返回
webhooks-list-heading = 你的 Webhook
webhooks-list-empty = 还没有 Webhook。在上方创建一个。

webhooks-name-label = 名称
webhooks-name-placeholder = 例如：部署摘要
webhooks-model-label = 模型
webhooks-model-placeholder = 模型 ID
webhooks-prompt-label = 提示词
webhooks-prompt-placeholder = 模型应该如何处理传入的数据？

webhooks-sync-toggle-label = 等待响应（将模型输出返回给调用方）
webhooks-tools-toggle-label = 允许工具（使用你的工具运行，例如网页搜索、RAG、连接器）
webhooks-tools-warning = 任何拥有触发 URL 的人都可以发送内容，让模型以你的身份使用你的工具进行处理。仅对可信的调用方启用此项。

webhooks-gdpr-warning = 此模型在欧盟境外运行。请勿通过此 Webhook 发送个人数据。
webhooks-nda-warning = 此模型未获准处理受 NDA 限制的内容。请勿通过此 Webhook 发送机密数据。
webhooks-model-non-gdpr = { $model }（非欧盟）
webhooks-model-nda-restricted = { $model }（受 NDA 限制）
webhooks-model-non-gdpr-nda-restricted = { $model }（非欧盟，受 NDA 限制）

webhooks-reveal-heading = 你的触发 URL
webhooks-reveal-note = 立即复制——它只显示一次。任何拥有此 URL 的人都可以触发该 Webhook。丢失了？轮换以获取新的 URL。
webhooks-copy = 复制

webhooks-badge-active = 已启用
webhooks-badge-paused = 已暂停
webhooks-mode-sync = 等待响应
webhooks-mode-async = 触发即忘
webhooks-never-fired = 尚未触发
webhooks-last-success = 上次触发于 { $when }
webhooks-last-success-open = 上次触发于 { $when } — 打开
webhooks-last-failure = 上次触发失败于 { $when }
webhooks-last-failure-open = 上次触发失败于 { $when } — 打开

webhooks-pause-title = 暂停
webhooks-resume-title = 恢复
webhooks-rotate-title = 轮换密钥
webhooks-edit-title = 编辑
webhooks-delete-title = 删除

webhooks-err-name-length = 名称为必填项，且不得超过 128 个字符。
webhooks-err-prompt-length = 提示词为必填项，且不得超过 8000 个字符。
webhooks-err-pick-model = 请选择一个模型。

webhooks-toast-created = Webhook 已创建。
webhooks-toast-updated = Webhook 已更新。
webhooks-toast-paused = Webhook 已暂停。
webhooks-toast-resumed = Webhook 已恢复。
webhooks-toast-rotated = 密钥已轮换——旧 URL 不再有效。
webhooks-toast-deleted = Webhook 已删除。
webhooks-toast-already-gone = 该 Webhook 已不存在。
webhooks-toast-not-found = 未找到 Webhook。
webhooks-toast-save-failed = 无法保存 Webhook。
webhooks-toast-update-failed = 无法更新 Webhook。
webhooks-toast-delete-failed = 无法删除 Webhook。
webhooks-toast-refresh-failed = 无法刷新 Webhook。

# --- 使用不同的提示词重新运行 ---
webhooks-rerun-link = 重新运行
webhooks-rerun-page-title = 重新运行 Webhook — LLM Gateway
webhooks-rerun-heading = 使用不同的提示词重新运行
webhooks-rerun-intro = 使用可编辑的提示词，重放此 Webhook 最近收到的载荷。运行结果会作为新的对话打开。
webhooks-rerun-payload-label = 已捕获的载荷（原样重放）
webhooks-rerun-submit = 重新运行
webhooks-rerun-no-payload = 此 Webhook 尚未捕获载荷——请先触发一次。
webhooks-rerun-no-payload-notice = 此 Webhook 尚未触发，因此没有可重放的载荷。请先触发一次，然后回来使用不同的提示词重新运行。
webhooks-toast-rerun-started = 重新运行完成——正在打开对话……

# --- 运行历史 ---
webhooks-runs-link = 运行记录
webhooks-runs-page-title = Webhook 运行记录 — LLM Gateway
webhooks-runs-heading = 运行记录 · { $name }
webhooks-runs-intro = 最近的触发和重新运行。打开某次运行以阅读其对话，或使用不同的提示词重新运行其载荷。
webhooks-runs-empty = 还没有运行记录。触发该 Webhook 后即可在此查看历史。
webhooks-run-open = 打开对话
webhooks-run-rerun = 重新运行
webhooks-run-source-fire = 触发
webhooks-run-source-rerun = 重新运行
webhooks-run-status-ok = 成功
webhooks-run-status-error = 错误
webhooks-run-status-pending = 运行中

# --- 复用对话 ---
webhooks-reuse-toggle-label = 复用对话（每次触发都接续上一次的对话）
webhooks-reuse-rounds-prefix = 重放最近
webhooks-reuse-rounds-suffix = 轮
webhooks-reuse-rounds-aria = 要重放的历史轮数
