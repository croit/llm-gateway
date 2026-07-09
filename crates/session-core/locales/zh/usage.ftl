# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/usage.rs` — the
# per-user `/usage` usage-statistics page and its admin-only "all
# users" scope toggle.

usage-title-all = 用量 — 所有用户 — LLM Gateway
usage-title-mine = 我的用量 — LLM Gateway

usage-heading-all = 用量 — 所有用户
usage-heading-mine = 我的用量
usage-blurb-all = 按用户和按后端统计的所有访问方式的请求量与令牌用量。“请求”统计的是对上游后端的调用次数，因此一次使用工具的对话轮次（会产生多次往返）计为不止一次请求。
usage-blurb-mine = 你在聊天界面、API 和计划任务中的请求量与令牌用量。“请求”统计的是对上游后端的调用次数，因此一次使用工具的对话轮次计为不止一次请求。

usage-metrics-disabled-prefix = 用量统计已禁用（
usage-metrics-disabled-suffix = ）。以下数字仅反映禁用前记录的数据。

usage-toggle-mine = 我的
usage-toggle-all = 所有用户

usage-source-all = 所有来源
usage-source-api = API (/v1)
usage-source-chat = 聊天界面
usage-source-scheduled = 计划任务
usage-backend-all = 所有后端

usage-filter-period = 时间段
usage-filter-source = 来源
usage-filter-backend = 后端
usage-apply = 应用

usage-stat-requests-title = 请求数
usage-stat-requests-desc = 对上游后端的调用
usage-stat-tokens-title = 令牌数
usage-stat-tokens-desc = 提示 + 补全
usage-stat-cost-title = 成本
usage-stat-cost-desc = 按配置的模型价格计算
usage-stat-users-title = 用户数
usage-stat-users-desc = 该时间段内活跃
usage-stat-errors-title = 错误数
usage-stat-errors-desc = 状态码 ≥ 400

usage-table-by-user = 按用户
usage-table-by-backend = 按后端
usage-table-by-source = 按来源
usage-table-by-model = 按模型

usage-key-user = 用户
usage-key-backend = 后端
usage-key-source = 来源
usage-key-model = 模型

usage-col-requests = 请求数
usage-col-tokens = 令牌数
usage-col-cost = 成本
usage-col-errors = 错误数

usage-no-activity = 此时间段内无活动。

usage-limits-heading = 你的限额
usage-limit-used = 已使用 { $percent }%
usage-limit-refreshes = { $time } 刷新
usage-unpriced-warning = 支出不包含未定价的模型：{ $models }。请在 /admin/models 中设置价格以计入。
