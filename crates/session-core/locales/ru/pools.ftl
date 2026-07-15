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

pools-field-allowed-groups = Ð Ð°Ð·ÑÐµÑÑÐ½Ð½ÑÐµ Ð³ÑÑÐ¿Ð¿Ñ
pools-field-allowed-groups-hint = ÐÑÑÐ¿Ð¿Ñ ÑÐ»ÑÐ·Ð° (ÑÐµÑÐµÐ· Ð·Ð°Ð¿ÑÑÑÑ), ÐºÐ¾ÑÐ¾ÑÑÐ¼ ÑÐ°Ð·ÑÐµÑÐµÐ½Ð¾ Ð²Ð¸Ð´ÐµÑÑ Ð¸ Ð¸ÑÐ¿Ð¾Ð»ÑÐ·Ð¾Ð²Ð°ÑÑ Ð¼Ð¾Ð´ÐµÐ»Ð¸ ÑÑÐ¾Ð³Ð¾ Ð¿ÑÐ»Ð°. ÐÑÑÑÐ¾ = Ð²ÑÐµ. ÐÐ´Ð¼Ð¸Ð½Ñ Ð²ÑÐµÐ³Ð´Ð° Ð¸Ð¼ÐµÑÑ Ð´Ð¾ÑÑÑÐ¿. Ð£Ð¿ÑÐ°Ð²Ð»ÐµÐ½Ð¸Ðµ Ð³ÑÑÐ¿Ð¿Ð°Ð¼Ð¸ Ð² Admin â ÐÑÑÐ¿Ð¿Ñ.
