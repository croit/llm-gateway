# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/admin.rs` — the
# `/admin/models` page for server-wide per-model sampling defaults
# and reasoning-effort/budget overrides.

admin-page-title = Valores predeterminados del modelo — LLM Gateway
admin-heading = Valores predeterminados del modelo
admin-intro-prefix = Parámetros de muestreo predeterminados a nivel de servidor para este modelo, en TOML. Se aplican a
admin-intro-every = cada
admin-intro-middle = solicitud de este modelo, de cualquier usuario o token — a menos que quien llame defina la misma clave en su propia solicitud, la cual
admin-intro-always-wins = siempre prevalece
admin-intro-suffix = . Piénsalo como el mínimo que todos obtienen si no especifican sus propios valores. Vacío = sin valores predeterminados, se aplica el comportamiento integrado del backend.
admin-no-models = Aún no se anuncian modelos de chat. En cuanto un backend upstream esté accesible, aparecerá aquí.

admin-toml-placeholder-header = # Claves comunes (vLLM/OpenAI):
admin-toml-defaults-label = Valores predeterminados TOML
admin-save = Guardar

admin-reasoning-style-aria = Estilo de razonamiento
admin-reasoning-auto = Razonamiento: Automático
admin-reasoning-none = Razonamiento: ninguno
admin-reasoning-qwen = Razonamiento: Qwen (vLLM)
admin-reasoning-openai = Razonamiento: OpenAI
admin-reasoning-glm = Razonamiento: GLM / z.AI
admin-reasoning-anthropic = Razonamiento: Anthropic

admin-effort-standard = Estándar
admin-effort-deep = Profundo
admin-effort-max = Máx
admin-budget-placeholder = predeterminado
admin-budget-hint = Tokens de pensamiento máximos por nivel de esfuerzo. Vacío = valor predeterminado del backend (sin límite). «Fast» desactiva el razonamiento.
admin-effort-default-option = (predeterminado)
admin-effort-hint = Esfuerzo de razonamiento por nivel. Vacío = valor predeterminado integrado. «Fast» desactiva el razonamiento.
admin-save-reasoning-budget = Guardar presupuesto de razonamiento

admin-malformed-form = formulario con formato incorrecto: { $err }
admin-missing-model-name = falta el campo model_name
admin-db-delete-error = eliminación en la base de datos: { $err }
admin-cleared-defaults = valores predeterminados borrados para `{ $model }`
admin-invalid-toml = TOML no válido: { $err }
admin-db-upsert-error = upsert en la base de datos: { $err }
admin-saved-defaults = valores predeterminados guardados para `{ $model }`
admin-unknown-reasoning-style = estilo de razonamiento desconocido `{ $style }`
admin-db-error = base de datos: { $err }
admin-saved-reasoning-style = estilo de razonamiento guardado para `{ $model }`
admin-budget-not-positive = el presupuesto `{ $value }` debe ser un entero positivo
admin-unknown-reasoning-effort = esfuerzo de razonamiento desconocido `{ $value }`
admin-saved-reasoning-budget = presupuesto de razonamiento guardado para `{ $model }`

admin-context-window-label = Contexto
admin-context-window-unit = tok
admin-context-window-placeholder = predet.
admin-context-window-aria = Ventana de contexto (tokens)
admin-context-window-invalid = la ventana de contexto `{ $value }` debe ser un entero positivo
admin-context-window-saved = ventana de contexto establecida para `{ $model }`
admin-context-window-cleared = ventana de contexto borrada para `{ $model }`

# Precios por modelo para la contabilidad de costes (precio por 1 M de tokens, entrada / salida).
admin-price-label = Precio ({ $cur })
admin-price-in-placeholder = ent
admin-price-out-placeholder = sal
admin-price-in-aria = Precio de entrada por 1 M de tokens
admin-price-out-aria = Precio de salida por 1 M de tokens
admin-price-unit = /1M
admin-price-invalid = el precio `{ $value }` debe ser un número no negativo
admin-price-saved = precios establecidos para `{ $model }`

# Modelos predeterminados por función (preseleccionado en los selectores de
# chat/voz, y alternativa de la API cuando una llamada omite el modelo).
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
admin-other-heading = Otros modelos (precios)
admin-other-intro = Modelos de embedding, imagen, voz y transcripción. Los ajustes de muestreo y razonamiento no se aplican, pero fija precios por 1 M de tokens para que su uso cuente en el coste y los límites de coste.

# Tarjeta de alias: nombres de modelo que son alias de otro modelo (real).
admin-aliases-heading = Alias
admin-aliases-intro = Estos nombres son alias de otro modelo. No tienen ajustes ni precio propios: cada solicitud se configura y contabiliza como el modelo al que se resuelve.
admin-alias-chip = alias

# Model capabilities (vision, tools, structured output) + fallback model refs.
admin-capabilities-heading = Capabilities
admin-cap-unknown = Unknown
admin-cap-enabled = Enabled
admin-cap-disabled = Disabled
admin-cap-structured-output = Structured output
admin-cap-no-fallback = (none)
admin-cap-fallback-vision = Fallback for vision
admin-cap-fallback-tools = Fallback for tools
admin-capabilities-saved = saved capabilities for `{ $model }`
admin-capabilities-error = failed to save capabilities: { $err }

# Upstream topology reload ("Apply changes" button).
admin-reloaded = reloaded { $pools } pools, { $backends } backends
admin-reload-error = reload failed: { $err }
