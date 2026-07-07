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
