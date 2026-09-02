# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Ajustes del operador (/admin/settings). Los títulos de tarjeta
# (settings-s-*), las etiquetas de campo (settings-f-*) y su ayuda
# (settings-f-*-help) se derivan de las entradas de
# gateway_core::server::settings::SECTIONS:
# `sandbox.runner_url` -> `settings-f-sandbox-runner_url`.
# Véase locales/en/settings.ftl para la fuente.

settings-heading = Ajustes
settings-intro = Ajustes de operación de esta pasarela. Se guardan en la base de datos, así que no hace falta ningún fichero de configuración — cada campo muestra además la clave TOML a la que sustituye.
settings-save = Guardar sección
settings-saved = Guardado. Activo desde la siguiente petición.
settings-saved-restart = Guardado. Algunos campos de esta sección solo se aplican tras reiniciar.
settings-save-failed = No se han podido guardar estos ajustes.
settings-cleared = Restablecido. Vuelve a aplicarse el valor por defecto.
settings-restart-badge = reinicio
settings-restart-note = Los campos marcados con «reinicio» se leen solo al arrancar; cambiarlos requiere reiniciar.
settings-secret-set = guardado — escribe un valor nuevo para sustituirlo
settings-secret-unset = sin definir
settings-secret-clear = Borrar

settings-no-backend-heading = Todavía no hay backend de modelos
settings-no-backend-body = El inicio de sesión ya está configurado, pero esta pasarela no sirve ningún modelo hasta que añadas un backend. Hasta entonces, el chat y la API /v1 rechazan las peticiones.
settings-no-backend-cta = Añadir un backend en /admin/upstreams →

settings-tab-chat = Chat
settings-tab-tools = Herramientas
settings-tab-data = Contenido y datos
settings-tab-access = Acceso y uso
settings-tab-notifications = Notificaciones
settings-show-fields = Mostrar { $count } ajustes más
settings-model-automatic = Automático — usar el primer modelo disponible
settings-model-none-configured = Todavía no hay ningún modelo de este tipo configurado. Añade un pool en /admin/upstreams y aparecerá aquí.
settings-model-unavailable = { $model } (configurado, pero no disponible ahora)
settings-restart-pending-heading = Reinicio pendiente
settings-restart-pending-body = Estos ajustes están guardados, pero solo se aplican tras reiniciar la pasarela:

# ─── Tarjetas de sección ─────────────────────────────────────────────────────

settings-s-chat-ocr = OCR de documentos
settings-s-chat-ocr-blurb = Convertir los PDF e imágenes subidos en texto que el modelo pueda leer.
settings-s-chat-compaction = Compactación de conversaciones
settings-s-chat-compaction-blurb = Resumir la mitad más antigua de una conversación larga para que siga cabiendo en la ventana de contexto del modelo.
settings-s-chat-s3 = Almacenamiento de adjuntos (S3)
settings-s-chat-s3-blurb = Almacenamiento de objetos para los adjuntos del chat. Sin él, las subidas se rechazan.
settings-s-sandbox = Sandbox de código
settings-s-sandbox-blurb = El ejecutor aislado que corre el código escrito por el modelo.
settings-s-comfyui = ComfyUI imagen y vídeo
settings-s-comfyui-blurb = El worker headless de ComfyUI detrás de las herramientas de imagen y vídeo.
settings-s-rag = Indexación RAG
settings-s-rag-blurb = Dónde se guardan las fuentes indexadas y con cuánta intensidad trabaja el indexador.
settings-s-skills = Habilidades
settings-s-skills-blurb = El directorio de paquetes en disco detrás de /admin/skills.
settings-s-typst = Plantillas Typst
settings-s-typst-blurb = Las plantillas detrás de la exportación a PDF y las herramientas de documentos.
settings-s-geoip = GeoIP
settings-s-geoip-blurb = Ubicación aproximada del cliente, para la herramienta get_user_location.
settings-s-usage = Métricas de uso
settings-s-usage-blurb = Contabilidad por solicitud detrás de /usage.
settings-s-limits = Límites de tasa y cuotas
settings-s-limits-blurb = Interruptor principal de las reglas configuradas en /admin/limits.
settings-s-feedback = Widget de comentarios
settings-s-feedback-blurb = Dónde abre incidencias el widget de comentarios integrado.
settings-s-push = Web Push
settings-s-push-blurb = Avisos al terminar una respuesta. El par de claves se genera y guarda automáticamente.
settings-s-gateway = Sesiones y tokens
settings-s-gateway-blurb = Cuánto tiempo siguen siendo válidos un inicio de sesión del navegador y un token de API, y si los administradores pueden suplantar a otros usuarios.

# ─── Campos ──────────────────────────────────────────────────────────────────

settings-f-chat-ocr-enabled = Activar OCR
settings-f-chat-ocr-enabled-help = Interruptor principal para extraer texto de los documentos subidos.
settings-f-chat-ocr-model = Modelo de OCR
settings-f-chat-ocr-model-help = Qué modelo lee las páginas. Debe servirlo un pool de tipo ocr; en automático se usa el primero disponible.
settings-f-chat-ocr-max_tokens = Presupuesto de tokens por solicitud
settings-f-chat-ocr-max_tokens-help = Presupuesto de tokens para una solicitud de OCR.
settings-f-chat-ocr-ngram_window = Ventana de solapamiento
settings-f-chat-ocr-ngram_window-help = Solapamiento con el que se unen los textos de las páginas sin repetir contenido.
settings-f-chat-ocr-max_bytes = Tamaño máximo del documento
settings-f-chat-ocr-max_bytes-help = Documento más grande que se acepta, en bytes.
settings-f-chat-ocr-max_pages = Páginas máximas
settings-f-chat-ocr-max_pages-help = Número máximo de páginas que se leen de un mismo documento.
settings-f-chat-ocr-dpi = Resolución de rasterizado
settings-f-chat-ocr-dpi-help = Resolución a la que se renderizan las páginas PDF antes de leerlas, en DPI.
settings-f-chat-ocr-max_output_chars = Texto extraído máximo
settings-f-chat-ocr-max_output_chars-help = Tope del texto extraído de un documento, en caracteres.
settings-f-chat-ocr-timeout_secs = Tiempo límite
settings-f-chat-ocr-timeout_secs-help = Plazo para un documento, en segundos.
settings-f-chat-ocr-max_concurrency = Páginas en paralelo
settings-f-chat-ocr-max_concurrency-help = Cuántas páginas se leen a la vez.
settings-f-chat-ocr-auto_min_text_chars_per_page = Umbral de detección de escaneo
settings-f-chat-ocr-auto_min_text_chars_per_page-help = Por debajo de esta cantidad de caracteres incrustados por página, un PDF se trata como escaneado y se envía a OCR.

settings-f-chat-compaction-enabled = Activar la compactación
settings-f-chat-compaction-enabled-help = Interruptor principal para resumir conversaciones largas.
settings-f-chat-compaction-default_context_window = Ventana de contexto asumida
settings-f-chat-compaction-default_context_window-help = Ventana de contexto en tokens que se asume para un modelo que no declara ninguna.
settings-f-chat-compaction-trigger_ratio = Umbral de activación
settings-f-chat-compaction-trigger_ratio-help = Fracción de la ventana de contexto que dispara la compactación (0,7 = al 70 % de ocupación).
settings-f-chat-compaction-keep_recent_turns = Turnos recientes conservados
settings-f-chat-compaction-keep_recent_turns-help = Turnos que se conservan literalmente al final de la conversación.
settings-f-chat-compaction-min_turns_to_compact = Longitud mínima de conversación
settings-f-chat-compaction-min_turns_to_compact-help = Nunca compactar una conversación con menos turnos que este número.
settings-f-chat-compaction-summary_max_tokens = Presupuesto de tokens del resumen
settings-f-chat-compaction-summary_max_tokens-help = Presupuesto de tokens para el resumen que sustituye a los turnos compactados.

settings-f-chat-s3-enabled = Guardar los adjuntos en S3
settings-f-chat-s3-enabled-help = Desactivado, los adjuntos del chat no están disponibles.
settings-f-chat-s3-endpoint = URL del endpoint
settings-f-chat-s3-endpoint-help = Por ejemplo https://s3.eu-central-1.amazonaws.com, o una dirección de MinIO.
settings-f-chat-s3-region = Región
settings-f-chat-s3-region-help = Nombre de la región.
settings-f-chat-s3-bucket = Bucket
settings-f-chat-s3-bucket-help = Bucket que contiene los adjuntos.
settings-f-chat-s3-key_prefix = Prefijo de clave
settings-f-chat-s3-key_prefix-help = Prefijo bajo el que se escribe cada clave de objeto.
settings-f-chat-s3-access_key = ID de clave de acceso
settings-f-chat-s3-access_key-help = Identificador de la clave de acceso con la que se llega al bucket.
settings-f-chat-s3-secret_key = Clave de acceso secreta
settings-f-chat-s3-secret_key-help = Mitad secreta de esa clave de acceso. Se guarda cifrada.

settings-f-sandbox-enabled = Activar las herramientas del sandbox
settings-f-sandbox-enabled-help = Registra las herramientas que permiten al modelo ejecutar código.
settings-f-sandbox-runner_url = URL del ejecutor
settings-f-sandbox-runner_url-help = URL base del servicio sandbox-runner. Ejecuta código arbitrario, así que solo debe ser accesible desde la pasarela.
settings-f-sandbox-timeout_secs = Tiempo límite
settings-f-sandbox-timeout_secs-help = Plazo HTTP para una ejecución, en segundos.
settings-f-sandbox-max_artifact_bytes = Tamaño máximo de artefacto
settings-f-sandbox-max_artifact_bytes-help = Archivo individual más grande que se acepta de vuelta de una ejecución, en bytes.

settings-f-comfyui-enabled = Activar las herramientas de imagen y vídeo
settings-f-comfyui-enabled-help = Registra las herramientas comfyui_*.
settings-f-comfyui-base_url = URL de ComfyUI
settings-f-comfyui-base_url-help = URL base de la instancia de ComfyUI. No tiene autenticación, así que solo debe ser accesible desde la pasarela.
settings-f-comfyui-content_dir = Directorio de workflows
settings-f-comfyui-content_dir-help = Contiene un subdirectorio por workflow. Usa el botón de recarga en /admin/comfyui para releerlo sin reiniciar.
settings-f-comfyui-timeout_secs = Tiempo límite
settings-f-comfyui-timeout_secs-help = Plazo para una ejecución de workflow, en segundos.
settings-f-comfyui-queue_poll_interval_ms = Intervalo de sondeo de la cola
settings-f-comfyui-queue_poll_interval_ms-help = Cada cuánto pregunta la pasarela a ComfyUI por un trabajo en curso, en milisegundos.
settings-f-comfyui-max_concurrent_jobs = Trabajos simultáneos
settings-f-comfyui-max_concurrent_jobs-help = Cuántos workflows puede tener el modelo en ejecución a la vez.

settings-f-rag-enabled = Ejecutar el indexador
settings-f-rag-enabled-help = Interruptor principal de la indexación y la recuperación RAG.
settings-f-rag-data_dir = Directorio de índices
settings-f-rag-data_dir-help = Dónde se guardan los índices. Debe estar en el volumen persistente, o cada reinicio reindexará todo. Los índices existentes no se mueven con él: apunta esto a otro sitio y todo se reindexa desde cero.
settings-f-rag-clone_concurrency = Trabajos de indexación en paralelo
settings-f-rag-clone_concurrency-help = Cuántos clones de git y trabajos de indexación se ejecutan a la vez.

settings-f-skills-enabled = Cargar los paquetes de habilidades
settings-f-skills-enabled-help = Interruptor principal de las habilidades gestionadas en /admin/skills.
settings-f-skills-dir = Directorio de paquetes
settings-f-skills-dir-help = Directorio que contiene los paquetes de habilidades.

settings-f-typst-enabled = Cargar las plantillas Typst
settings-f-typst-enabled-help = Interruptor principal de la exportación a PDF y las herramientas de documentos.
settings-f-typst-templates_dir = Directorio de plantillas
settings-f-typst-templates_dir-help = Directorio que contiene las plantillas. Se relee al guardar, así que añadir una no requiere reiniciar.

settings-f-geoip-enabled = Activar las consultas GeoIP
settings-f-geoip-enabled-help = Interruptor principal de la herramienta get_user_location.
settings-f-geoip-db_path = Archivo de base de datos
settings-f-geoip-db_path-help = Ruta de la base de datos BIN de IP2Location.
settings-f-geoip-update_token = Token de descarga
settings-f-geoip-update_token-help = Token de IP2Location para actualizar la base de datos. Se guarda cifrado.

settings-f-usage-enabled = Registrar el uso
settings-f-usage-enabled-help = Contabilidad por solicitud detrás de /usage.
settings-f-usage-retention_days = Retención
settings-f-usage-retention_days-help = Cuántos días se conservan los registros.
settings-f-usage-currency = Moneda
settings-f-usage-currency-help = Moneda en la que se informan los costes.

settings-f-limits-enabled = Aplicar límites y cuotas
settings-f-limits-enabled-help = Desactivado, las reglas de /admin/limits se ignoran.

settings-f-feedback-enabled = Ofrecer el widget de comentarios
settings-f-feedback-enabled-help = Interruptor principal del botón de comentarios de la aplicación.
settings-f-feedback-github_owner = Propietario del repositorio
settings-f-feedback-github_owner-help = Usuario u organización de GitHub que posee el gestor de incidencias.
settings-f-feedback-github_repo = Repositorio
settings-f-feedback-github_repo-help = Nombre del repositorio en el que se abren las incidencias.
settings-f-feedback-github_token = Token de GitHub
settings-f-feedback-github_token-help = Necesita issues:write, y además contents:write si se adjuntan capturas. Se guarda cifrado.
settings-f-feedback-github_api_base = URL base de la API
settings-f-feedback-github_api_base-help = URL base de la API REST. Cámbiala para GitHub Enterprise.
settings-f-feedback-labels = Etiquetas de incidencia
settings-f-feedback-labels-help = Etiquetas que se aplican a cada incidencia abierta.
settings-f-feedback-assets_branch = Rama de capturas
settings-f-feedback-assets_branch-help = Rama huérfana en la que se hacen commit las capturas de pantalla.
settings-f-feedback-extraction_model = Modelo de extracción
settings-f-feedback-extraction_model-help = Modelo de chat que convierte una nota de voz en los campos del formulario.
settings-f-feedback-voice_model = Modelo de transcripción
settings-f-feedback-voice_model-help = Modelo que convierte la nota de voz en texto.

settings-f-push-enabled = Enviar notificaciones push
settings-f-push-enabled-help = Expone los endpoints de push y avisa cuando termina una respuesta.
settings-f-push-contact = Contacto del operador
settings-f-push-contact-help = Una URI mailto: o https: con la que el servicio de push puede localizarte.

settings-f-gateway-token_ttl_days = Vida útil de los tokens de API
settings-f-gateway-token_ttl_days-help = Cuántos días sigue siendo válido un token gwk_… recién creado.
settings-f-gateway-session_ttl_days = Tiempo de inactividad de sesión
settings-f-gateway-session_ttl_days-help = Límite de inactividad deslizante para un inicio de sesión del navegador, en días: cada solicitud lo empuja hacia delante, así que es el tiempo que alguien puede ausentarse antes de tener que volver a entrar.
settings-f-gateway-session_absolute_max_days = Edad máxima de la sesión
settings-f-gateway-session_absolute_max_days-help = Tope estricto en días desde el inicio de sesión, que ninguna actividad prolonga. También obliga a pasar periódicamente por el proveedor de identidad, el único momento en que se releen las reclamaciones de grupo.
settings-f-gateway-allow_impersonation = Permitir la suplantación
settings-f-gateway-allow_impersonation-help = Deja que los administradores actúen como otro usuario para depurar. Cada suplantación queda auditada y muestra un aviso permanente; desactivado, los botones se ocultan y el endpoint rechaza.
