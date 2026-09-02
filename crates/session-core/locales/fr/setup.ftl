# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Assistant de configuration du déploiement (/setup).

setup-step-1-of-2 = Étape 1 sur 2
setup-provider-heading = Connecter votre fournisseur d'identité
setup-provider-intro = Cette passerelle n'a pas de comptes propres — les utilisateurs se connectent via votre fournisseur OIDC. Saisissez-le ci-dessous : nous tenterons une vraie connexion avant d'enregistrer quoi que ce soit.

setup-field-public-url = URL publique de cette passerelle
setup-field-public-url-help = L'adresse que vos utilisateurs ouvriront. Elle doit correspondre exactement, https compris, car les redirections de connexion en sont dérivées.

setup-redirect-uri-heading = Autorisez cette URI de redirection chez votre fournisseur
setup-redirect-uri-help = Ajoutez-la aux URI de redirection autorisées du client avant de continuer. Un fournisseur qui ne la reconnaît pas refusera la connexion.

setup-field-issuer = URL de l'émetteur
setup-field-issuer-help = Copiez-la exactement telle que votre fournisseur l'indique — la barre oblique finale compte. Keycloak l'omet, Authentik l'attend.

setup-field-client-id = Identifiant client
setup-field-client-secret = Secret client

setup-field-scopes = Scopes
setup-field-scopes-help = Séparés par des espaces. openid est toujours demandé. Conservez celui qui transporte l'appartenance aux groupes.

setup-field-roles-claim = Claim de groupe
setup-field-roles-claim-help = Le claim qui liste les groupes d'un utilisateur. Vous hésitez ? Laissez-le tel quel et choisissez depuis votre propre jeton à l'étape suivante.

setup-test-button = Se connecter pour tester
setup-test-button-help = Rien n'est encore enregistré. Vous reviendrez ici après la connexion.

setup-step-2-of-2 = Étape 2 sur 2
setup-admin-heading = Choisir qui administre cette passerelle
setup-login-worked = La connexion a fonctionné. Votre fournisseur vous a identifié comme :
setup-admin-intro = Voici ce que votre fournisseur a réellement déclaré à votre sujet. Choisissez le groupe qui doit accorder l'accès administrateur complet — toute autre personne qui se connecte obtient un compte ordinaire.
setup-no-claims = Votre fournisseur n'a envoyé aucun claim ressemblant à un groupe. Saisissez le claim et la valeur à la main ci-dessous, ou ajoutez un scope groups au client et réessayez.

setup-or-manual = ou saisir manuellement
setup-manual-claim = Claim
setup-manual-value = Valeur
setup-manual-help = Utilisez ceci si le groupe administrateur n'est pas un groupe dont vous faites partie. Une valeur saisie ici a priorité sur la sélection ci-dessus.

setup-finish-button = Terminer la configuration
setup-back-button = Retour aux paramètres du fournisseur
setup-show-token = Afficher tout ce que le fournisseur a envoyé
