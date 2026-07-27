# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/chat/mod.rs` — the
# multi-conversation chat page's server-side handlers: page title
# fallback, sidebar/effort/share/pin toasts, and the SSE-toast error
# messages the composer's fetch layer surfaces on failed actions.

chat-default-title = Чат

chat-toast-conversation-already-gone = Беседа уже была удалена.
chat-toast-share-copied = Ссылка скопирована — любой вошедший пользователь с этой ссылкой может читать беседу.
chat-toast-share-stopped = Общий доступ прекращён — ссылка больше не работает.
chat-toast-pinned = Закреплено — теперь эта беседа остаётся вверху.
chat-toast-unpinned = Откреплено.
chat-toast-already-in-your-chats = Эта беседа уже есть в ваших чатах.
chat-toast-effort-set = Уровень размышлений: { $level }

chat-mcp-bridged-description = Инструменты, предоставленные интеграцией «{ $name }».

chat-error-conversation-not-found = Беседа не найдена.
chat-error-message-not-found = Сообщение не найдено.
chat-error-message-empty = сообщение не может быть пустым
chat-error-message-must-not-be-empty = Сообщение не должно быть пустым.
chat-error-still-streaming = Для этого пользователя ещё идёт передача ответа — подождите или нажмите «Стоп».
chat-error-retry-assistant-only = Повтор применяется только к ответам ассистента.
chat-error-edit-own-messages-only = Редактирование применяется только к вашим собственным сообщениям.
chat-error-pdf-export-unavailable = Экспорт в PDF недоступен: CLI typst не установлен на шлюзе
chat-error-pdf-export-failed = Не удалось экспортировать в PDF

chat-error-document-not-found = Документ не найден.
chat-error-document-too-large = Этот документ слишком велик для сохранения (лимит 512 КБ).

chat-error-auth-required = требуется авторизация
chat-error-no-such-turn = такого сообщения не существует
chat-error-db-error = ошибка базы данных
chat-error-attachments-not-configured = вложения чата не настроены
chat-error-bad-filename = недопустимое имя файла
chat-error-attachment-not-found = не найдено
chat-error-rate-limited = Достигнут лимит использования. Подробности и время сброса — на /usage.
