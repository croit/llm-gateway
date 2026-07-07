# STATUS: llm-generated, unreviewed — pending native-speaker QA

scheduled-page-title = Запланированные действия — LLM Gateway
scheduled-edit-page-title = Изменить запланированное действие — LLM Gateway

scheduled-heading = Запланированные действия
scheduled-intro = Запускайте промпт автоматически по расписанию. Каждый запуск открывает новый чат, который можно прочитать здесь — выберите модель, напишите промпт и укажите, когда он должен запускаться.
scheduled-create-submit = Создать запланированное действие
scheduled-list-heading = Ваши запланированные действия
scheduled-list-empty = Пока нет запланированных действий. Создайте одно выше.

scheduled-back = Назад
scheduled-edit-heading = Изменить запланированное действие
scheduled-save-submit = Сохранить изменения

scheduled-name-label = Название
scheduled-name-placeholder = напр. Ежедневная сводка новостей
scheduled-model-label = Модель
scheduled-model-placeholder = идентификатор модели (напр. gpt-4o-mini)
scheduled-gdpr-warning = Эта модель не соответствует требованиям GDPR. Запланированные запуски будут автоматически отправлять ей ваш промпт — избегайте персональных данных.
scheduled-nda-warning = Эта модель не защищена соглашением о конфиденциальности. Не планируйте отправку материалов, защищённых NDA или являющихся собственностью компании, в эту модель.
scheduled-prompt-label = Промпт
scheduled-prompt-placeholder = Что модель должна делать при каждом запуске?
scheduled-tools-toggle-label = Разрешить инструменты (веб-поиск, RAG, вложения) — как в чате
scheduled-reuse-toggle-label = Использовать чат предыдущего запуска повторно — каждый запуск продолжает тот же разговор
scheduled-reuse-rounds-prefix = отправлять последние
scheduled-reuse-rounds-aria = Количество раундов истории для повторного воспроизведения
scheduled-reuse-rounds-suffix = раундов

scheduled-builder-heading = Расписание
scheduled-mode-hourly = Ежечасно
scheduled-mode-daily = Ежедневно
scheduled-mode-weekly = Еженедельно
scheduled-mode-monthly = Ежемесячно
scheduled-mode-advanced = Расширенно
scheduled-weekday-mon = Пн
scheduled-weekday-tue = Вт
scheduled-weekday-wed = Ср
scheduled-weekday-thu = Чт
scheduled-weekday-fri = Пт
scheduled-weekday-sat = Сб
scheduled-weekday-sun = Вс
scheduled-on-day-label = В день
scheduled-of-every-month = каждого месяца
scheduled-at-label = В
scheduled-hour-aria = Час
scheduled-minute-aria = Минута
scheduled-of-every-hour = каждого часа
scheduled-timezone-label = Часовой пояс
scheduled-timezone-placeholder = Europe/Berlin
scheduled-cron-label = Cron-выражение
scheduled-cron-help = Пять полей: минута час день-месяца месяц день-недели.

scheduled-no-upcoming-runs = Нет предстоящих запусков.
scheduled-next-runs-prefix = Следующие запуски:{ " " }

scheduled-err-pick-weekday = Выберите хотя бы один день недели.
scheduled-err-enter-cron = Введите cron-выражение.
scheduled-err-unknown-schedule-type = Неизвестный тип расписания «{ $kind }».
scheduled-field-minute = минута
scheduled-field-hour = час
scheduled-field-day-of-month = день месяца
scheduled-err-enter-field = Введите { $field }.
scheduled-err-invalid-field = Неверное значение поля «{ $field }»: { $value }.
scheduled-err-field-range = Значение «{ $field }» должно быть от { $min } до { $max }.
scheduled-err-name-length = Название должно содержать от 1 до 128 символов.
scheduled-err-prompt-length = Промпт должен содержать от 1 до 8000 символов.
scheduled-err-pick-model = Выберите модель.
scheduled-err-unknown-timezone = Неизвестный часовой пояс «{ $tz }».

scheduled-model-non-gdpr = { $model } (не соответствует GDPR)
scheduled-model-nda-restricted = { $model } (ограничение конфиденциальности)
scheduled-model-non-gdpr-nda-restricted = { $model } (не соответствует GDPR, ограничение конфиденциальности)

scheduled-toast-save-failed = Не удалось сохранить расписание.
scheduled-toast-created = Запланированное действие создано.
scheduled-toast-updated = Расписание обновлено.
scheduled-toast-not-found = Такого запланированного действия не существует.
scheduled-toast-update-failed = Не удалось обновить расписание.
scheduled-toast-resumed = Расписание возобновлено.
scheduled-toast-paused = Расписание приостановлено.
scheduled-toast-refresh-failed = Не удалось обновить расписание.
scheduled-toast-deleted = Запланированное действие удалено.
scheduled-toast-already-gone = Уже удалено.
scheduled-toast-delete-failed = Не удалось удалить расписание.

scheduled-badge-active = активно
scheduled-badge-paused = приостановлено
scheduled-status-paused = Приостановлено
scheduled-next-run = Следующий запуск: { $when }
scheduled-no-upcoming-run = Нет предстоящих запусков
scheduled-last-success = Последний: ✓ { $when }
scheduled-last-success-open = Последний: ✓ { $when } — открыть
scheduled-last-failure = Последний: ✗ { $when }
scheduled-last-failure-open = Последний: ✗ { $when } — открыть
scheduled-pause-title = Приостановить
scheduled-resume-title = Возобновить
scheduled-edit-title = Изменить
scheduled-delete-title = Удалить
