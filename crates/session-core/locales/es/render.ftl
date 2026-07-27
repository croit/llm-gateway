# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `session-core/src/render.rs` — the HTML renderers for
# the chat-style session UI (conversation bubbles, tool-call rows, the
# document canvas, and the composer). Driver-agnostic: both the gateway
# and any future consumer of this crate render through these functions.

render-edit-button = ✎ Editar
render-edit-confirm = ¿Guardar y regenerar? Esto elimina todos los mensajes siguientes.
render-edit-save = Guardar y regenerar
render-edit-cancel = Cancelar

render-retry-button = ↻ Reintentar
render-retry-confirm = ¿Regenerar esta respuesta? Esto la elimina junto con todo lo que sigue.

render-attachment-unavailable-title = Este archivo adjunto ya no está disponible
render-attachment-unavailable-meta = no disponible
render-attachment-open-title = Abrir { $filename } · { $mime } · { $size }
render-attachment-title = { $filename } · { $mime } · { $size }
render-attachment-chip-title = { $mime } · { $size }
render-attachment-remove-aria = Eliminar adjunto
render-attachment-remove-confirm = ¿Eliminar { $filename }? Esta acción no se puede deshacer.

# Título de cada medio generado en una respuesta con varios medios, para
# poder referirse a ellos («convierte la 2.ª imagen en un vídeo»).
render-media-label = { $kind ->
    [image] Imagen { $n }
    [video] Vídeo { $n }
    [audio] Audio { $n }
   *[other] Medio { $n }
}

# Botón de copiar en un bloque de código de una respuesta (solo icono,
# por lo que este es su tooltip / nombre accesible).
render-code-copy = Copiar código
render-code-copied = Copiado

render-thinking-spinner = Pensando…
render-thinking-finalized = Pensó durante { $secs } s
render-thinking-in-progress = Pensando… ({ $secs } s)

render-tools-running = Herramientas en ejecución
render-tools-errored = Llamadas a herramientas
render-tools-used = Herramientas utilizadas
render-tools-summary = { $count } llamadas · { $breakdown }

render-tool-status-calling = Llamando
render-tool-status-used = Utilizada
render-tool-status-error = Error de herramienta
render-tool-input-label = Entrada
render-tool-output-label = Salida
render-tool-output-truncated = truncado para su visualización — los { $bytes } bytes completos siguen disponibles para el modelo y almacenados en la base de datos; mostrando los primeros { $chars } caracteres

render-canvas-version-by-you = tú
render-canvas-hand-edited = editado por ti
render-canvas-edit-button = ✎ Editar
render-canvas-save = Guardar como nueva versión
render-canvas-cancel = Cancelar
render-canvas-edit-hint = Se guarda como una nueva versión; el asistente sabrá que lo cambiaste.
render-canvas-close-title = Cerrar
render-canvas-close-aria = Cerrar el panel del documento
render-canvas-document-aria = Documento
render-canvas-version-aria = Versión

render-composer-attach-aria = Adjuntar archivos
render-composer-attach-title = Adjuntar archivos (también arrastrar / pegar)
render-composer-record-aria = Grabar mensaje de voz
render-composer-record-title = Grabar
render-composer-send = Enviar
render-composer-stop = Detener

render-compaction-divider = Mensajes anteriores condensados para ahorrar contexto
