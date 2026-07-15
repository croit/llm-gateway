# STATUS: llm-generated, unreviewed — pending native-speaker QA

tokens-page-title = Tokens de API — LLM Gateway
tokens-page-heading = Tokens de API
tokens-intro = Tokens Bearer para la API compatible con OpenAI. El texto plano solo se muestra al crearlo — guárdelo en un lugar seguro.

tokens-create-heading = Crear token
tokens-create-description = Genere un nuevo token Bearer para la API compatible con OpenAI.
tokens-name-label = Nombre
tokens-name-placeholder = p. ej. laptop, ci-runner
tokens-ttl-label = TTL (días)
tokens-create-submit = Crear token

tokens-list-heading = Sus tokens
tokens-list-empty = Aún no hay tokens. Cree uno arriba.

tokens-badge-revoked = revocado
tokens-badge-active = activo
tokens-remove-button = Eliminar
tokens-rotate-button = Rotar
tokens-rotate-title = Emitir un nuevo secreto para este token (conserva su nombre y configuración)
tokens-revoke-button = Revocar

tokens-row-meta = creado { $created } · último uso { $last_used } · expira { $expires }
tokens-last-used-never = nunca

tokens-tool-use-aria = Uso de herramientas
tokens-tool-use-label = Uso de herramientas
tokens-tool-use-description = Permitir que este token llame a las herramientas del gateway (búsqueda web, RAG, …).
tokens-capabilities-summary = Capacidades

tokens-mcp-allow-aria = Permitir herramientas MCP en modo "ask" a través de la API
tokens-mcp-allow-label = Permitir herramientas MCP “ask” a través de la API
tokens-mcp-allow-description = Las herramientas de conector que requieren aprobación no pueden solicitar confirmación a través de la API; al activarlo se ejecutan sin preguntar.

tokens-minted-heading = Token creado
tokens-minted-copy-warning = Copie el valor ahora — no podrá volver a verlo después.
tokens-copy-aria = Copiar token
tokens-copy-title = Copiar token
tokens-minted-name = Nombre: { $name }

tokens-account-heading = Cuenta
tokens-signed-in-as = Conectado como { $email }
tokens-account-user-id-label = ID de usuario
tokens-account-oidc-label = Roles OIDC
tokens-account-rbac-label = IDs de rol RBAC
tokens-roles-none = ninguno
tokens-roles-none-granted = ninguno concedido

tokens-malformed-form = formulario inválido: { $err }
tokens-name-length = El nombre del token debe tener entre 1 y 128 caracteres.
tokens-store-failed = No se pudo guardar el token.
tokens-created-toast = Token creado.

tokens-revoked-not-found = Token revocado no encontrado.
tokens-revoked-toast = Token revocado.
tokens-already-revoked = El token ya estaba revocado.
tokens-revoke-failed = Error al revocar.

tokens-load-failed = No se pudo cargar el token.
tokens-not-found-or-revoked = Token no encontrado o ya revocado.
tokens-rotated-not-found = Token rotado no encontrado.
tokens-rotated-toast = Token rotado — copie el nuevo valor.
tokens-rotate-failed = Error al rotar.

tokens-removed-toast = Token eliminado.
tokens-still-active = El token sigue activo — revóquelo primero.
tokens-remove-failed = Error al eliminar.

tokens-not-found = Token no encontrado.
tokens-update-failed = No se pudo actualizar el token.
tokens-tool-use-enabled-toast = Uso de herramientas activado para este token.
tokens-tool-use-disabled-toast = Uso de herramientas desactivado para este token.
tokens-mcp-ask-enabled-toast = Herramientas MCP "ask" a través de la API activadas para este token.
tokens-mcp-ask-disabled-toast = Herramientas MCP "ask" a través de la API desactivadas para este token.

tokens-unknown-tool = Herramienta desconocida.
tokens-save-pref-failed = No se pudo guardar la preferencia.
tokens-capability-enabled-toast = { $name } activado para este token.
tokens-capability-disabled-toast = { $name } desactivado para este token.

# Web Push "turn complete" opt-in card (rendered by `render_push_card`; wired
# client-side by `ui/ts/push.ts`). Device-local notification settings.
tokens-push-heading = Notificaciones
tokens-push-description = Recibe una notificación en este dispositivo cuando termine una respuesta que iniciaste mientras estás fuera de la aplicación.
tokens-push-enable = Activar en este dispositivo
tokens-push-disable = Desactivar en este dispositivo
tokens-push-on = Las notificaciones están activadas para este dispositivo.
tokens-push-off = Las notificaciones están desactivadas para este dispositivo.
tokens-push-denied = Este navegador ha bloqueado las notificaciones. Permítelas en la configuración del navegador para activarlas.
tokens-push-unsupported = Este navegador no admite notificaciones.
tokens-push-enabled = Notificaciones activadas en este dispositivo.
tokens-push-disabled = Notificaciones desactivadas en este dispositivo.
tokens-push-error = No se pudo cambiar la configuración de notificaciones.
