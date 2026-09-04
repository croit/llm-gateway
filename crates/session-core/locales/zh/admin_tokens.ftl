# STATUS: llm-generated, unreviewed — pending native-speaker QA

admin-tokens-page-title = API 令牌
admin-tokens-heading = API 令牌
admin-tokens-blurb = 本部署中的所有 API 令牌及其所有者。令牌本身永不显示——数据库中只保存其 SHA-256，因此无法在此恢复。配额在限额页面按令牌设置。模型白名单由两个相互独立的部分组成——所有者在其令牌页面设置的部分，以及你在下方设置的部分——令牌只能使用同时出现在两者中的模型，因此任何一方都只能收紧。
admin-tokens-none = 尚未创建任何 API 令牌。
admin-tokens-count = 共 { $count } 个令牌
admin-tokens-col-name = 令牌
admin-tokens-col-owner = 所有者
admin-tokens-col-state = 状态
admin-tokens-col-dates = 创建 / 使用 / 过期
admin-tokens-col-scope = 模型与配额
admin-tokens-badge-expired = 已过期
admin-tokens-models-summary-all = 模型：全部（无运营方限制）
admin-tokens-models-summary-restricted = 模型：运营方允许 { $count } 个
admin-tokens-models-help = 针对此令牌的运营方限制，与所有者自己的限制相互独立。令牌只能使用同时出现在两个列表中的模型——因此在此勾选不会授予所有者已排除的模型，所有者也无法重新授予你移除的模型。
admin-tokens-models-restrict-label = 将此令牌限制为特定模型
admin-tokens-models-saved-toast = 已设置运营方限制：{ $count } 个模型。
admin-tokens-models-cleared-toast = 已移除运营方限制。
