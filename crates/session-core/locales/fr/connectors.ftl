# STATUS: llm-generated, unreviewed — pending native-speaker QA

connectors-page-title = Connecteurs — LLM Gateway
connectors-heading = Connecteurs
connectors-restore-defaults-button = Restaurer les valeurs par défaut
connectors-catalog-intro = Gérez les serveurs MCP que les utilisateurs peuvent connecter depuis Intégrations. Activez un connecteur pour le rendre visible. Les connecteurs qui ne peuvent pas utiliser l'enregistrement dynamique de client (par ex. Google) nécessitent un identifiant/secret client OAuth de déploiement avant de pouvoir être activés.
connectors-empty-state = Aucun connecteur pour l'instant.

connectors-badge-enabled = Activé
connectors-badge-disabled = Désactivé
connectors-badge-default = Par défaut
connectors-badge-dcr = DCR
connectors-badge-needs-client-id = Identifiant client requis
connectors-disable-button = Désactiver
connectors-enable-disabled-title = Ajoutez d'abord l'identifiant client OAuth ci-dessous (Modifier → Identifiant client OAuth)
connectors-enable-button = Activer
connectors-delete-confirm = Supprimer ce connecteur ? Il sera retiré pour tous les utilisateurs, ainsi que leurs connexions et jetons enregistrés. Cette action est irréversible.
connectors-delete-button = Supprimer
connectors-edit-summary = Modifier

connectors-add-summary = Ajouter un connecteur

connectors-oauth-help-token-1 = Connecteur à jeton : définissez l'URL du serveur MCP ci-dessus ; chaque utilisateur colle son propre jeton API dans Intégrations (envoyé comme
connectors-oauth-help-token-2 = ). Aucun client OAuth requis.

connectors-oauth-help-dcr-heading = Enregistrement dynamique de client — aucun client OAuth requis
connectors-oauth-help-dcr-body = Définissez simplement l'URL du serveur MCP ci-dessus. Le serveur enregistre automatiquement cette passerelle (RFC 7591) ; chaque utilisateur clique ensuite sur Connecter et s'autorise avec son propre compte — une seule connexion couvre tous les services exposés par le serveur.

connectors-oauth-help-gws-1 = Pointez ceci vers votre
connectors-oauth-help-gws-self-hosted = serveur MCP Google Workspace auto-hébergé
connectors-oauth-help-gws-2 = (par ex.
connectors-oauth-help-gws-3 = ) fonctionnant en mode streamable-HTTP — l'URL se termine par
connectors-oauth-help-gws-4 = . Ce serveur détient le client OAuth Google et utilise les
connectors-oauth-help-gws-ga-apis = API Google GA
connectors-oauth-help-gws-5 = (pas de developer preview). Autorisez l'URI de redirection de cette passerelle sur le serveur via
connectors-oauth-help-gws-footer = Les points de terminaison MCP hébergés par Google (gmailmcp/calendarmcp/drivemcp.googleapis.com) ne sont volontairement pas utilisés — ils nécessitent l'inscription de l'organisation au programme Workspace Developer Preview. Voir docs/connectors.md pour la procédure de déploiement.

connectors-oauth-help-generic-heading = Configuration du client OAuth
connectors-oauth-help-generic-intro = Enregistrez cette URI de redirection exacte auprès de votre client OAuth, puis collez son identifiant client (et son secret) ci-dessous :
connectors-oauth-help-google-1 = Google : créez un
connectors-oauth-help-google-link = identifiant client OAuth 2.0 (application Web)
connectors-oauth-help-google-2 = dans la Google Cloud Console, ajoutez l'URI de redirection ci-dessus, et activez les API Gmail / Google Agenda / Google Drive pour le projet.
connectors-oauth-help-github-1 = GitHub : créez une
connectors-oauth-help-github-link = application OAuth
connectors-oauth-help-github-2 = (Paramètres → Paramètres développeur → Applications OAuth), définissez l'URL de rappel d'autorisation sur l'URI de redirection ci-dessus, et copiez l'identifiant client ainsi qu'un secret client généré.
connectors-oauth-help-fallback = Créez un client OAuth chez votre fournisseur avec cette URI de redirection et les URL d'autorisation/de jeton définies ci-dessous.
connectors-oauth-why-1 = Pourquoi une étape d'admin ponctuelle ? En OAuth, l'identifiant client identifie
connectors-term-this-gateway = cette passerelle
connectors-oauth-why-2 = en tant qu'application (partagée par tous les utilisateurs) — seul le jeton d'accès par utilisateur diffère. Claude Desktop s'en passe car Anthropic fournit des applications préenregistrées liées à son URL de redirection fixe ; une passerelle auto-hébergée utilise sa propre URI de redirection (ci-dessus), et Google/GitHub ne prennent pas en charge l'enregistrement automatique (DCR) comme le fait Atlassian — vous enregistrez donc une fois, puis chaque utilisateur n'a plus qu'à cliquer sur Connecter.
connectors-oauth-why-no-app = Aucune application OAuth du tout ?
connectors-oauth-why-3 = Passez l'authentification sur « Jeton fourni par l'utilisateur » : chaque utilisateur colle alors son propre jeton (par ex. un jeton d'accès personnel GitHub) — les identifiants proviennent alors directement de l'utilisateur, sans client admin.

connectors-field-key-label = Clé (identifiant stable)
connectors-field-key-placeholder = par ex. gmail
connectors-field-key-readonly-label = Clé
connectors-field-name-label = Nom
connectors-field-name-placeholder = Nom d'affichage
connectors-field-icon-label = Icône (emoji)
connectors-field-category-label = Catégorie
connectors-field-category-placeholder = Google
connectors-field-description-label = Description
connectors-field-description-placeholder = Ce que fait ce connecteur
connectors-field-url-label = URL du serveur MCP
connectors-field-auth-label = Authentification
connectors-auth-option-oauth = OAuth 2.1 (chaque utilisateur s'autorise via le fournisseur)
connectors-auth-option-token = Jeton fourni par l'utilisateur (chaque utilisateur colle son propre jeton API)
connectors-auth-option-none = Aucune (serveur public, sans authentification)
connectors-field-client-json-label = Coller le JSON du client OAuth (optionnel — par ex. « Télécharger le fichier JSON » de Google)
connectors-field-client-json-help = Renseigne l'identifiant/le secret client (ainsi que les URL d'autorisation et de jeton) à partir du fichier. Ou utilisez les champs individuels ci-dessous.
connectors-field-client-id-label = Identifiant client OAuth
connectors-field-client-id-placeholder = …apps.googleusercontent.com / identifiant d'application OAuth GitHub
connectors-field-client-id-help-1 = L'identifiant public qui identifie
connectors-field-client-id-help-2 = en tant qu'application auprès du fournisseur — créé une fois par un administrateur sur la page des identifiants OAuth du fournisseur (Google Cloud → Identifiants, GitHub → Applications OAuth). Ce n'est pas un secret propre à l'utilisateur. Laissez vide si DCR est activé.
connectors-field-client-secret-label = Secret client OAuth
connectors-secret-placeholder-existing = •••••••• (laisser vide pour conserver)
connectors-secret-placeholder-new = secret client (optionnel)
connectors-field-client-secret-help = Délivré en même temps que l'identifiant client sur la même page. Stocké chiffré ; laissez vide pour conserver celui existant.
connectors-field-use-dcr-label = Essayer l'enregistrement dynamique de client (RFC 7591)
connectors-field-scopes-label = Scopes (séparés par des espaces)
connectors-advanced-summary = Avancé : substitutions de découverte
connectors-field-authorize-url-label = URL d'autorisation
connectors-field-token-url-label = URL du jeton
connectors-field-registration-url-label = URL d'enregistrement
connectors-placeholder-optional-override = substitution optionnelle
connectors-field-required-role-label = Rôle requis (verrou RBAC)
connectors-placeholder-optional = optionnel
connectors-save-changes-button = Enregistrer les modifications
connectors-add-connector-button = Ajouter un connecteur

connectors-error-missing-fields = la clé, le nom et l'URL sont requis
connectors-error-bad-client-json = impossible de lire un client_id depuis le JSON collé — le fichier client OAuth Google attendu est ({"{"}"web":{"{"}"client_id":…,"client_secret":…{"}"}{"}"}).
connectors-error-sealing-secret = scellement du secret : { $error }
connectors-error-saving = enregistrement du connecteur : { $error }
connectors-error-needs-client-id = ce connecteur nécessite un identifiant client OAuth avant de pouvoir être activé (il ne peut pas utiliser l'enregistrement dynamique). Modifiez-le et ajoutez l'identifiant/le secret client.
connectors-error-toggling = bascule du connecteur : { $error }
connectors-error-deleting = suppression du connecteur : { $error }
connectors-error-restoring = restauration des valeurs par défaut : { $error }
