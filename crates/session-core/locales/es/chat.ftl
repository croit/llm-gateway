# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/chat/mod.rs` — the
# multi-conversation chat page's server-side handlers: page title
# fallback, sidebar/effort/share/pin toasts, and the SSE-toast error
# messages the composer's fetch layer surfaces on failed actions.

chat-default-title = Chat

chat-toast-conversation-already-gone = La conversación ya había desaparecido.
chat-toast-share-copied = Enlace copiado — cualquier usuario autenticado con el enlace puede seguir la conversación.
chat-toast-share-stopped = Uso compartido detenido — el enlace ya no funciona.
chat-toast-pinned = Fijado — esta conversación ahora permanece arriba.
chat-toast-unpinned = Desfijado.
chat-toast-already-in-your-chats = Esta conversación ya está en tus chats.
chat-toast-effort-set = Esfuerzo de razonamiento: { $level }

chat-mcp-bridged-description = Herramientas conectadas mediante la integración "{ $name }".

chat-error-conversation-not-found = Conversación no encontrada.
chat-error-message-not-found = Mensaje no encontrado.
chat-error-message-empty = el mensaje no puede estar vacío
chat-error-message-must-not-be-empty = El mensaje no debe estar vacío.
chat-error-still-streaming = Todavía se está transmitiendo una respuesta para este usuario — espera o pulsa Detener.
chat-error-retry-assistant-only = Reintentar solo se aplica a las respuestas del asistente.
chat-error-edit-own-messages-only = Editar solo se aplica a tus propios mensajes.
chat-error-pdf-export-unavailable = Exportación a PDF no disponible: el CLI de typst no está instalado en la pasarela
chat-error-pdf-export-failed = Error al exportar a PDF

chat-error-auth-required = se requiere autenticación
chat-error-no-such-turn = no existe ese mensaje
chat-error-db-error = error de base de datos
chat-error-attachments-not-configured = los adjuntos del chat no están configurados
chat-error-bad-filename = nombre de archivo no válido
chat-error-attachment-not-found = no encontrado
