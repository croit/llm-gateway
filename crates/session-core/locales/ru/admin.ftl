# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/admin.rs` — страница
# `/admin/models`.

admin-page-title = Модели — LLM Gateway
admin-heading = Модели
admin-intro-prefix = Настройки по каждой модели — цены, окно контекста, рассуждения, возможности и значения сэмплирования — применяются к
admin-intro-every = каждому
admin-intro-middle = запросу к этой модели от любого пользователя или токена, если только вызывающая сторона не задаст то же значение, которое
admin-intro-always-wins = всегда имеет приоритет
admin-intro-suffix = . Чат-модели, псевдонимы и другие виды — всё в одном списке.
admin-no-models = Пока нет доступных моделей. Как только появится доступный вышестоящий бэкенд, он отобразится здесь.

admin-filter-placeholder = Фильтр моделей…
admin-filter-all = Все
admin-filter-chat = чат
admin-filter-other = другие виды
admin-filter-aliases = псевдонимы
admin-filter-configured = только настроенные

admin-col-model = Модель
admin-col-kind = Вид
admin-col-price = Цена вх/вых
admin-col-context = Контекст
admin-col-reasoning = Рассуждения
admin-col-configured = Настроено

admin-value-default = по умолчанию
admin-value-na = н/д
admin-not-configured = не настроено
admin-alias-inherits = наследует настройки цели
admin-reasoning-auto-resolved = Авто → { $style }

admin-badge-price = ЦЕНА
admin-badge-ctx = КТХ
admin-badge-budget = БЮДЖЕТ
admin-badge-caps = ВОЗМ
admin-badge-toml = TOML

admin-save-model = Сохранить модель
admin-clear-overrides = Очистить все настройки
admin-cancel = Отмена
admin-other-price-note = Сэмплирование, рассуждения и контекст к этому виду не применяются — только цены, для учёта расходов.

admin-toml-placeholder-header = # Общие ключи (vLLM/OpenAI):
admin-toml-defaults-label = Значения сэмплирования (TOML)

admin-reasoning-style-label = Стиль рассуждений
admin-reasoning-style-aria = Стиль рассуждений
admin-reasoning-auto = Авто
admin-reasoning-none = нет
admin-reasoning-qwen = Qwen (vLLM)
admin-reasoning-openai = OpenAI
admin-reasoning-glm = GLM / z.AI
admin-reasoning-anthropic = Anthropic

admin-effort-standard = Стандартный
admin-effort-deep = Глубокий
admin-effort-max = Макс
admin-budget-placeholder = по умолчанию
admin-budget-hint = Максимум токенов на размышления для каждого уровня. Пусто = значение бэкенда по умолчанию (без ограничений). «Fast» отключает рассуждения.
admin-effort-default-option = (по умолчанию)
admin-effort-hint = Уровень усилий для рассуждений по каждому уровню. Пусто = встроенное значение по умолчанию. «Fast» отключает рассуждения.

admin-malformed-form = некорректная форма: { $err }
admin-missing-model-name = отсутствует поле model_name
admin-db-delete-error = ошибка удаления из БД: { $err }
admin-invalid-toml = недопустимый TOML: { $err }
admin-db-upsert-error = ошибка upsert в БД: { $err }
admin-saved-model = `{ $model }` сохранено — вступает в силу немедленно
admin-cleared-defaults = настройки для `{ $model }` очищены
admin-unknown-reasoning-style = неизвестный стиль рассуждений `{ $style }`
admin-db-error = БД: { $err }
admin-budget-not-positive = бюджет `{ $value }` должен быть положительным целым числом
admin-unknown-reasoning-effort = неизвестный уровень усилий рассуждений `{ $value }`
admin-context-window-invalid = окно контекста `{ $value }` должно быть положительным целым

# Цены по каждой модели для учёта расходов (цена за 1 млн токенов, ввод / вывод).
admin-price-label = { $cur }/{ $unit }
admin-price-unit-tokens = 1 млн токенов
admin-price-unit-images = изображение
admin-price-unit-characters = символ
admin-price-unit-seconds = секунда
admin-price-in-label = Цена вх
admin-price-out-label = Цена вых
admin-price-in-placeholder = без цены
admin-price-out-placeholder = без цены
admin-price-invalid = цена `{ $value }` должна быть неотрицательным числом

# Окно контекста (управляет авто-компактизацией).
admin-context-window-full-label = Окно контекста (токены)
admin-context-window-placeholder = по умолч.

admin-alias-chip = псевдоним

# Модели по умолчанию для функций.
admin-defaults-heading = Модели по умолчанию
admin-defaults-intro = Выберите модель, предварительно выбранную для каждой функции. Пусто = первая доступная модель (прежнее поведение).
admin-defaults-chat-label = Чат
admin-defaults-voice-label = Голос (транскрипция)
admin-defaults-image-label = Генерация изображений
admin-defaults-embedding-label = Эмбеддинги (RAG)
admin-defaults-first-option = Первая доступная
admin-defaults-saved = модель по умолчанию установлена: `{ $model }`
admin-defaults-cleared = модель по умолчанию сброшена
admin-defaults-unknown-feature = неизвестная функция `{ $feature }`

# Возможности модели (три состояния) + резервные модели.
admin-capabilities-heading = Возможности
admin-cap-vision = Зрение
admin-cap-tools = Инструменты
admin-cap-structured-output = Структурированный вывод
admin-cap-audio-input = Аудиовход
admin-cap-pdf-input = Ввод PDF
admin-cap-parallel-tools = Параллельные инструменты
admin-cap-unknown = Неизвестно
admin-cap-enabled = Включено
admin-cap-disabled = Отключено
admin-cap-no-fallback = (нет)
admin-cap-fallback-vision = Резерв для зрения
admin-cap-fallback-tools = Резерв для инструментов

# Перезагрузка топологии upstream ("Apply changes" на /admin/upstreams).
admin-reloaded = перезагружено { $pools } pools, { $backends } backends
admin-reload-error = ошибка перезагрузки: { $err }
