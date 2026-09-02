# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Asistente de configuración del despliegue (/setup).

setup-step-1-of-2 = Paso 1 de 2
setup-provider-heading = Conecta tu proveedor de identidad
setup-provider-intro = Esta pasarela no tiene cuentas propias: la gente inicia sesión a través de tu proveedor OIDC. Introdúcelo abajo e intentaremos un inicio de sesión real antes de guardar nada.

setup-field-public-url = URL pública de esta pasarela
setup-field-public-url-help = La dirección que abrirán tus usuarios. Debe coincidir exactamente, incluido https, porque las redirecciones de inicio de sesión se construyen a partir de ella.

setup-redirect-uri-heading = Autoriza esta URI de redirección en tu proveedor
setup-redirect-uri-help = Añádela a las URI de redirección permitidas del cliente antes de continuar. Un proveedor que no la reconozca rechazará el inicio de sesión.

setup-field-issuer = URL del emisor
setup-field-issuer-help = Cópiala exactamente como la indica tu proveedor: la barra final importa. Keycloak la omite, Authentik la espera.

setup-field-client-id = ID de cliente
setup-field-client-secret = Secreto de cliente

setup-field-scopes = Scopes
setup-field-scopes-help = Separados por espacios. openid siempre se solicita. Conserva el que transporta la pertenencia a grupos.

setup-field-roles-claim = Claim de grupo
setup-field-roles-claim-help = Qué claim enumera los grupos de un usuario. ¿Dudas? Déjalo y elígelo desde tu propio token en la siguiente pantalla.

setup-test-button = Iniciar sesión para probar
setup-test-button-help = Todavía no se guarda nada. Volverás aquí después de iniciar sesión.

setup-step-2-of-2 = Paso 2 de 2
setup-admin-heading = Elige quién administra esta pasarela
setup-login-worked = El inicio de sesión funcionó. Tu proveedor te identificó como:
setup-admin-intro = Abajo está lo que tu proveedor dijo realmente sobre ti. Elige el grupo que debe conceder acceso administrativo completo: cualquier otra persona que inicie sesión obtiene una cuenta normal.
setup-no-claims = Tu proveedor no envió ningún claim parecido a un grupo. Escribe el claim y el valor a mano abajo, o añade un scope groups al cliente e inténtalo de nuevo.

setup-or-manual = o introdúcelo manualmente
setup-manual-claim = Claim
setup-manual-value = Valor
setup-manual-help = Úsalo si el grupo que debe ser administrador no es uno al que perteneces. Un valor introducido aquí prevalece sobre la selección de arriba.

setup-finish-button = Finalizar la configuración
setup-back-button = Volver a los ajustes del proveedor
setup-show-token = Mostrar todo lo que envió el proveedor
