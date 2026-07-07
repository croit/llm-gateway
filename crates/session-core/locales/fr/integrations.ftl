# STATUS: llm-generated, unreviewed — pending native-speaker QA

integrations-page-title = Intégrations — LLM Gateway
integrations-heading = Intégrations
integrations-intro = Connectez vos propres comptes pour que l'assistant puisse agir en votre nom — lire vos e-mails, votre calendrier, vos fichiers, vos dépôts, et plus encore. Chaque connexion utilise vos propres autorisations et peut être déconnectée à tout moment.
integrations-empty = Aucun connecteur n'est encore disponible. Un administrateur peut les activer dans Admin → Connecteurs.

integrations-badge-connected = Connecté
integrations-badge-needs-reconnect = Reconnexion nécessaire
integrations-badge-needs-admin-setup = Configuration admin nécessaire

integrations-reconnect-title = Rétablir la connexion (réauthentification / nouvelle tentative)
integrations-reconnect-button = Reconnecter
integrations-disconnect-button = Déconnecter
integrations-disconnect-confirm = Déconnecter cette intégration ? Votre jeton d'accès enregistré sera supprimé.
integrations-connect-button = Connecter

integrations-token-label = Votre jeton API
integrations-token-placeholder = collez votre jeton

integrations-tools-error-prefix = Impossible de charger les outils de ce connecteur :
integrations-tools-error-hint = Vérifiez l'URL du serveur MCP / votre jeton, puis utilisez Reconnecter ci-dessus.
integrations-tools-empty = Ce connecteur n'expose aucun outil.
integrations-tools-header = Autorisations des outils ({ $count })
integrations-set-all-label = Tout définir :
integrations-mode-always = Toujours
integrations-mode-ask = Demander
integrations-mode-off = Désactivé
integrations-tools-toggle = Afficher / masquer les outils individuels
integrations-tool-kind-read = lecture
integrations-tool-kind-write = écriture

integrations-error-unknown-connector = connecteur inconnu ou désactivé
integrations-error-forbidden-role = vous n'avez pas accès à ce connecteur
integrations-error-not-oauth = ce connecteur n'utilise pas OAuth
integrations-error-oauth-discovery-failed = échec de la découverte OAuth : { $error }
integrations-error-needs-setup-no-client = ce connecteur nécessite une configuration : aucun identifiant client n'est configuré et le fournisseur n'offre pas d'enregistrement dynamique. Demandez à un administrateur d'ajouter un client OAuth.
integrations-error-sealing-client-secret = scellement du secret client : { $error }
integrations-error-dcr-failed = l'enregistrement dynamique du client a échoué : { $error }
integrations-error-needs-setup-admin = ce connecteur nécessite une configuration : un administrateur doit configurer un identifiant client OAuth.
integrations-error-building-authorize-url = construction de l'URL d'autorisation : { $error }
integrations-error-persisting-authorization = enregistrement de l'autorisation : { $error }
integrations-error-provider-error = le fournisseur a renvoyé une erreur : { $error } { $desc }
integrations-error-callback-missing = il manque le code ou l'état dans le retour d'appel
integrations-error-auth-expired = cette autorisation a expiré ou a déjà été utilisée — recommencez depuis Intégrations
integrations-error-loading-authorization = chargement de l'autorisation : { $error }
integrations-error-state-mismatch = l'état d'autorisation ne correspondait pas à votre session
integrations-error-connector-missing = le connecteur n'existe plus
integrations-error-decrypting-client-secret = déchiffrement du secret client : { $error }
integrations-error-connector-missing-client-id = il manque l'identifiant client OAuth du connecteur
integrations-error-sealing-access-token = scellement du jeton d'accès : { $error }
integrations-error-sealing-refresh-token = scellement du jeton de rafraîchissement : { $error }
integrations-error-saving-connection = enregistrement de la connexion : { $error }
integrations-error-not-token-based = ce connecteur n'est pas basé sur un jeton
integrations-error-token-required = un jeton est requis
integrations-error-sealing-token = scellement du jeton : { $error }
integrations-error-unknown-connector-plain = connecteur inconnu
integrations-error-invalid-mode = mode d'autorisation invalide
integrations-error-saving-tool-permission = enregistrement de l'autorisation de l'outil : { $error }
integrations-error-saving-permissions = enregistrement des autorisations : { $error }
integrations-error-listing-tools = liste des outils : { $error }
integrations-error-disconnecting = déconnexion : { $error }
integrations-error-connection-unavailable = connexion indisponible
