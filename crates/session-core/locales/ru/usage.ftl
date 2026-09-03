# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/usage.rs` — the
# per-user `/usage` usage-statistics page and its admin-only "all
# users" scope toggle.

usage-title-all = Использование — все пользователи — LLM Gateway
usage-title-mine = Ваше использование — LLM Gateway

usage-heading-all = Использование — все пользователи
usage-heading-mine = Ваше использование
usage-blurb-all = Объём запросов и использование токенов по пользователям и по бэкендам для всех способов доступа. «Запросы» считает обращения к бэкенду, поэтому обмен с использованием инструментов (несколько раундов) засчитывается как более одного.
usage-blurb-mine = Ваш объём запросов и использование токенов в чате, API и запланированных действиях. «Запросы» считает обращения к бэкенду, поэтому обмен с использованием инструментов засчитывается как более одного.

usage-metrics-disabled-prefix = Сбор метрик использования отключён (
usage-metrics-disabled-suffix = ). Приведённые ниже цифры отражают только данные, записанные до отключения.

usage-toggle-mine = Моё
usage-toggle-all = Все пользователи

usage-source-all = Все источники
usage-source-api = API (/v1)
usage-source-chat = Чат
usage-source-scheduled = Запланировано
usage-backend-all = Все бэкенды

usage-filter-period = Период
usage-filter-source = Источник
usage-filter-backend = Бэкенд
usage-apply = Применить

usage-stat-requests-title = Запросы
usage-stat-requests-desc = обращения к бэкенду
usage-stat-tokens-title = Токены
usage-stat-tokens-desc = запрос + ответ
usage-stat-cost-title = Стоимость
usage-stat-cost-desc = по настроенным ценам моделей
usage-stat-users-title = Пользователи
usage-stat-users-desc = активны за период
usage-stat-errors-title = Ошибки
usage-stat-errors-desc = статус ≥ 400

usage-table-by-user = По пользователю
usage-table-by-backend = По бэкенду
usage-table-by-source = По источнику
usage-table-by-model = По модели

usage-key-user = Пользователь
usage-key-backend = Бэкенд
usage-key-source = Источник
usage-key-model = Модель

usage-col-requests = Запросы
usage-col-tokens = Токены
usage-col-cost = Стоимость
usage-col-errors = Ошибки

usage-no-activity = Нет активности за этот период.

usage-limits-heading = Ваши лимиты
usage-limit-used = использовано { $percent } %
usage-limit-refreshes = обновится { $time }
usage-unpriced-warning = Расходы не включают модели без цены: { $models }. Задайте цены в /admin/models, чтобы учитывать их.

usage-table-by-token = По API-токену
usage-key-token = API-токен
usage-token-none = Чат и запланированные запуски (без токена)
usage-token-all = Все токены
usage-filter-token = Токен
