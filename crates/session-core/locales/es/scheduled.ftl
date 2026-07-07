# STATUS: llm-generated, unreviewed — pending native-speaker QA

scheduled-page-title = Acciones programadas — LLM Gateway
scheduled-edit-page-title = Editar acción programada — LLM Gateway

scheduled-heading = Acciones programadas
scheduled-intro = Ejecuta un prompt automáticamente según una programación. Cada ejecución abre un nuevo chat que puedes leer aquí — elige un modelo, escribe el prompt y decide cuándo debe ejecutarse.
scheduled-create-submit = Crear acción programada
scheduled-list-heading = Tus acciones programadas
scheduled-list-empty = Aún no hay acciones programadas. Crea una arriba.

scheduled-back = Atrás
scheduled-edit-heading = Editar acción programada
scheduled-save-submit = Guardar cambios

scheduled-name-label = Nombre
scheduled-name-placeholder = p. ej. Resumen diario de noticias
scheduled-model-label = Modelo
scheduled-model-placeholder = id del modelo (p. ej. gpt-4o-mini)
scheduled-gdpr-warning = Este modelo no cumple con el RGPD. Las ejecuciones programadas le enviarán tu prompt automáticamente — evita datos personales.
scheduled-nda-warning = Este modelo no está cubierto por un acuerdo de confidencialidad. No programes material protegido por NDA o propietario para este modelo.
scheduled-prompt-label = Prompt
scheduled-prompt-placeholder = ¿Qué debe hacer el modelo en cada ejecución?
scheduled-tools-toggle-label = Permitir herramientas (búsqueda web, RAG, adjuntos) — igual que en el chat
scheduled-reuse-toggle-label = Reutilizar el chat de la ejecución anterior — cada ejecución continúa la misma conversación
scheduled-reuse-rounds-prefix = enviar las últimas
scheduled-reuse-rounds-aria = Número de rondas de historial a repetir
scheduled-reuse-rounds-suffix = rondas

scheduled-builder-heading = Programación
scheduled-mode-hourly = Cada hora
scheduled-mode-daily = Diaria
scheduled-mode-weekly = Semanal
scheduled-mode-monthly = Mensual
scheduled-mode-advanced = Avanzada
scheduled-weekday-mon = Lun
scheduled-weekday-tue = Mar
scheduled-weekday-wed = Mié
scheduled-weekday-thu = Jue
scheduled-weekday-fri = Vie
scheduled-weekday-sat = Sáb
scheduled-weekday-sun = Dom
scheduled-on-day-label = El día
scheduled-of-every-month = de cada mes
scheduled-at-label = A las
scheduled-hour-aria = Hora
scheduled-minute-aria = Minuto
scheduled-of-every-hour = de cada hora
scheduled-timezone-label = Zona horaria
scheduled-timezone-placeholder = Europe/Berlin
scheduled-cron-label = Expresión cron
scheduled-cron-help = Cinco campos: minuto hora día-del-mes mes día-de-la-semana.

scheduled-no-upcoming-runs = No hay próximas ejecuciones.
scheduled-next-runs-prefix = Próximas ejecuciones:{ " " }

scheduled-err-pick-weekday = Elige al menos un día de la semana.
scheduled-err-enter-cron = Introduce una expresión cron.
scheduled-err-unknown-schedule-type = Tipo de programación desconocido «{ $kind }».
scheduled-field-minute = minuto
scheduled-field-hour = hora
scheduled-field-day-of-month = día del mes
scheduled-err-enter-field = Introduce { $field }.
scheduled-err-invalid-field = { $field } no válido: { $value }.
scheduled-err-field-range = { $field } debe estar entre { $min } y { $max }.
scheduled-err-name-length = El nombre debe tener entre 1 y 128 caracteres.
scheduled-err-prompt-length = El prompt debe tener entre 1 y 8000 caracteres.
scheduled-err-pick-model = Elige un modelo.
scheduled-err-unknown-timezone = Zona horaria desconocida «{ $tz }».

scheduled-model-non-gdpr = { $model } (no conforme con el RGPD)
scheduled-model-nda-restricted = { $model } (restringido por confidencialidad)
scheduled-model-non-gdpr-nda-restricted = { $model } (no conforme con el RGPD, restringido por confidencialidad)

scheduled-toast-save-failed = No se pudo guardar la programación.
scheduled-toast-created = Acción programada creada.
scheduled-toast-updated = Programación actualizada.
scheduled-toast-not-found = No existe esa acción programada.
scheduled-toast-update-failed = No se pudo actualizar la programación.
scheduled-toast-resumed = Programación reanudada.
scheduled-toast-paused = Programación pausada.
scheduled-toast-refresh-failed = No se pudo actualizar la programación.
scheduled-toast-deleted = Acción programada eliminada.
scheduled-toast-already-gone = Ya se había eliminado.
scheduled-toast-delete-failed = No se pudo eliminar la programación.

scheduled-badge-active = activa
scheduled-badge-paused = pausada
scheduled-status-paused = Pausada
scheduled-next-run = Próxima ejecución: { $when }
scheduled-no-upcoming-run = Sin próxima ejecución
scheduled-last-success = Última: ✓ { $when }
scheduled-last-success-open = Última: ✓ { $when } — abrir
scheduled-last-failure = Última: ✗ { $when }
scheduled-last-failure-open = Última: ✗ { $when } — abrir
scheduled-pause-title = Pausar
scheduled-resume-title = Reanudar
scheduled-edit-title = Editar
scheduled-delete-title = Eliminar
