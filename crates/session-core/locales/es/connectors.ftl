# STATUS: llm-generated, unreviewed — pending native-speaker QA

connectors-page-title = Conectores — LLM Gateway
connectors-heading = Conectores
connectors-restore-defaults-button = Restaurar valores predeterminados
connectors-catalog-intro = Selecciona los servidores MCP que los usuarios pueden conectar desde Integraciones. Habilita un conector para hacerlo visible. Los conectores que no pueden usar el registro dinámico de cliente (p. ej. Google) necesitan un id/secreto de cliente OAuth de despliegue antes de poder habilitarse.
connectors-empty-state = Todavía no hay conectores.

connectors-badge-enabled = Habilitado
connectors-badge-disabled = Deshabilitado
connectors-badge-default = Predeterminado
connectors-badge-dcr = DCR
connectors-badge-needs-client-id = Necesita id de cliente
connectors-disable-button = Deshabilitar
connectors-enable-disabled-title = Añade primero el id de cliente OAuth abajo (Editar → Id de cliente OAuth)
connectors-enable-button = Habilitar
connectors-delete-confirm = ¿Eliminar este conector? Se elimina para todos los usuarios, junto con sus conexiones y tokens almacenados. Esta acción no se puede deshacer.
connectors-delete-button = Eliminar
connectors-edit-summary = Editar

connectors-add-summary = Añadir un conector

connectors-oauth-help-token-1 = Conector de token: configura arriba la URL del servidor MCP; cada usuario pega su propio token de API en Integraciones (enviado como
connectors-oauth-help-token-2 = ). No se necesita cliente OAuth.

connectors-oauth-help-dcr-heading = Registro dinámico de clientes — no se necesita cliente OAuth
connectors-oauth-help-dcr-body = Basta con configurar arriba la URL del servidor MCP. El servidor registra esta pasarela automáticamente (RFC 7591); luego cada usuario hace clic en Conectar y se autoriza con su propia cuenta — un único inicio de sesión cubre todos los servicios que expone el servidor.

connectors-oauth-help-gws-1 = Apunta esto a tu
connectors-oauth-help-gws-self-hosted = servidor MCP de Google Workspace autoalojado
connectors-oauth-help-gws-2 = (p. ej.
connectors-oauth-help-gws-3 = ) ejecutándose en modo streamable-HTTP — la URL termina en
connectors-oauth-help-gws-4 = . Ese servidor guarda el cliente OAuth de Google y usa las
connectors-oauth-help-gws-ga-apis = API GA de Google
connectors-oauth-help-gws-5 = (sin developer preview). Permite la URI de redirección de esta pasarela en el servidor mediante
connectors-oauth-help-gws-footer = Los endpoints MCP alojados por Google (gmailmcp/calendarmcp/drivemcp.googleapis.com) no se usan intencionadamente — requieren inscribir la organización en el Workspace Developer Preview Program. Consulta docs/connectors.md para el procedimiento de despliegue.

connectors-oauth-help-generic-heading = Configurar el cliente OAuth
connectors-oauth-help-generic-intro = Registra esta URI de redirección exacta en tu cliente OAuth y luego pega su id de cliente (y secreto) abajo:
connectors-oauth-help-google-1 = Google: crea un
connectors-oauth-help-google-link = id de cliente OAuth 2.0 (aplicación web)
connectors-oauth-help-google-2 = en Google Cloud Console, añade la URI de redirección de arriba y habilita las API de Gmail / Google Calendar / Google Drive para el proyecto.
connectors-oauth-help-github-1 = GitHub: crea una
connectors-oauth-help-github-link = aplicación OAuth
connectors-oauth-help-github-2 = (Configuración → Configuración de desarrollador → Aplicaciones OAuth), define la URL de retorno de autorización con la URI de redirección de arriba, y copia el Client ID y un secreto de cliente generado.
connectors-oauth-help-fallback = Crea un cliente OAuth en tu proveedor con esta URI de redirección y las URL de autorización/token configuradas abajo.
connectors-oauth-why-1 = ¿Por qué un paso de administración único? En OAuth, el id de cliente identifica
connectors-term-this-gateway = esta pasarela
connectors-oauth-why-2 = como una aplicación (compartida por todos los usuarios) — solo difiere el token de acceso de cada usuario. Claude Desktop se lo salta porque Anthropic distribuye aplicaciones preregistradas ligadas a su URL de redirección fija; una pasarela autoalojada usa su propia URI de redirección (arriba), y Google/GitHub no admiten el registro automático (DCR) como sí hace Atlassian — así que te registras una vez y luego cada usuario solo tiene que hacer clic en Conectar.
connectors-oauth-why-no-app = ¿Ninguna aplicación OAuth en absoluto?
connectors-oauth-why-3 = Cambia la autenticación a «Token proporcionado por el usuario» y cada usuario pega su propio token (p. ej. un token de acceso personal de GitHub) — las credenciales llegan entonces directamente del usuario, sin cliente de administrador.

connectors-field-key-label = Clave (id estable)
connectors-field-key-placeholder = p. ej. gmail
connectors-field-key-readonly-label = Clave
connectors-field-name-label = Nombre
connectors-field-name-placeholder = Nombre para mostrar
connectors-field-icon-label = Icono (emoji)
connectors-field-category-label = Categoría
connectors-field-category-placeholder = Google
connectors-field-description-label = Descripción
connectors-field-description-placeholder = Qué hace este conector
connectors-field-url-label = URL del servidor MCP
connectors-field-auth-label = Autenticación
connectors-auth-option-oauth = OAuth 2.1 (cada usuario se autoriza mediante el proveedor)
connectors-auth-option-token = Token proporcionado por el usuario (cada usuario pega su propio token de API)
connectors-field-client-json-label = Pegar el JSON del cliente OAuth (opcional — p. ej. «Descargar JSON» de Google)
connectors-field-client-json-help = Rellena el id/secreto de cliente (y las URL de autorización y token) a partir del archivo. O usa los campos individuales de abajo.
connectors-field-client-id-label = Id de cliente OAuth
connectors-field-client-id-placeholder = …apps.googleusercontent.com / id de aplicación OAuth de GitHub
connectors-field-client-id-help-1 = El id público que identifica
connectors-field-client-id-help-2 = como aplicación ante el proveedor — creado una vez por un administrador en la página de credenciales OAuth del proveedor (Google Cloud → Credenciales, GitHub → Aplicaciones OAuth). No es un secreto por usuario. Déjalo en blanco si DCR está habilitado.
connectors-field-client-secret-label = Secreto de cliente OAuth
connectors-secret-placeholder-existing = •••••••• (deja en blanco para conservarlo)
connectors-secret-placeholder-new = secreto de cliente (opcional)
connectors-field-client-secret-help = Se emite junto con el id de cliente en la misma página. Se almacena cifrado; déjalo en blanco para conservar el existente.
connectors-field-use-dcr-label = Probar el registro dinámico de clientes (RFC 7591)
connectors-field-scopes-label = Scopes (separados por espacios)
connectors-advanced-summary = Avanzado: anulaciones de descubrimiento
connectors-field-authorize-url-label = URL de autorización
connectors-field-token-url-label = URL de token
connectors-field-registration-url-label = URL de registro
connectors-placeholder-optional-override = anulación opcional
connectors-field-required-role-label = Rol requerido (puerta RBAC)
connectors-placeholder-optional = opcional
connectors-save-changes-button = Guardar cambios
connectors-add-connector-button = Añadir conector

connectors-error-missing-fields = se requieren la clave, el nombre y la URL
connectors-error-bad-client-json = no se pudo leer un client_id del JSON pegado — se esperaba el archivo de cliente OAuth de Google ({"{"}"web":{"{"}"client_id":…,"client_secret":…{"}"}{"}"}).
connectors-error-sealing-secret = sellando el secreto: { $error }
connectors-error-saving = guardando el conector: { $error }
connectors-error-needs-client-id = este conector necesita un id de cliente OAuth antes de poder habilitarse (no puede usar el registro dinámico). Edítalo y añade el id/secreto de cliente.
connectors-error-toggling = cambiando el conector: { $error }
connectors-error-deleting = eliminando el conector: { $error }
connectors-error-restoring = restaurando los valores predeterminados: { $error }
