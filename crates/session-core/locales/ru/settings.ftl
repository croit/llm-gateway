# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Настройки оператора (/admin/settings). Заголовки карточек (settings-s-*),
# подписи полей (settings-f-*) и их подсказки (settings-f-*-help) выводятся
# из записей в gateway_core::server::settings::SECTIONS:
# `sandbox.runner_url` -> `settings-f-sandbox-runner_url`.
# Источник — locales/en/settings.ftl.

settings-heading = Настройки
settings-intro = Эксплуатационные настройки этого шлюза. Они хранятся в базе данных, файл конфигурации не нужен — рядом с каждым полем показан ключ TOML, который оно заменяет.
settings-save = Сохранить раздел
settings-saved = Сохранено. Действует со следующего запроса.
settings-saved-restart = Сохранено. Некоторые поля этого раздела вступят в силу только после перезапуска.
settings-save-failed = Не удалось сохранить эти настройки.
settings-cleared = Сброшено. Снова действует значение по умолчанию.
settings-restart-badge = перезапуск
settings-restart-note = Поля с пометкой «перезапуск» читаются только при старте; чтобы изменения подействовали, нужен перезапуск.
settings-secret-set = сохранено — введите новое значение, чтобы заменить
settings-secret-unset = не задано
settings-secret-clear = Очистить

settings-no-backend-heading = Бэкенд моделей ещё не добавлен
settings-no-backend-body = Вход настроен, но шлюз не выдаёт ни одной модели, пока не добавлен бэкенд. До этого чат и API /v1 будут отклонять запросы.
settings-no-backend-cta = Добавить бэкенд в /admin/upstreams →

settings-tab-chat = Чат
settings-tab-tools = Инструменты
settings-tab-data = Контент и данные
settings-tab-access = Доступ и использование
settings-tab-notifications = Уведомления
settings-show-fields = Показать ещё { $count } настроек
settings-model-automatic = Автоматически — использовать первую доступную модель
settings-model-none-configured = Модель такого типа ещё не настроена. Добавьте соответствующий пул в /admin/upstreams, и она появится здесь.
settings-model-unavailable = { $model } (настроена, но сейчас недоступна)
settings-restart-pending-heading = Требуется перезапуск
settings-restart-pending-body = Эти настройки сохранены, но вступят в силу только после перезапуска шлюза:

# ─── Карточки разделов ───────────────────────────────────────────────────────

settings-s-chat-ocr = OCR документов
settings-s-chat-ocr-blurb = Превращение загруженных PDF и изображений в текст, который может прочитать модель.
settings-s-chat-compaction = Сжатие переписки
settings-s-chat-compaction-blurb = Пересказ более старой половины длинного разговора, чтобы он и дальше помещался в контекстное окно модели.
settings-s-chat-s3 = Хранилище вложений (S3)
settings-s-chat-s3-blurb = Объектное хранилище для вложений чата. Без него загрузка файлов отклоняется.
settings-s-sandbox = Песочница для кода
settings-s-sandbox-blurb = Изолированный исполнитель, который запускает написанный моделью код.
settings-s-comfyui = ComfyUI: изображения и видео
settings-s-comfyui-blurb = Headless-воркер ComfyUI за инструментами работы с изображениями и видео.
settings-s-rag = Индексация RAG
settings-s-rag-blurb = Где хранятся проиндексированные источники и насколько интенсивно работает индексатор.
settings-s-skills = Навыки
settings-s-skills-blurb = Каталог бандлов на диске за страницей /admin/skills.
settings-s-typst = Шаблоны Typst
settings-s-typst-blurb = Шаблоны за экспортом в PDF и инструментами работы с документами.
settings-s-geoip = GeoIP
settings-s-geoip-blurb = Приблизительное местоположение клиента для инструмента get_user_location.
settings-s-usage = Метрики использования
settings-s-usage-blurb = Учёт по каждому запросу за страницей /usage.
settings-s-limits = Ограничения и квоты
settings-s-limits-blurb = Главный переключатель правил, настроенных на /admin/limits.
settings-s-feedback = Виджет обратной связи
settings-s-feedback-blurb = Куда встроенный виджет обратной связи создаёт задачи.
settings-s-push = Web Push
settings-s-push-blurb = Уведомления о завершении ответа. Пара ключей создаётся и сохраняется автоматически.
settings-s-gateway = Сессии и токены
settings-s-gateway-blurb = Как долго действуют вход через браузер и токен API, и могут ли администраторы работать от имени другого пользователя.

# ─── Поля ────────────────────────────────────────────────────────────────────

settings-f-chat-ocr-enabled = Включить OCR
settings-f-chat-ocr-enabled-help = Главный переключатель извлечения текста из загруженных документов.
settings-f-chat-ocr-model = Модель OCR
settings-f-chat-ocr-model-help = Какая модель читает страницы. Её должен обслуживать пул вида ocr; в автоматическом режиме берётся первая доступная.
settings-f-chat-ocr-max_tokens = Бюджет токенов на запрос
settings-f-chat-ocr-max_tokens-help = Бюджет токенов для одного запроса OCR.
settings-f-chat-ocr-ngram_window = Окно перекрытия
settings-f-chat-ocr-ngram_window-help = Перекрытие, по которому тексты страниц сшиваются без повторов.
settings-f-chat-ocr-max_bytes = Максимальный размер документа
settings-f-chat-ocr-max_bytes-help = Наибольший принимаемый документ, в байтах.
settings-f-chat-ocr-max_pages = Максимум страниц
settings-f-chat-ocr-max_pages-help = Сколько страниц максимум читается из одного документа.
settings-f-chat-ocr-dpi = Разрешение растеризации
settings-f-chat-ocr-dpi-help = Разрешение, с которым страницы PDF рендерятся перед чтением, в DPI.
settings-f-chat-ocr-max_output_chars = Максимум извлечённого текста
settings-f-chat-ocr-max_output_chars-help = Предел объёма текста, извлекаемого из одного документа, в символах.
settings-f-chat-ocr-timeout_secs = Таймаут
settings-f-chat-ocr-timeout_secs-help = Срок обработки одного документа, в секундах.
settings-f-chat-ocr-max_concurrency = Страниц параллельно
settings-f-chat-ocr-max_concurrency-help = Сколько страниц читается одновременно.
settings-f-chat-ocr-auto_min_text_chars_per_page = Порог распознавания сканов
settings-f-chat-ocr-auto_min_text_chars_per_page-help = Если встроенного текста на странице меньше, PDF считается сканом и отправляется в OCR.

settings-f-chat-compaction-enabled = Включить сжатие
settings-f-chat-compaction-enabled-help = Главный переключатель пересказа длинных разговоров.
settings-f-chat-compaction-default_context_window = Предполагаемое контекстное окно
settings-f-chat-compaction-default_context_window-help = Контекстное окно в токенах, предполагаемое для модели, которая его не сообщает.
settings-f-chat-compaction-trigger_ratio = Порог срабатывания
settings-f-chat-compaction-trigger_ratio-help = Доля контекстного окна, при которой запускается сжатие (0,7 = при заполнении 70 %).
settings-f-chat-compaction-keep_recent_turns = Сохраняемые последние реплики
settings-f-chat-compaction-keep_recent_turns-help = Сколько реплик в конце разговора остаётся дословно.
settings-f-chat-compaction-min_turns_to_compact = Минимальная длина разговора
settings-f-chat-compaction-min_turns_to_compact-help = Разговоры короче этого числа реплик не сжимаются никогда.
settings-f-chat-compaction-summary_max_tokens = Бюджет токенов пересказа
settings-f-chat-compaction-summary_max_tokens-help = Бюджет токенов для пересказа, который заменяет сжатые реплики.

settings-f-chat-s3-enabled = Хранить вложения в S3
settings-f-chat-s3-enabled-help = Выключено — вложения в чате недоступны.
settings-f-chat-s3-endpoint = URL точки доступа
settings-f-chat-s3-endpoint-help = Например https://s3.eu-central-1.amazonaws.com или адрес MinIO.
settings-f-chat-s3-region = Регион
settings-f-chat-s3-region-help = Название региона.
settings-f-chat-s3-bucket = Бакет
settings-f-chat-s3-bucket-help = Бакет, в котором лежат вложения.
settings-f-chat-s3-key_prefix = Префикс ключа
settings-f-chat-s3-key_prefix-help = Префикс, под которым пишется каждый ключ объекта.
settings-f-chat-s3-access_key = Идентификатор ключа доступа
settings-f-chat-s3-access_key-help = Идентификатор ключа доступа, с которым выполняется обращение к бакету.
settings-f-chat-s3-secret_key = Секретный ключ доступа
settings-f-chat-s3-secret_key-help = Секретная половина этого ключа доступа. Хранится в зашифрованном виде.

settings-f-sandbox-enabled = Включить инструменты песочницы
settings-f-sandbox-enabled-help = Регистрирует инструменты, позволяющие модели выполнять код.
settings-f-sandbox-runner_url = URL исполнителя
settings-f-sandbox-runner_url-help = Базовый URL службы sandbox-runner. Она выполняет произвольный код, поэтому должна быть доступна только со шлюза.
settings-f-sandbox-timeout_secs = Таймаут
settings-f-sandbox-timeout_secs-help = HTTP-срок одного запуска, в секундах.
settings-f-sandbox-max_artifact_bytes = Максимальный размер артефакта
settings-f-sandbox-max_artifact_bytes-help = Наибольший отдельный файл, принимаемый обратно из запуска, в байтах.

settings-f-comfyui-enabled = Включить инструменты изображений и видео
settings-f-comfyui-enabled-help = Регистрирует инструменты comfyui_*.
settings-f-comfyui-base_url = URL ComfyUI
settings-f-comfyui-base_url-help = Базовый URL экземпляра ComfyUI. Аутентификации у него нет, поэтому он должен быть доступен только со шлюза.
settings-f-comfyui-content_dir = Каталог рабочих процессов
settings-f-comfyui-content_dir-help = Содержит по одному подкаталогу на рабочий процесс. Кнопка перезагрузки на /admin/comfyui перечитывает его без перезапуска.
settings-f-comfyui-timeout_secs = Таймаут
settings-f-comfyui-timeout_secs-help = Срок одного запуска рабочего процесса, в секундах.
settings-f-comfyui-queue_poll_interval_ms = Интервал опроса очереди
settings-f-comfyui-queue_poll_interval_ms-help = Как часто шлюз спрашивает ComfyUI о выполняющемся задании, в миллисекундах.
settings-f-comfyui-max_concurrent_jobs = Одновременные задания
settings-f-comfyui-max_concurrent_jobs-help = Сколько рабочих процессов модель может выполнять одновременно.

settings-f-rag-enabled = Запускать индексатор
settings-f-rag-enabled-help = Главный переключатель индексации и поиска RAG.
settings-f-rag-data_dir = Каталог индексов
settings-f-rag-data_dir-help = Где хранятся индексы. Должен находиться на постоянном томе, иначе каждый перезапуск переиндексирует всё. Существующие индексы за ним не переезжают — укажите новое место, и всё будет проиндексировано заново.
settings-f-rag-clone_concurrency = Параллельные задания индексации
settings-f-rag-clone_concurrency-help = Сколько git-клонов и заданий индексации выполняется одновременно.

settings-f-skills-enabled = Загружать бандлы навыков
settings-f-skills-enabled-help = Главный переключатель навыков, которыми управляет /admin/skills.
settings-f-skills-dir = Каталог бандлов
settings-f-skills-dir-help = Каталог, в котором лежат бандлы навыков.

settings-f-typst-enabled = Загружать шаблоны Typst
settings-f-typst-enabled-help = Главный переключатель экспорта в PDF и инструментов работы с документами.
settings-f-typst-templates_dir = Каталог шаблонов
settings-f-typst-templates_dir-help = Каталог с шаблонами. Перечитывается при сохранении, так что добавление шаблона не требует перезапуска.

settings-f-geoip-enabled = Включить запросы GeoIP
settings-f-geoip-enabled-help = Главный переключатель инструмента get_user_location.
settings-f-geoip-db_path = Файл базы данных
settings-f-geoip-db_path-help = Путь к базе IP2Location в формате BIN.
settings-f-geoip-update_token = Токен загрузки
settings-f-geoip-update_token-help = Токен IP2Location для обновления базы. Хранится в зашифрованном виде.

settings-f-usage-enabled = Записывать использование
settings-f-usage-enabled-help = Учёт по каждому запросу за страницей /usage.
settings-f-usage-retention_days = Срок хранения
settings-f-usage-retention_days-help = Сколько дней хранятся записи.
settings-f-usage-currency = Валюта
settings-f-usage-currency-help = Валюта, в которой показываются расходы.

settings-f-limits-enabled = Применять ограничения и квоты
settings-f-limits-enabled-help = Выключено — правила на /admin/limits игнорируются.

settings-f-feedback-enabled = Показывать виджет обратной связи
settings-f-feedback-enabled-help = Главный переключатель кнопки обратной связи в приложении.
settings-f-feedback-github_owner = Владелец репозитория
settings-f-feedback-github_owner-help = Пользователь или организация GitHub, которой принадлежит трекер задач.
settings-f-feedback-github_repo = Репозиторий
settings-f-feedback-github_repo-help = Название репозитория, в котором создаются задачи.
settings-f-feedback-github_token = Токен GitHub
settings-f-feedback-github_token-help = Нужны права issues:write, а для скриншотов ещё contents:write. Хранится в зашифрованном виде.
settings-f-feedback-github_api_base = Базовый URL API
settings-f-feedback-github_api_base-help = Базовый URL REST API. Меняется для GitHub Enterprise.
settings-f-feedback-labels = Метки задач
settings-f-feedback-labels-help = Метки, которые проставляются каждой создаваемой задаче.
settings-f-feedback-assets_branch = Ветка для скриншотов
settings-f-feedback-assets_branch-help = Осиротевшая ветка, в которую коммитятся скриншоты.
settings-f-feedback-extraction_model = Модель разбора
settings-f-feedback-extraction_model-help = Чат-модель, которая превращает голосовую заметку в поля формы.
settings-f-feedback-voice_model = Модель транскрипции
settings-f-feedback-voice_model-help = Модель, которая превращает голосовую заметку в текст.

settings-f-push-enabled = Отправлять push-уведомления
settings-f-push-enabled-help = Поднимает push-эндпоинты и уведомляет о завершении ответа.
settings-f-push-contact = Контакт оператора
settings-f-push-contact-help = URI вида mailto: или https:, по которому push-служба может с вами связаться.

settings-f-gateway-token_ttl_days = Срок жизни токенов API
settings-f-gateway-token_ttl_days-help = Сколько дней действует только что созданный токен gwk_….
settings-f-gateway-session_ttl_days = Таймаут простоя сессии
settings-f-gateway-session_ttl_days-help = Скользящий таймаут простоя для входа через браузер, в днях: каждый запрос сдвигает его вперёд, так что это срок, на который можно отсутствовать, не входя заново.
settings-f-gateway-session_absolute_max_days = Максимальный возраст сессии
settings-f-gateway-session_absolute_max_days-help = Жёсткий предел в днях с момента входа, который не продлевает никакая активность. Он также заставляет периодически проходить через провайдера идентификации — единственный момент, когда групповые claim'ы перечитываются.
settings-f-gateway-allow_impersonation = Разрешить работу от имени пользователя
settings-f-gateway-allow_impersonation-help = Позволяет администраторам действовать от имени другого пользователя для отладки. Каждый такой сеанс протоколируется и показывает постоянный баннер; при выключении кнопки скрыты, а эндпоинт отказывает.
