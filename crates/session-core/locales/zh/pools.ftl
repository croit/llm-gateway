# STATUS: llm-generated, unreviewed — pending native-speaker QA

pools-page-title = 上游池 — LLM Gateway
pools-heading = 上游池
pools-description = 按类型和选择器策略将后端分组为池。更改会保存到数据库，但只有在您点击“应用更改”后才会生效。

pools-fallbacks-heading = 未知模型回退
pools-fallbacks-description = 当请求指定了网关从未见过的模型时，为该类型替换使用此模型。留空 = 未命中时返回 404。

pools-add-heading = 添加池
pools-field-name = 名称
pools-field-kind = 类型
pools-field-strategy = 策略
pools-field-fallback-offline = 离线回退模型
pools-field-fallback-offline-placeholder = 当所有后端都离线时提供服务
pools-field-models = 提供的模型（白名单，逗号分隔）
pools-field-models-hint = 设置后，对于启用 /models 探测的后端仅提供这些 id，其余以划线显示。留空 = 提供后端报告的所有模型。
pools-field-voices = 语音（每行 lang=voice）
pools-field-offer-voices = 可选语音（每行一个，供用户选择）
pools-field-backends = 后端
pools-no-backends = 尚未定义任何后端。请先在“后端”页面添加一个。
pools-field-gdpr = 符合 GDPR
pools-field-nda = 受 NDA 保护
pools-field-enforce-limits = 强制执行速率限制与配额
pools-save-pool = 保存池
pools-add-pool = 添加池
pools-delete-pool = 删除

pools-error-name-required = 池名称为必填项
pools-error-invalid-kind = 无效的池类型 `{ $kind }`
pools-saved = 已保存池 `{ $name }` — 点击“应用更改”以重新加载
pools-deleted = 已删除池 `{ $name }` — 点击“应用更改”以重新加载
pools-fallback-saved = { $kind } 回退已设置为 `{ $model }`
pools-fallback-cleared = { $kind } 回退已清除

pools-field-allowed-groups = åè®¸çç»
pools-field-allowed-groups-hint = åè®¸æ¥çåä½¿ç¨æ­¤æ± æ¨¡åçç½å³ç»ï¼ç¨éå·åéï¼ãçç©º = ææäººãç®¡çåå§ç»ææéãå¨ ç®¡ç â ç» ä¸­ç®¡çç»ã
