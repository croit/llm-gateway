# STATUS: llm-generated, unreviewed — pending native-speaker QA

integrations-page-title = Integraciones — LLM Gateway
integrations-heading = Integraciones
integrations-intro = Conecte sus propias cuentas para que el asistente pueda actuar en su nombre — leyendo su correo, calendario, archivos, repositorios y más. Cada conexión usa sus propios permisos y puede desconectarse en cualquier momento.
integrations-empty = Todavía no hay conectores disponibles. Un administrador puede habilitarlos en Admin → Conectores.

integrations-badge-connected = Conectado
integrations-badge-needs-reconnect = Necesita reconexión
integrations-badge-needs-admin-setup = Necesita configuración del administrador

integrations-reconnect-title = Restablecer la conexión (reautenticar / reintentar)
integrations-reconnect-button = Reconectar
integrations-disconnect-button = Desconectar
integrations-disconnect-confirm = ¿Desconectar esta integración? Se eliminará su token de acceso almacenado.
integrations-connect-button = Conectar

integrations-token-label = Su token de API
integrations-token-placeholder = pegue su token

integrations-tools-error-prefix = No se pudieron cargar las herramientas de este conector:
integrations-tools-error-hint = Compruebe la URL del servidor MCP / su token y luego use Reconectar arriba.
integrations-tools-error-hint-reauth = Su autorización ya no es válida: use Reconectar arriba para iniciar sesión de nuevo.
integrations-tools-empty = Este conector no expone ninguna herramienta.
integrations-tools-header = Permisos de herramientas ({ $count })
integrations-set-all-label = Establecer todo:
integrations-mode-always = Siempre
integrations-mode-ask = Preguntar
integrations-mode-off = Desactivado
integrations-tools-toggle = Mostrar / ocultar herramientas individuales
integrations-tool-kind-read = lectura
integrations-tool-kind-write = escritura

integrations-error-unknown-connector = conector desconocido o deshabilitado
integrations-error-forbidden-role = no tiene acceso a este conector
integrations-error-not-oauth = este conector no usa OAuth
integrations-error-oauth-discovery-failed = falló el descubrimiento de OAuth: { $error }
integrations-error-needs-setup-no-client = este conector necesita configuración: no hay un id de cliente configurado y el proveedor no ofrece registro dinámico. Pida a un administrador que agregue un cliente OAuth.
integrations-error-sealing-client-secret = sellando el secreto del cliente: { $error }
integrations-error-dcr-failed = falló el registro dinámico del cliente: { $error }
integrations-error-needs-setup-admin = este conector necesita configuración: un administrador debe configurar un id de cliente OAuth.
integrations-error-building-authorize-url = construyendo la URL de autorización: { $error }
integrations-error-persisting-authorization = guardando la autorización: { $error }
integrations-error-provider-error = el proveedor devolvió un error: { $error } { $desc }
integrations-error-callback-missing = al callback le falta el código o el estado
integrations-error-auth-expired = esta autorización ha expirado o ya se usó — comience de nuevo desde Integraciones
integrations-error-loading-authorization = cargando la autorización: { $error }
integrations-error-state-mismatch = el estado de autorización no coincidió con su sesión
integrations-error-connector-missing = el conector ya no existe
integrations-error-decrypting-client-secret = descifrando el secreto del cliente: { $error }
integrations-error-connector-missing-client-id = al conector le falta su id de cliente OAuth
integrations-error-sealing-access-token = sellando el token de acceso: { $error }
integrations-error-sealing-refresh-token = sellando el token de actualización: { $error }
integrations-error-saving-connection = guardando la conexión: { $error }
integrations-error-not-token-based = este conector no se basa en un token
integrations-error-token-required = se requiere un token
integrations-error-sealing-token = sellando el token: { $error }
integrations-error-unknown-connector-plain = conector desconocido
integrations-error-invalid-mode = modo de permiso inválido
integrations-error-saving-tool-permission = guardando el permiso de la herramienta: { $error }
integrations-error-saving-permissions = guardando los permisos: { $error }
integrations-error-listing-tools = listando las herramientas: { $error }
integrations-error-disconnecting = desconectando: { $error }
integrations-error-connection-unavailable = conexión no disponible
