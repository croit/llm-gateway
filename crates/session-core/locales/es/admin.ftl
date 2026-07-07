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
