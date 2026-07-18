# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/upstreams.rs` — 合并后的
# `/admin/upstreams` 页面（池 + 后端）。

upstreams-page-title = 上游 — LLM Gateway
upstreams-heading = 上游
upstreams-description = 池按类型和选择策略对后端分组。健康状况、负载和提供的模型均实时探测。拓扑更改会保存到数据库，并在“应用更改”后生效。

upstreams-add-pool = 池
upstreams-add-backend = 后端
upstreams-cancel = 取消
upstreams-edit-pool = 编辑池
upstreams-edit-backend = 编辑后端
upstreams-delete-confirm = 确定删除？

upstreams-apply-count = 项未应用的更改
upstreams-apply-note = —— 运行时注册表仍在提供旧的拓扑。

upstreams-comp-gdpr = GDPR
upstreams-comp-nda = NDA
upstreams-comp-limits = 限额

upstreams-backend-pending = 待应用

# 划掉的模型徽章上的提示：通过 /models 发现，但因该池的模型列表（白名单）未包含而被保留。
upstreams-model-withheld-title = 通过 /models 发现，但被该池的模型列表保留 — 不提供也不公告。
# 已服务模型之后的折叠标签：点击展开被保留（未启用）的模型。
upstreams-models-inactive-pill = +{ $count } 个未启用
upstreams-models-inactive-hide = 收起

upstreams-unassigned-heading = 未分配
upstreams-unassigned-description = 未分配给任何池的后端。将其加入某个池以向其路由流量。

upstreams-empty = 尚未配置任何池或后端。添加一个池或后端即可开始。
