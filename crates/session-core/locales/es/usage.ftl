# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/usage.rs` — the
# per-user `/usage` usage-statistics page and its admin-only "all
# users" scope toggle.

usage-title-all = Uso — todos los usuarios — LLM Gateway
usage-title-mine = Tu uso — LLM Gateway

usage-heading-all = Uso — todos los usuarios
usage-heading-mine = Tu uso
usage-blurb-all = Volumen de solicitudes y uso de tokens por usuario y por backend en todos los métodos de acceso. «Solicitudes» cuenta las llamadas al backend de origen, así que un turno que usa herramientas (que hace varias idas y vueltas) cuenta como más de una.
usage-blurb-mine = Tu volumen de solicitudes y uso de tokens en la interfaz de chat, la API y las acciones programadas. «Solicitudes» cuenta las llamadas al backend de origen, así que un turno que usa herramientas cuenta como más de una.

usage-metrics-disabled-prefix = Las métricas de uso están desactivadas (
usage-metrics-disabled-suffix = ). Las cifras siguientes reflejan solo los datos registrados antes de desactivarlas.

usage-toggle-mine = Mío
usage-toggle-all = Todos los usuarios

usage-source-all = Todas las fuentes
usage-source-api = API (/v1)
usage-source-chat = Interfaz de chat
usage-source-scheduled = Programado
usage-backend-all = Todos los backends

usage-filter-period = Período
usage-filter-source = Fuente
usage-filter-backend = Backend
usage-apply = Aplicar

usage-stat-requests-title = Solicitudes
usage-stat-requests-desc = llamadas al backend de origen
usage-stat-tokens-title = Tokens
usage-stat-tokens-desc = prompt + finalización
usage-stat-cost-title = Coste
usage-stat-cost-desc = a los precios de modelo configurados
usage-stat-users-title = Usuarios
usage-stat-users-desc = activos en el período
usage-stat-errors-title = Errores
usage-stat-errors-desc = estado ≥ 400

usage-table-by-user = Por usuario
usage-table-by-backend = Por backend
usage-table-by-source = Por fuente
usage-table-by-model = Por modelo

usage-key-user = Usuario
usage-key-backend = Backend
usage-key-source = Fuente
usage-key-model = Modelo

usage-col-requests = Solicitudes
usage-col-tokens = Tokens
usage-col-cost = Coste
usage-col-errors = Errores

usage-no-activity = Sin actividad en este período.

usage-limits-heading = Tus límites
usage-limit-used = { $percent } % usado
usage-limit-refreshes = se renueva { $time }
usage-unpriced-warning = El gasto excluye modelos sin precio: { $models }. Configura precios en /admin/models para contarlos.
