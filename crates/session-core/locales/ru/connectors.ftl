# STATUS: llm-generated, unreviewed — pending native-speaker QA

connectors-page-title = Коннекторы — LLM Gateway
connectors-heading = Коннекторы
connectors-restore-defaults-button = Восстановить значения по умолчанию
connectors-catalog-intro = Настройте MCP-серверы, к которым пользователи могут подключаться в разделе «Интеграции». Включите коннектор, чтобы сделать его видимым. Коннекторам, которые не могут использовать динамическую регистрацию клиента (например, Google), перед включением нужен OAuth client id/secret для этого развёртывания.
connectors-empty-state = Пока нет коннекторов.

connectors-badge-enabled = Включён
connectors-badge-disabled = Отключён
connectors-badge-default = По умолчанию
connectors-badge-dcr = DCR
connectors-badge-needs-client-id = Нужен client id
connectors-disable-button = Отключить
connectors-enable-disabled-title = Сначала добавьте ниже OAuth client id (Изменить → OAuth client id)
connectors-enable-button = Включить
connectors-delete-confirm = Удалить этот коннектор? Он будет удалён для всех пользователей вместе с их сохранёнными подключениями и токенами. Это действие нельзя отменить.
connectors-delete-button = Удалить
connectors-edit-summary = Изменить

connectors-add-summary = Добавить коннектор

connectors-oauth-help-token-1 = Токен-коннектор: укажите выше URL MCP-сервера; каждый пользователь вставляет свой собственный API-токен в разделе «Интеграции» (отправляется как
connectors-oauth-help-token-2 = ). OAuth-клиент не требуется.

connectors-oauth-help-dcr-heading = Динамическая регистрация клиента — OAuth-клиент не требуется
connectors-oauth-help-dcr-body = Просто укажите выше URL MCP-сервера. Сервер автоматически регистрирует этот шлюз (RFC 7591); каждый пользователь затем нажимает «Подключить» и авторизуется под своей учётной записью — один вход в систему покрывает все сервисы, которые предоставляет сервер.

connectors-oauth-help-gws-1 = Укажите здесь свой
connectors-oauth-help-gws-self-hosted = самостоятельно размещённый MCP-сервер Google Workspace
connectors-oauth-help-gws-2 = (например
connectors-oauth-help-gws-3 = ), работающий в режиме streamable-HTTP — URL заканчивается на
connectors-oauth-help-gws-4 = . Этот сервер хранит OAuth-клиент Google и использует
connectors-oauth-help-gws-ga-apis = GA Google API
connectors-oauth-help-gws-5 = (без developer preview). Разрешите redirect URI этого шлюза на сервере через
connectors-oauth-help-gws-footer = Размещённые Google MCP-эндпоинты (gmailmcp/calendarmcp/drivemcp.googleapis.com) намеренно не используются — они требуют включения организации в программу Workspace Developer Preview. См. docs/connectors.md для инструкций по развёртыванию.

connectors-oauth-help-generic-heading = Настройка OAuth-клиента
connectors-oauth-help-generic-intro = Зарегистрируйте у вашего OAuth-клиента именно этот redirect URI, затем вставьте его client id (и secret) ниже:
connectors-oauth-help-google-1 = Google: создайте
connectors-oauth-help-google-link = OAuth 2.0 Client ID (веб-приложение)
connectors-oauth-help-google-2 = в Google Cloud Console, добавьте указанный выше redirect URI и включите для проекта API Gmail / Google Calendar / Google Drive.
connectors-oauth-help-github-1 = GitHub: создайте
connectors-oauth-help-github-link = OAuth-приложение
connectors-oauth-help-github-2 = (Settings → Developer settings → OAuth Apps), укажите в качестве Authorization callback URL указанный выше redirect URI и скопируйте Client ID и сгенерированный client secret.
connectors-oauth-help-fallback = Создайте у своего провайдера OAuth-клиент с этим redirect URI и указанными ниже authorize-/token-URL.
connectors-oauth-why-1 = Зачем нужен разовый шаг администратора? В OAuth client id идентифицирует
connectors-term-this-gateway = этот шлюз
connectors-oauth-why-2 = как приложение (общее для всех пользователей) — отличается только токен доступа каждого пользователя. Claude Desktop обходится без этого, потому что Anthropic поставляет предварительно зарегистрированные приложения с фиксированным redirect URL; самостоятельно размещённый шлюз использует собственный redirect URI (см. выше), а Google/GitHub не поддерживают автоматическую регистрацию (DCR), как Atlassian, — поэтому вы регистрируетесь один раз, а затем каждый пользователь просто нажимает «Подключить».
connectors-oauth-why-no-app = Совсем нет OAuth-приложения?
connectors-oauth-why-3 = Переключите аутентификацию на «Токен, предоставленный пользователем», и каждый пользователь вставит свой собственный токен (например, персональный токен доступа GitHub) — учётные данные тогда поступают напрямую от пользователя, без клиента администратора.

connectors-field-key-label = Ключ (стабильный id)
connectors-field-key-placeholder = например, gmail
connectors-field-key-readonly-label = Ключ
connectors-field-name-label = Название
connectors-field-name-placeholder = Отображаемое имя
connectors-field-icon-label = Значок (эмодзи)
connectors-field-category-label = Категория
connectors-field-category-placeholder = Google
connectors-field-description-label = Описание
connectors-field-description-placeholder = Что делает этот коннектор
connectors-field-url-label = URL MCP-сервера
connectors-field-auth-label = Аутентификация
connectors-auth-option-oauth = OAuth 2.1 (каждый пользователь авторизуется через провайдера)
connectors-auth-option-token = Токен, предоставленный пользователем (каждый пользователь вставляет свой собственный API-токен)
connectors-auth-option-none = Нет (публичный сервер, без аутентификации)
connectors-field-client-json-label = Вставить JSON OAuth-клиента (необязательно — например, «Download JSON» от Google)
connectors-field-client-json-help = Заполняет client id/secret (а также authorize- и token-URL) из файла. Либо используйте отдельные поля ниже.
connectors-field-client-id-label = OAuth client id
connectors-field-client-id-placeholder = …apps.googleusercontent.com / id OAuth-приложения GitHub
connectors-field-client-id-help-1 = Публичный id, который идентифицирует
connectors-field-client-id-help-2 = как приложение перед провайдером — создаётся один раз администратором на странице учётных данных OAuth провайдера (Google Cloud → Credentials, GitHub → OAuth Apps). Не является секретом отдельного пользователя. Оставьте пустым, если включён DCR.
connectors-field-client-secret-label = OAuth client secret
connectors-secret-placeholder-existing = •••••••• (оставьте пустым, чтобы сохранить текущий)
connectors-secret-placeholder-new = client secret (необязательно)
connectors-field-client-secret-help = Выдаётся вместе с client id на той же странице. Хранится в зашифрованном виде; оставьте пустым, чтобы сохранить текущий.
connectors-field-use-dcr-label = Попробовать динамическую регистрацию клиента (RFC 7591)
connectors-field-scopes-label = Scopes (через пробел)
connectors-advanced-summary = Дополнительно: переопределения discovery
connectors-field-authorize-url-label = Authorize URL
connectors-field-token-url-label = Token URL
connectors-field-registration-url-label = Registration URL
connectors-placeholder-optional-override = необязательное переопределение
connectors-field-required-role-label = Требуемая роль (RBAC-ограничение)
connectors-placeholder-optional = необязательно
connectors-save-changes-button = Сохранить изменения
connectors-add-connector-button = Добавить коннектор

connectors-error-missing-fields = необходимо указать key, name и URL
connectors-error-bad-client-json = не удалось прочитать client_id из вставленного JSON — ожидался файл OAuth-клиента Google ({"{"}"web":{"{"}"client_id":…,"client_secret":…{"}"}{"}"}).
connectors-error-sealing-secret = запечатывание секрета: { $error }
connectors-error-saving = сохранение коннектора: { $error }
connectors-error-needs-client-id = этому коннектору нужен OAuth client id, прежде чем его можно будет включить (он не может использовать динамическую регистрацию). Отредактируйте его и добавьте client id/secret.
connectors-error-toggling = переключение коннектора: { $error }
connectors-error-deleting = удаление коннектора: { $error }
connectors-error-restoring = восстановление значений по умолчанию: { $error }
