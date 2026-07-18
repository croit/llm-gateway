# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/admin.rs` — la página
# `/admin/models`.

admin-page-title = Modelos — LLM Gateway
admin-heading = Modelos
admin-intro-prefix = Ajustes por modelo — precios, ventana de contexto, razonamiento, capacidades y valores de muestreo — aplicados a
admin-intro-every = cada
admin-intro-middle = solicitud de este modelo, de cualquier usuario o token, salvo que quien llame defina el mismo valor, el cual
admin-intro-always-wins = siempre prevalece
admin-intro-suffix = . Los modelos de chat, los alias y otras clases están todos en una lista.
admin-no-models = Aún no se anuncian modelos. En cuanto un backend upstream esté accesible, aparecerá aquí.

admin-filter-placeholder = Filtrar modelos…
admin-filter-all = Todos
admin-filter-chat = chat
admin-filter-other = otras clases
admin-filter-aliases = alias
admin-filter-configured = solo configurados

admin-col-model = Modelo
admin-col-kind = Clase
admin-col-price = Precio ent/sal
admin-col-context = Contexto
admin-col-reasoning = Razonamiento
admin-col-configured = Configurado

admin-value-default = predeterminado
admin-value-na = n/d
admin-not-configured = sin configurar
admin-alias-inherits = hereda los ajustes del destino
admin-reasoning-auto-resolved = Auto → { $style }

admin-badge-price = PRECIO
admin-badge-ctx = CTX
admin-badge-budget = PRESUP
admin-badge-caps = CAPS
admin-badge-toml = TOML

admin-save-model = Guardar modelo
admin-clear-overrides = Borrar todos los ajustes
admin-cancel = Cancelar
admin-other-price-note = El muestreo, el razonamiento y el contexto no se aplican a esta clase — solo los precios, para la contabilidad de costes.

admin-toml-placeholder-header = # Claves comunes (vLLM/OpenAI):
admin-toml-defaults-label = Valores de muestreo (TOML)

admin-reasoning-style-label = Estilo de razonamiento
admin-reasoning-style-aria = Estilo de razonamiento
admin-reasoning-auto = Automático
admin-reasoning-none = ninguno
admin-reasoning-qwen = Qwen (vLLM)
admin-reasoning-openai = OpenAI
admin-reasoning-glm = GLM / z.AI
admin-reasoning-anthropic = Anthropic

admin-effort-standard = Estándar
admin-effort-deep = Profundo
admin-effort-max = Máx
admin-budget-placeholder = predeterminado
admin-budget-hint = Tokens de pensamiento máximos por nivel de esfuerzo. Vacío = valor predeterminado del backend (sin límite). «Fast» desactiva el razonamiento.
admin-effort-default-option = (predeterminado)
admin-effort-hint = Esfuerzo de razonamiento por nivel. Vacío = valor predeterminado integrado. «Fast» desactiva el razonamiento.

admin-malformed-form = formulario con formato incorrecto: { $err }
admin-missing-model-name = falta el campo model_name
admin-db-delete-error = eliminación en la base de datos: { $err }
admin-invalid-toml = TOML no válido: { $err }
admin-db-upsert-error = upsert en la base de datos: { $err }
admin-saved-model = `{ $model }` guardado — efectivo de inmediato
admin-cleared-defaults = ajustes borrados para `{ $model }`
admin-unknown-reasoning-style = estilo de razonamiento desconocido `{ $style }`
admin-db-error = base de datos: { $err }
admin-budget-not-positive = el presupuesto `{ $value }` debe ser un entero positivo
admin-unknown-reasoning-effort = esfuerzo de razonamiento desconocido `{ $value }`
admin-context-window-invalid = la ventana de contexto `{ $value }` debe ser un entero positivo

# Precios por modelo para la contabilidad de costes (precio por 1 M de tokens, entrada / salida).
admin-price-label = { $cur }/{ $unit }
admin-price-unit-tokens = 1 M de tokens
admin-price-unit-images = imagen
admin-price-unit-characters = carácter
admin-price-unit-seconds = segundo
admin-price-in-label = Precio ent
admin-price-out-label = Precio sal
admin-price-in-placeholder = sin precio
admin-price-out-placeholder = sin precio
admin-price-invalid = el precio `{ $value }` debe ser un número no negativo

# Ventana de contexto (controla la compactación automática).
admin-context-window-full-label = Ventana de contexto (tokens)
admin-context-window-placeholder = predet.

admin-alias-chip = alias

# Modelos predeterminados por función.
admin-defaults-heading = Modelos predeterminados
admin-defaults-intro = Elige el modelo preseleccionado para cada función. Vacío = el primer modelo disponible (comportamiento anterior).
admin-defaults-chat-label = Chat
admin-defaults-voice-label = Voz (transcripción)
admin-defaults-image-label = Generación de imágenes
admin-defaults-embedding-label = Embedding (RAG)
admin-defaults-first-option = Primero disponible
admin-defaults-saved = modelo predeterminado establecido en `{ $model }`
admin-defaults-cleared = modelo predeterminado restablecido
admin-defaults-unknown-feature = función desconocida `{ $feature }`

# Capacidades del modelo (tri-estado) + modelos de reserva.
admin-capabilities-heading = Capacidades
admin-cap-vision = Visión
admin-cap-tools = Herramientas
admin-cap-structured-output = Salida estructurada
admin-cap-audio-input = Entrada de audio
admin-cap-pdf-input = Entrada de PDF
admin-cap-parallel-tools = Herramientas en paralelo
admin-cap-unknown = Desconocido
admin-cap-enabled = Activado
admin-cap-disabled = Desactivado
admin-cap-no-fallback = (ninguno)
admin-cap-fallback-vision = Reserva para visión
admin-cap-fallback-tools = Reserva para herramientas

# Recarga de la topología upstream ("Apply changes" en /admin/upstreams).
admin-reloaded = { $pools } pools, { $backends } backends recargados
admin-reload-error = fallo al recargar: { $err }
