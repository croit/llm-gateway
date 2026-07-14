# STATUS: llm-generated, unreviewed — pending native-speaker QA

pools-page-title = Восходящие пулы — LLM Gateway
pools-heading = Восходящие пулы
pools-description = Группируйте бэкенды в пулы по типу и стратегии выбора. Изменения сохраняются в базе данных, но вступают в силу только после нажатия «Применить изменения».

pools-fallbacks-heading = Резервы для неизвестных моделей
pools-fallbacks-description = Когда в запросе указана модель, о которой шлюз никогда не слышал, подставлять эту модель для данного типа. Пусто = промах возвращает 404.

pools-add-heading = Добавить пул
pools-field-name = Название
pools-field-kind = Тип
pools-field-strategy = Стратегия
pools-field-fallback-offline = Резервная офлайн-модель
pools-field-fallback-offline-placeholder = используется, когда все бэкенды недоступны
pools-field-models = Обслуживаемые модели (белый список, через запятую)
pools-field-models-hint = Если задано, от бэкенда с зондированием /models обслуживаются только эти id — остальные показаны зачёркнутыми. Пусто = обслуживать всё, что сообщает бэкенд.
pools-field-voices = Голоса (lang=voice по одному в строке)
pools-field-backends = Бэкенды
pools-no-backends = Бэкенды ещё не заданы. Сначала добавьте один на странице «Бэкенды».
pools-field-gdpr = Соответствует GDPR
pools-field-nda = Покрыто NDA
pools-field-enforce-limits = Применять лимиты запросов и квоты
pools-save-pool = Сохранить пул
pools-add-pool = Добавить пул
pools-delete-pool = Удалить

pools-error-name-required = требуется название пула
pools-error-invalid-kind = недопустимый тип пула `{ $kind }`
pools-saved = пул `{ $name }` сохранён — нажмите «Применить изменения» для перезагрузки
pools-deleted = пул `{ $name }` удалён — нажмите «Применить изменения» для перезагрузки
pools-fallback-saved = резерв для типа { $kind } установлен на `{ $model }`
pools-fallback-cleared = резерв для типа { $kind } очищен
