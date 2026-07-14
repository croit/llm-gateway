# STATUS: llm-generated, unreviewed — pending native-speaker QA

backends-page-title = Восходящие бэкенды — LLM Gateway
backends-heading = Восходящие бэкенды
backends-description-prefix = Живой обзор настроенных восходящих пулов — состояние, текущая нагрузка относительно лимита каждого бэкенда и модели, которые каждый из них сейчас предоставляет. Только для чтения: маршрутизация полностью зависит от того, что бэкенды сообщают через свой
backends-description-suffix = проверочный запрос.
backends-summary = { $total } бэкендов · { $healthy } исправны · { $down } недоступны
backends-unknown-fallback-prefix = Резерв для неизвестной модели —
backends-empty-prefix = Восходящие пулы не настроены. Добавьте блок
backends-empty-suffix = в gateway.toml и перезапустите.

backends-fallback-offline-title = fallback_offline: используется, когда все бэкенды известной модели в этом пуле недоступны
backends-fallback-offline-badge = офлайн ↩ { $model }
backends-pool-empty = В этом пуле нет бэкендов.

backends-status-down = недоступен
backends-status-saturated = перегружен
backends-status-up = активен

backends-inflight-label = в обработке { $load }
backends-activity-summary = 15м { $m15 } · 30м { $m30 } · 60м { $m60 }
backends-no-models = модели не заявлены
backends-aliases-label = алиасы:

backends-alias-target-title = алиас → { $target }
backends-alias-disabled-label = { $name } (отключён)
backends-alias-disabled-title = простой алиас отключён — этот бэкенд обслуживает несколько моделей; укажите явную цель (форма сопоставления)
backends-alias-bare-title = алиас → модель этого бэкенда

# Backend CRUD editor (add/edit/delete backends stored in the DB topology).
backends-manage-heading = Управление бэкендами
backends-manage-description = Добавляйте, редактируйте или удаляйте восходящие бэкенды. Изменения сохраняются в базе данных, но вступают в силу только после нажатия «Применить изменения».
backends-apply-changes = Применить изменения
backends-add-heading = Добавить бэкенд
backends-field-name = Название
backends-field-base-url = Базовый URL
backends-field-api-key-env = Переменная окружения с API-ключом
backends-field-health-path = Путь проверки состояния
backends-field-weight = Вес
backends-field-max-inflight = Макс. одновременных
backends-field-pool = Пул
backends-field-pool-none = (нет)
backends-field-pool-hint = Назначает этот бэкенд одному пулу. Бэкенд в нескольких пулах сводится к выбранному здесь.
backends-field-models = Модели (через запятую)
backends-field-aliases = Алиасы (name=target по одному в строке)
backends-field-probe-models = Определять модели через проверочный запрос /models
backends-field-supports-edit = Поддерживает редактирование изображений
backends-save-backend = Сохранить бэкенд
backends-add-backend = Добавить бэкенд
backends-delete-backend = Удалить
backends-error-name-required = требуется название бэкенда
backends-error-base-url-required = требуется базовый URL
backends-saved = бэкенд `{ $name }` сохранён — нажмите «Применить изменения» для перезагрузки
backends-deleted = бэкенд `{ $name }` удалён — нажмите «Применить изменения» для перезагрузки

backends-field-api-key = Ключ API
backends-field-api-key-placeholder = Ключ API (хранится в зашифрованном виде)
backends-field-api-key-keep = оставьте пустым, чтобы сохранить текущий ключ
