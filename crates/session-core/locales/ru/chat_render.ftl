# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/chat/render.rs` — the
# gateway-only chat-page chrome: the header model/voice pickers, the
# compliance banners, the composer's "+" tools/integrations/skills menu,
# the "Denken" (effort/thinking) picker, and the share/export/fork
# controls. Prefixed `chat-render-` (rather than `chat-`) to avoid
# colliding with `chat/mod.rs`'s own `chat-*` keys in the sibling
# `chat.ftl`.

chat-render-canvas-toggle-title = Показать / скрыть холст документа
chat-render-canvas-toggle-label = Холст
chat-render-canvas-document-tab = Документ
chat-render-canvas-assets-tab = Файлы
chat-render-canvas-assets-heading = Файлы беседы
chat-render-canvas-assets-count = { $count ->
    [one] { $count } файл
    [few] { $count } файла
    [many] { $count } файлов
   *[other] { $count } файла
}
chat-render-canvas-assets-empty = В эту беседу пока не добавлены файлы.
chat-render-canvas-asset-download = Скачать файл
chat-render-canvas-close-title = Закрыть холст

chat-render-model-placeholder = модель (напр., gpt-4o-mini)
chat-render-model-aria = Модель чата
chat-render-voice-model-aria = Голосовая модель

chat-render-model-non-gdpr = { $id } (не соответствует GDPR)
chat-render-model-confidential = { $id } (ограничение конфиденциальности)
chat-render-model-non-gdpr-confidential = { $id } (не соответствует GDPR, ограничение конфиденциальности)

chat-render-gdpr-banner = Вы отправляете данные модели, не соответствующей GDPR. Не вводите личную информацию (имена, адреса электронной почты, адреса, данные клиентов или сотрудников).
chat-render-nda-banner = Эта модель не покрывается соглашением о конфиденциальности. Не отправляйте материалы, защищённые NDA, или конфиденциальные данные.

chat-render-shared-readonly-banner = Общий чат — только для чтения. Отвечать может только создатель.
chat-render-composer-placeholder = Сообщение модели…

chat-render-new-conversation-fallback = Новая беседа

chat-render-feedback-title = Отправить отзыв

chat-render-effort-title = Уровень размышлений
chat-render-effort-tooltip = Уровень размышлений: выше = больше рассуждений и больше циклов инструментов, но медленнее
chat-render-effort-label-prefix = Размышления:
chat-render-effort-fast = Быстро
chat-render-effort-standard = Стандарт
chat-render-effort-deep = Глубоко
chat-render-effort-max = Максимум

chat-render-tools-tooltip = Инструменты, интеграции и скиллы для этой беседы
chat-render-tools-label = Инструменты
chat-render-tools-search-placeholder = Поиск инструментов…
chat-render-all-tools-label = Все инструменты
chat-render-no-tools-prefix = Для вашей учётной записи пока нет доступных инструментов. Подключите интеграцию в разделе
chat-render-no-tools-suffix = .

chat-render-close = Закрыть

chat-render-group-web-network = Веб и сеть
chat-render-group-attachments-documents = Вложения и документы
chat-render-group-document-templates = Шаблоны документов
chat-render-group-knowledge-base = База знаний
chat-render-group-code-sandbox = Код и песочница
chat-render-group-memory = Память
chat-render-group-integrations = Интеграции
chat-render-group-utility = Утилиты
chat-render-group-skills = Скиллы

chat-render-tool-count = { $count ->
    [one] { $count } инструмент
    [few] { $count } инструмента
   *[many] { $count } инструментов
}

chat-render-active-count-title = Активные инструменты — нажмите для управления
chat-render-unpin-title = Открепить (вернуть в автоматический режим)

chat-render-state-off-tip = Выключено — заблокировано; скрыто от ассистента
chat-render-state-auto-tip = Автоматически — ассистент включает его сам, когда это нужно
chat-render-state-on-tip = Включено — всегда доступно ассистенту

chat-render-share-label-on = Открыт доступ ✓
chat-render-share-label-off = Поделиться
chat-render-share-tooltip = Общие чаты может читать любой авторизованный пользователь, у которого есть ссылка

chat-render-fork-tooltip = Скопировать эту беседу в свои чаты, чтобы продолжить общение
chat-render-fork-label = Продолжить в моих чатах

chat-render-export-tooltip = Скачать эту беседу
chat-render-export-aria = Экспортировать беседу
chat-render-export-label = Экспорт
chat-render-export-pdf = PDF-документ
chat-render-export-md = Markdown (.md)
