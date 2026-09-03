# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Административный редактор лимитов запросов / квот (/admin/limits).
limits-heading = Лимиты запросов и квоты
limits-intro = Ограничьте, сколько запросов, токенов или средств может использовать вызывающая сторона в скользящем окне. Правила разрешаются от наиболее конкретного к общему: побеждает собственное правило пользователя, иначе — самое щедрое из его ролей, иначе — глобальное значение по умолчанию. Без правил все безлимитны. Правило для API-токена — это дополнительный потолок, который проверяется вместе с бюджетом владельца, поэтому оно может только сузить расход этого токена. Учитываются только тарифицируемые пулы (самостоятельно размещённые пулы с enforce_limits = false исключены), а весь бюджет пользователя распределяется между его API-токенами, чатом и запланированными запусками.
limits-add-heading = Добавить или обновить лимит
limits-field-subject = Применяется к
limits-field-subject-id = Роль / пользователь / токен
limits-field-subject-id-ph = id роли, email пользователя или id токена
limits-field-model = Модель
limits-field-model-ph = все модели
limits-field-dimension = Лимит
limits-field-window = За
limits-field-value = Значение
limits-add-submit = Сохранить лимит
limits-subject-global = Все (по умолчанию)
limits-subject-role = Роль
limits-subject-user = Пользователь
limits-dim-requests = Запросы
limits-dim-tokens = Токены
limits-dim-cost = Стоимость ({ $cur })
limits-dim-cost-short = Стоимость
limits-win-hour = Час
limits-win-day = День
limits-win-week = Неделя
limits-win-month = Месяц
limits-col-subject = Применяется к
limits-col-scope = Модель
limits-col-limit = Лимит
limits-col-window = Окно
limits-col-value = Значение
limits-col-actions = Действия
limits-none = Лимиты не настроены — все безлимитны.
limits-all-models = все модели
limits-delete = Удалить
limits-saved = лимит сохранён для { $subject }
limits-deleted = лимит удалён
limits-invalid-value = значение `{ $value }` должно быть неотрицательным числом
limits-unknown-role = неизвестная роль `{ $role }`
limits-unknown-user = ни один пользователь не соответствует `{ $user }`
limits-missing-subject-id = введите id роли, email пользователя или id токена
limits-subject-token = API-токен
limits-unknown-token = не найден токен `{ $token }`
