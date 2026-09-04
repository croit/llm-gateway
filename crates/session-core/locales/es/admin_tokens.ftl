# STATUS: llm-generated, unreviewed — pending native-speaker QA

admin-tokens-page-title = Tokens de API
admin-tokens-heading = Tokens de API
admin-tokens-blurb = Todos los tokens de API de esta instalación y quién es su propietario. El token nunca se muestra: solo se guarda un SHA-256, así que no puede recuperarse aquí. Las cuotas se fijan por token en la página de límites. La lista de modelos permitidos tiene dos mitades independientes —la del propietario, en su propia página de tokens, y la tuya, abajo— y el token solo puede usar los modelos que estén en ambas, así que cada parte solo puede restringir.
admin-tokens-none = Todavía no se ha creado ningún token de API.
admin-tokens-count = { $count } token(s)
admin-tokens-col-name = Token
admin-tokens-col-owner = Propietario
admin-tokens-col-state = Estado
admin-tokens-col-dates = Creado / usado / caduca
admin-tokens-col-scope = Modelos y cuota
admin-tokens-badge-expired = Caducado
admin-tokens-models-summary-all = Modelos: todos (sin restricción del operador)
admin-tokens-models-summary-restricted = Modelos: el operador permite { $count }
admin-tokens-models-help = Una restricción del operador sobre este token, independiente de la del propietario. El token solo puede usar modelos que estén en ambas listas, así que marcar uno aquí no concede un modelo que su propietario excluyó, y el propietario no puede volver a conceder uno que tú quites.
admin-tokens-models-restrict-label = Limitar este token a modelos concretos
admin-tokens-models-saved-toast = Restricción del operador fijada: { $count } modelos.
admin-tokens-models-cleared-toast = Restricción del operador eliminada.
