# STATUS: llm-generated, unreviewed — pending native-speaker QA

backends-page-title = 上游后端 — LLM Gateway
backends-heading = 上游后端
backends-description-prefix = 已配置上游池的实时视图——各后端的健康状态、相对于其上限的当前负载，以及每个后端目前提供的模型。仅供查看：路由完全取决于后端通过其
backends-description-suffix = 探测接口报告的信息。
backends-summary = 共 { $total } 个后端 · { $healthy } 个健康 · { $down } 个离线
backends-unknown-fallback-prefix = 未知模型回退 —
backends-empty-prefix = 未配置任何上游池。请在 gateway.toml 中添加
backends-empty-suffix = 块并重启。

backends-fallback-offline-title = fallback_offline：当此池中某已知模型的所有后端都离线时使用
backends-fallback-offline-badge = 离线 ↩ { $model }
backends-pool-empty = 此池中没有后端。

backends-status-down = 离线
backends-status-saturated = 已饱和
backends-status-up = 正常

backends-inflight-label = 处理中 { $load }
backends-activity-summary = 15分钟 { $m15 } · 30分钟 { $m30 } · 60分钟 { $m60 }
backends-no-models = 未提供任何模型
backends-aliases-label = 别名：

backends-alias-target-title = 别名 → { $target }
backends-alias-disabled-label = { $name }（已禁用）
backends-alias-disabled-title = 裸别名已禁用 — 此后端提供多个模型，请为其指定明确的目标（映射表单）
backends-alias-bare-title = 别名 → 此后端的模型
