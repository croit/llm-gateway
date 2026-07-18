# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/upstreams.rs` — объединённая
# страница `/admin/upstreams` (пулы + бэкенды).

upstreams-page-title = Апстримы — LLM Gateway
upstreams-heading = Апстримы
upstreams-description = Пулы группируют бэкенды по виду и стратегии выбора. Здоровье, нагрузка и обслуживаемые модели проверяются в реальном времени. Изменения топологии сохраняются в базе и вступают в силу при применении изменений.

upstreams-add-pool = Пул
upstreams-add-backend = Бэкенд
upstreams-cancel = Отмена
upstreams-edit-pool = Изменить пул
upstreams-edit-backend = Изменить бэкенд
upstreams-delete-confirm = Точно удалить?

upstreams-apply-count = неприменённых изменений
upstreams-apply-note = — реестр времени выполнения всё ещё обслуживает прежнюю топологию.

upstreams-comp-gdpr = GDPR
upstreams-comp-nda = NDA
upstreams-comp-limits = лимиты

upstreams-backend-pending = ожидает

# Подсказка на зачёркнутом чипе модели: обнаружена через /models, но удержана,
# так как список моделей пула (белый список) её не включает.
upstreams-model-withheld-title = Обнаружена через /models, но удержана списком моделей этого пула — не обслуживается и не анонсируется.
# Свёрнутая метка после обслуживаемых моделей: щелчок раскрывает удержанные (неактивные) чипы.
upstreams-models-inactive-pill = +{ $count } неактивных
upstreams-models-inactive-hide = скрыть

upstreams-unassigned-heading = Без назначения
upstreams-unassigned-description = Бэкенды, не назначенные ни одному пулу. Добавьте бэкенд в пул, чтобы направлять на него трафик.

upstreams-empty = Пулы и бэкенды ещё не настроены. Добавьте пул или бэкенд, чтобы начать.
