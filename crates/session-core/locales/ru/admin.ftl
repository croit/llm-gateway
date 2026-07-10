# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/admin.rs` — the
# `/admin/models` page for server-wide per-model sampling defaults
# and reasoning-effort/budget overrides.

admin-page-title = Настройки модели по умолчанию — LLM Gateway
admin-heading = Настройки модели по умолчанию
admin-intro-prefix = Общесерверные параметры сэмплирования по умолчанию для этой модели, в формате TOML. Они применяются к
admin-intro-every = каждому
admin-intro-middle = запросу к этой модели от любого пользователя или токена — если только вызывающая сторона не задаст тот же ключ в своём собственном запросе, который
admin-intro-always-wins = всегда имеет приоритет
admin-intro-suffix = . Считайте это минимальным уровнем, который получает каждый, если не указывает собственные значения. Пусто = значений по умолчанию нет, действует встроенное поведение бэкенда.
admin-no-models = Пока нет доступных чат-моделей. Как только появится доступный вышестоящий бэкенд, он отобразится здесь.

admin-toml-placeholder-header = # Общие ключи (vLLM/OpenAI):
admin-toml-defaults-label = Значения TOML по умолчанию
admin-save = Сохранить

admin-reasoning-style-aria = Стиль рассуждений
admin-reasoning-auto = Рассуждения: Авто
admin-reasoning-none = Рассуждения: нет
admin-reasoning-qwen = Рассуждения: Qwen (vLLM)
admin-reasoning-openai = Рассуждения: OpenAI
admin-reasoning-glm = Рассуждения: GLM / z.AI
admin-reasoning-anthropic = Рассуждения: Anthropic

admin-effort-standard = Стандартный
admin-effort-deep = Глубокий
admin-effort-max = Макс
admin-budget-placeholder = по умолчанию
admin-budget-hint = Максимум токенов на размышления для каждого уровня. Пусто = значение бэкенда по умолчанию (без ограничений). «Fast» отключает рассуждения.
admin-effort-default-option = (по умолчанию)
admin-effort-hint = Уровень усилий для рассуждений по каждому уровню. Пусто = встроенное значение по умолчанию. «Fast» отключает рассуждения.
admin-save-reasoning-budget = Сохранить бюджет рассуждений

admin-malformed-form = некорректная форма: { $err }
admin-missing-model-name = отсутствует поле model_name
admin-db-delete-error = ошибка удаления из БД: { $err }
admin-cleared-defaults = значения по умолчанию для `{ $model }` очищены
admin-invalid-toml = недопустимый TOML: { $err }
admin-db-upsert-error = ошибка upsert в БД: { $err }
admin-saved-defaults = значения по умолчанию для `{ $model }` сохранены
admin-unknown-reasoning-style = неизвестный стиль рассуждений `{ $style }`
admin-db-error = БД: { $err }
admin-saved-reasoning-style = стиль рассуждений для `{ $model }` сохранён
admin-budget-not-positive = бюджет `{ $value }` должен быть положительным целым числом
admin-unknown-reasoning-effort = неизвестный уровень усилий рассуждений `{ $value }`
admin-saved-reasoning-budget = бюджет рассуждений для `{ $model }` сохранён

admin-context-window-label = Контекст
admin-context-window-unit = ток.
admin-context-window-placeholder = по умолч.
admin-context-window-aria = Окно контекста (токены)
admin-context-window-invalid = окно контекста `{ $value }` должно быть положительным целым
admin-context-window-saved = окно контекста задано для `{ $model }`
admin-context-window-cleared = окно контекста очищено для `{ $model }`

# Цены по каждой модели для учёта расходов (цена за 1 млн токенов, ввод / вывод).
admin-price-label = Цена ({ $cur })
admin-price-in-placeholder = вх
admin-price-out-placeholder = вых
admin-price-in-aria = Цена ввода за 1 млн токенов
admin-price-out-aria = Цена вывода за 1 млн токенов
admin-price-unit = /1M
admin-price-invalid = цена `{ $value }` должна быть неотрицательным числом
admin-price-saved = цены заданы для `{ $model }`

# Модели по умолчанию для функций (предварительно выбираются в списках
# чата/голоса и как резерв API, когда вызов не указывает модель).
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
admin-other-heading = Другие модели (цены)
admin-other-intro = Модели эмбеддингов, изображений, синтеза речи и транскрипции. Настройки сэмплирования и рассуждений не применяются, но задайте цены за 1 млн токенов, чтобы их использование учитывалось в стоимости и лимитах стоимости.

# Карточка псевдонимов: имена моделей, являющиеся псевдонимами другой (реальной) модели.
admin-aliases-heading = Псевдонимы
admin-aliases-intro = Эти имена — псевдонимы другой модели. У них нет собственных настроек или цены: каждый запрос настраивается и учитывается как модель, в которую он разрешается.
admin-alias-chip = псевдоним
