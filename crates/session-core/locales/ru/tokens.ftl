# STATUS: llm-generated, unreviewed — pending native-speaker QA

tokens-page-title = API-токены — LLM Gateway
tokens-page-heading = API-токены
tokens-intro = Bearer-токены для API, совместимого с OpenAI. Открытый текст показывается только при создании — сохраните его в надёжном месте.

tokens-create-heading = Создать токен
tokens-create-description = Создайте новый Bearer-токен для API, совместимого с OpenAI.
tokens-name-label = Имя
tokens-name-placeholder = напр. laptop, ci-runner
tokens-ttl-label = Срок действия (дней)
tokens-create-submit = Создать токен

tokens-list-heading = Ваши токены
tokens-list-empty = Токенов пока нет. Создайте один выше.

tokens-badge-revoked = отозван
tokens-badge-active = активен
tokens-remove-button = Удалить
tokens-rotate-button = Обновить
tokens-rotate-title = Выпустить новый секрет для этого токена (имя и настройки сохраняются)
tokens-revoke-button = Отозвать

tokens-row-meta = создан { $created } · последнее использование { $last_used } · истекает { $expires }
tokens-last-used-never = никогда

tokens-tool-use-aria = Использование инструментов
tokens-tool-use-label = Использование инструментов
tokens-tool-use-description = Разрешить этому токену вызывать инструменты шлюза (веб-поиск, RAG, …).
tokens-capabilities-summary = Возможности

tokens-mcp-allow-aria = Разрешить MCP-инструменты в режиме «ask» через API
tokens-mcp-allow-label = Разрешить MCP-инструменты «ask» через API
tokens-mcp-allow-description = Инструменты коннектора, требующие подтверждения, не могут запрашивать его через API; включение этой опции запускает их без запроса.

tokens-minted-heading = Токен создан
tokens-minted-copy-warning = Скопируйте значение сейчас — повторно увидеть его будет нельзя.
tokens-copy-aria = Скопировать токен
tokens-copy-title = Скопировать токен
tokens-minted-name = Имя: { $name }

tokens-account-heading = Аккаунт
tokens-signed-in-as = Вы вошли как { $email }
tokens-account-user-id-label = ID пользователя
tokens-account-oidc-label = Роли OIDC
tokens-account-rbac-label = ID ролей RBAC
tokens-roles-none = нет
tokens-roles-none-granted = не предоставлены

tokens-malformed-form = некорректная форма: { $err }
tokens-name-length = Имя токена должно содержать от 1 до 128 символов.
tokens-store-failed = Не удалось сохранить токен.
tokens-created-toast = Токен создан.

tokens-revoked-not-found = Отозванный токен не найден.
tokens-revoked-toast = Токен отозван.
tokens-already-revoked = Токен уже был отозван.
tokens-revoke-failed = Не удалось отозвать токен.

tokens-load-failed = Не удалось загрузить токен.
tokens-not-found-or-revoked = Токен не найден или уже отозван.
tokens-rotated-not-found = Обновлённый токен не найден.
tokens-rotated-toast = Токен обновлён — скопируйте новое значение.
tokens-rotate-failed = Не удалось обновить токен.

tokens-removed-toast = Токен удалён.
tokens-still-active = Токен всё ещё активен — сначала отзовите его.
tokens-remove-failed = Не удалось удалить токен.

tokens-not-found = Токен не найден.
tokens-update-failed = Не удалось обновить токен.
tokens-tool-use-enabled-toast = Использование инструментов включено для этого токена.
tokens-tool-use-disabled-toast = Использование инструментов отключено для этого токена.
tokens-mcp-ask-enabled-toast = MCP-инструменты «ask» через API включены для этого токена.
tokens-mcp-ask-disabled-toast = MCP-инструменты «ask» через API отключены для этого токена.

tokens-unknown-tool = Неизвестный инструмент.
tokens-save-pref-failed = Не удалось сохранить настройку.
tokens-capability-enabled-toast = { $name } включён для этого токена.
tokens-capability-disabled-toast = { $name } отключён для этого токена.

# Web Push "turn complete" opt-in card (rendered by `render_push_card`; wired
# client-side by `ui/ts/push.ts`). Device-local notification settings.
tokens-push-heading = Уведомления
tokens-push-description = Получайте уведомление на этом устройстве, когда начатый вами ответ завершится, пока вы не в приложении.
tokens-push-enable = Включить на этом устройстве
tokens-push-disable = Выключить на этом устройстве
tokens-push-on = Уведомления включены для этого устройства.
tokens-push-off = Уведомления выключены для этого устройства.
tokens-push-denied = Этот браузер заблокировал уведомления. Разрешите их в настройках браузера, чтобы включить.
tokens-push-unsupported = Этот браузер не поддерживает уведомления.
tokens-push-enabled = Уведомления включены на этом устройстве.
tokens-push-disabled = Уведомления выключены на этом устройстве.
tokens-push-error = Не удалось изменить настройки уведомлений.

# Использование, список разрешённых моделей и квота для токена (/tokens).
tokens-usage-line = в этом месяце: { $requests } запросов · { $tokens } токенов · { $cost }
tokens-models-summary-all = Модели: все
tokens-models-summary-restricted = Модели: выбрано { $count }
tokens-models-help = Если выключено, токен следует вашему собственному доступу, включая модели, добавленные позже. Если включено, он может использовать только отмеченные модели — добавленная после этого модель останется недоступной, пока вы не отметите её здесь.
tokens-models-restrict-label = Ограничить этот токен определёнными моделями
tokens-models-none-picked = Отметьте хотя бы одну модель или отключите ограничение.
tokens-models-save = Сохранить модели
tokens-models-saved-toast = Токен ограничен { $count } моделями.
tokens-models-cleared-toast = Токен может использовать все ваши модели.
tokens-limits-summary-none = Квота: нет
tokens-limits-summary-some = Квота: правил — { $count }
tokens-limits-help = Ограничение только для этого токена. Ваш собственный бюджет продолжает действовать, поэтому это может только сузить расход токена, но не расширить его.
tokens-limits-add = Добавить квоту
tokens-limits-remove = Удалить
tokens-limits-saved-toast = Квота токена сохранена.
tokens-limits-removed-toast = Квота токена удалена.
tokens-limits-not-yours = Эту квоту вы удалить не можете.
tokens-limits-admin-set = Эту квоту для токена задал администратор; изменить её можно только на странице лимитов администратора.
tokens-limits-admin-badge = задано администратором
