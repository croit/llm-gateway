# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/chat/render.rs` — the
# gateway-only chat-page chrome: the header model/voice pickers, the
# compliance banners, the composer's "+" tools/integrations/skills menu,
# the "Denken" (effort/thinking) picker, and the share/export/fork
# controls. Prefixed `chat-render-` (rather than `chat-`) to avoid
# colliding with `chat/mod.rs`'s own `chat-*` keys in the sibling
# `chat.ftl`.

chat-render-canvas-toggle-title = Mostrar / ocultar el lienzo de documento
chat-render-canvas-toggle-label = Lienzo
chat-render-canvas-document-tab = Documento
chat-render-canvas-assets-tab = Archivos
chat-render-canvas-assets-heading = Archivos de la conversación
chat-render-canvas-assets-count = { $count ->
    [one] { $count } archivo
   *[other] { $count } archivos
}
chat-render-canvas-assets-empty = Todavía no se han añadido archivos a esta conversación.
chat-render-canvas-asset-download = Descargar archivo
chat-render-canvas-close-title = Cerrar lienzo

chat-render-model-placeholder = modelo (p. ej., gpt-4o-mini)
chat-render-model-aria = Modelo de chat
chat-render-voice-model-aria = Modelo de voz
chat-render-tts-voice-aria = Voz de las respuestas
chat-render-tts-voice-default = Voz predeterminada

chat-render-model-non-gdpr = { $id } (no conforme con el RGPD)
chat-render-model-confidential = { $id } (confidencialidad restringida)
chat-render-model-non-gdpr-confidential = { $id } (no conforme con el RGPD, confidencialidad restringida)

chat-render-gdpr-banner = Estás enviando datos a un modelo que no cumple con el RGPD. No introduzcas información personal (nombres, correos electrónicos, direcciones, datos de clientes o empleados).
chat-render-nda-banner = Este modelo no está cubierto por un acuerdo de confidencialidad. No envíes material protegido por un NDA ni información confidencial.

chat-render-shared-readonly-banner = Chat compartido — solo lectura. Solo quien lo creó puede responder.
chat-render-composer-placeholder = Escribe un mensaje al modelo…

chat-render-new-conversation-fallback = Nueva conversación

chat-render-feedback-title = Enviar comentarios

chat-render-effort-title = Esfuerzo de razonamiento
chat-render-effort-tooltip = Esfuerzo de razonamiento: más alto = más razonamiento y más rondas de herramientas, pero más lento
chat-render-effort-label-prefix = Razonamiento:
chat-render-effort-fast = Rápido
chat-render-effort-standard = Estándar
chat-render-effort-deep = Profundo
chat-render-effort-max = Máximo

chat-render-tools-tooltip = Herramientas, integraciones y skills para esta conversación
chat-render-tools-label = Herramientas
chat-render-tools-search-placeholder = Buscar herramientas…
chat-render-all-tools-label = Todas las herramientas
chat-render-no-tools-prefix = Todavía no hay herramientas disponibles para tu cuenta. Conecta una integración en
chat-render-no-tools-suffix = .

chat-render-close = Cerrar

chat-render-group-web-network = Web y Red
chat-render-group-attachments-documents = Adjuntos y Documentos
chat-render-group-document-templates = Plantillas de documentos
chat-render-group-knowledge-base = Base de conocimiento
chat-render-group-code-sandbox = Código y Sandbox
chat-render-group-memory = Memoria
chat-render-group-integrations = Integraciones
chat-render-group-utility = Utilidades
chat-render-group-skills = Skills

chat-render-tool-count = { $count ->
    [one] { $count } herramienta
   *[other] { $count } herramientas
}

chat-render-active-count-title = Herramientas activas — toca para gestionar
chat-render-unpin-title = Desanclar (volver a Automático)

chat-render-state-off-tip = Desactivado — bloqueado; oculto para el asistente
chat-render-state-auto-tip = Automático — el asistente lo activa cuando una solicitud lo necesita
chat-render-state-on-tip = Activado — siempre disponible para el asistente

chat-render-share-label-on = Compartido ✓
chat-render-share-label-off = Compartir
chat-render-share-tooltip = Los chats compartidos pueden leerlos cualquier usuario con sesión iniciada que tenga el enlace

chat-render-fork-tooltip = Copia esta conversación en tus propios chats para seguir chateando
chat-render-fork-label = Continuar en mis chats

chat-render-export-tooltip = Descargar esta conversación
chat-render-export-aria = Exportar conversación
chat-render-export-label = Exportar
chat-render-export-pdf = Documento PDF
chat-render-export-md = Markdown (.md)
