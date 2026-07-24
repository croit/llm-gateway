# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `session-core/src/render.rs` — the HTML renderers for
# the chat-style session UI (conversation bubbles, tool-call rows, the
# document canvas, and the composer). Driver-agnostic: both the gateway
# and any future consumer of this crate render through these functions.

render-edit-button = ✎ Изменить
render-edit-confirm = Сохранить и сгенерировать заново? Это удалит все сообщения ниже.
render-edit-save = Сохранить и сгенерировать заново
render-edit-cancel = Отмена

render-retry-button = ↻ Повторить
render-retry-confirm = Сгенерировать этот ответ заново? Это удалит его и всё, что ниже.

render-attachment-unavailable-title = Этот вложенный файл больше недоступен
render-attachment-unavailable-meta = недоступно
render-attachment-open-title = Открыть { $filename } · { $mime } · { $size }
render-attachment-title = { $filename } · { $mime } · { $size }
render-attachment-chip-title = { $mime } · { $size }
render-attachment-remove-aria = Удалить вложение
render-attachment-remove-confirm = Удалить { $filename }? Это действие нельзя отменить.

# Подпись к каждому сгенерированному медиа в ответе с несколькими медиа,
# чтобы на него можно было сослаться («сделай видео из 2-й картинки»).
render-media-label = { $kind ->
    [image] Изображение { $n }
    [video] Видео { $n }
    [audio] Аудио { $n }
   *[other] Медиа { $n }
}

render-thinking-spinner = Думает…
render-thinking-finalized = Думал { $secs } с
render-thinking-in-progress = Думает… ({ $secs } с)

render-tools-running = Инструменты выполняются
render-tools-errored = Вызовы инструментов
render-tools-used = Использованные инструменты
render-tools-summary = { $count } вызовов · { $breakdown }

render-tool-status-calling = Вызывается
render-tool-status-used = Использован
render-tool-status-error = Ошибка инструмента
render-tool-input-label = Входные данные
render-tool-output-label = Результат
render-tool-output-truncated = усечено для отображения — все { $bytes } байт по-прежнему доступны модели и сохранены в базе данных; отображаются первые { $chars } симв.

render-canvas-close-title = Закрыть
render-canvas-close-aria = Закрыть панель документа
render-canvas-document-aria = Документ
render-canvas-version-aria = Версия

render-composer-attach-aria = Прикрепить файлы
render-composer-attach-title = Прикрепить файлы (также перетаскиванием / вставкой)
render-composer-record-aria = Записать голосовое сообщение
render-composer-record-title = Запись
render-composer-send = Отправить
render-composer-stop = Стоп

render-compaction-divider = Ранние сообщения свёрнуты для экономии контекста
