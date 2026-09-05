# STATUS: llm-generated, unreviewed — pending native-speaker QA

integrations-page-title = Интеграции — LLM Gateway
integrations-heading = Интеграции
integrations-intro = Подключите свои собственные учётные записи, чтобы ассистент мог действовать от вашего имени — читать вашу почту, календарь, файлы, репозитории и многое другое. Каждое подключение использует ваши собственные права доступа и может быть отключено в любой момент.
integrations-empty = Пока нет доступных коннекторов. Администратор может включить их в разделе Админ → Коннекторы.

integrations-badge-connected = Подключено
integrations-badge-needs-reconnect = Требуется переподключение
integrations-badge-needs-admin-setup = Требуется настройка администратором

integrations-reconnect-title = Восстановить соединение (повторная авторизация / повтор)
integrations-reconnect-button = Переподключить
integrations-disconnect-button = Отключить
integrations-disconnect-confirm = Отключить эту интеграцию? Сохранённый токен доступа будет удалён.
integrations-connect-button = Подключить

integrations-token-label = Ваш API-токен
integrations-token-placeholder = вставьте ваш токен

integrations-tools-error-prefix = Не удалось загрузить инструменты этого коннектора:
integrations-tools-error-hint = Проверьте URL MCP-сервера / ваш токен, затем используйте «Переподключить» выше.
integrations-tools-error-hint-reauth = Ваша авторизация больше не действует — нажмите «Переподключить» выше и войдите заново.
integrations-tools-empty = Этот коннектор не предоставляет инструментов.
integrations-tools-header = Права на инструменты ({ $count })
integrations-set-all-label = Установить все:
integrations-mode-always = Всегда
integrations-mode-ask = Спрашивать
integrations-mode-off = Выключено
integrations-tools-toggle = Показать / скрыть отдельные инструменты
integrations-tool-kind-read = чтение
integrations-tool-kind-write = запись

integrations-error-unknown-connector = неизвестный или отключённый коннектор
integrations-error-forbidden-role = у вас нет доступа к этому коннектору
integrations-error-not-oauth = этот коннектор не использует OAuth
integrations-error-oauth-discovery-failed = не удалось обнаружить параметры OAuth: { $error }
integrations-error-needs-setup-no-client = этот коннектор требует настройки: не настроен id клиента, а провайдер не предлагает динамическую регистрацию. Попросите администратора добавить клиент OAuth.
integrations-error-sealing-client-secret = запечатывание секрета клиента: { $error }
integrations-error-dcr-failed = динамическая регистрация клиента не удалась: { $error }
integrations-error-needs-setup-admin = этот коннектор требует настройки: администратор должен настроить id клиента OAuth.
integrations-error-building-authorize-url = построение URL авторизации: { $error }
integrations-error-persisting-authorization = сохранение авторизации: { $error }
integrations-error-provider-error = провайдер вернул ошибку: { $error } { $desc }
integrations-error-callback-missing = в обратном вызове отсутствует код или состояние
integrations-error-auth-expired = срок действия этой авторизации истёк или она уже использована — начните заново со страницы «Интеграции»
integrations-error-loading-authorization = загрузка авторизации: { $error }
integrations-error-state-mismatch = состояние авторизации не совпало с вашей сессией
integrations-error-connector-missing = коннектор больше не существует
integrations-error-decrypting-client-secret = расшифровка секрета клиента: { $error }
integrations-error-connector-missing-client-id = у коннектора отсутствует id клиента OAuth
integrations-error-sealing-access-token = запечатывание токена доступа: { $error }
integrations-error-sealing-refresh-token = запечатывание токена обновления: { $error }
integrations-error-saving-connection = сохранение подключения: { $error }
integrations-error-not-token-based = этот коннектор не основан на токене
integrations-error-token-required = требуется токен
integrations-error-sealing-token = запечатывание токена: { $error }
integrations-error-unknown-connector-plain = неизвестный коннектор
integrations-error-invalid-mode = недопустимый режим доступа
integrations-error-saving-tool-permission = сохранение прав на инструмент: { $error }
integrations-error-saving-permissions = сохранение прав доступа: { $error }
integrations-error-listing-tools = получение списка инструментов: { $error }
integrations-error-disconnecting = отключение: { $error }
integrations-error-connection-unavailable = соединение недоступно
