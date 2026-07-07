# STATUS: llm-generated, unreviewed — pending native-speaker QA

tokens-page-title = Jetons API — LLM Gateway
tokens-page-heading = Jetons API
tokens-intro = Jetons Bearer pour l'API compatible OpenAI. Le texte en clair n'est affiché qu'à la création — conservez-le en lieu sûr.

tokens-create-heading = Créer un jeton
tokens-create-description = Créer un nouveau jeton Bearer pour l'API compatible OpenAI.
tokens-name-label = Nom
tokens-name-placeholder = ex. laptop, ci-runner
tokens-ttl-label = Durée de vie (jours)
tokens-create-submit = Créer un jeton

tokens-list-heading = Vos jetons
tokens-list-empty = Aucun jeton pour l'instant. Créez-en un ci-dessus.

tokens-badge-revoked = révoqué
tokens-badge-active = actif
tokens-remove-button = Supprimer
tokens-rotate-button = Régénérer
tokens-rotate-title = Émettre un nouveau secret pour ce jeton (conserve son nom et ses paramètres)
tokens-revoke-button = Révoquer

tokens-row-meta = créé le { $created } · dernière utilisation { $last_used } · expire le { $expires }
tokens-last-used-never = jamais

tokens-tool-use-aria = Utilisation des outils
tokens-tool-use-label = Utilisation des outils
tokens-tool-use-description = Autoriser ce jeton à appeler les outils de la passerelle (recherche web, RAG, …).
tokens-capabilities-summary = Capacités

tokens-mcp-allow-aria = Autoriser les outils MCP en mode « ask » via l'API
tokens-mcp-allow-label = Autoriser les outils MCP « ask » via l'API
tokens-mcp-allow-description = Les outils de connecteur nécessitant une approbation ne peuvent pas demander de confirmation via l'API ; l'activation les exécute sans demander.

tokens-minted-heading = Jeton créé
tokens-minted-copy-warning = Copiez la valeur maintenant — vous ne pourrez plus la revoir ensuite.
tokens-copy-aria = Copier le jeton
tokens-copy-title = Copier le jeton
tokens-minted-name = Nom : { $name }

tokens-account-heading = Compte
tokens-signed-in-as = Connecté en tant que { $email }
tokens-account-user-id-label = ID utilisateur
tokens-account-oidc-label = Rôles OIDC
tokens-account-rbac-label = IDs de rôle RBAC
tokens-roles-none = aucun
tokens-roles-none-granted = aucun accordé

tokens-malformed-form = formulaire invalide : { $err }
tokens-name-length = Le nom du jeton doit comporter entre 1 et 128 caractères.
tokens-store-failed = Échec de l'enregistrement du jeton.
tokens-created-toast = Jeton créé.

tokens-revoked-not-found = Jeton révoqué introuvable.
tokens-revoked-toast = Jeton révoqué.
tokens-already-revoked = Le jeton était déjà révoqué.
tokens-revoke-failed = Échec de la révocation.

tokens-load-failed = Impossible de charger le jeton.
tokens-not-found-or-revoked = Jeton introuvable ou déjà révoqué.
tokens-rotated-not-found = Jeton régénéré introuvable.
tokens-rotated-toast = Jeton régénéré — copiez la nouvelle valeur.
tokens-rotate-failed = Échec de la régénération.

tokens-removed-toast = Jeton supprimé.
tokens-still-active = Le jeton est encore actif — révoquez-le d'abord.
tokens-remove-failed = Échec de la suppression.

tokens-not-found = Jeton introuvable.
tokens-update-failed = Impossible de mettre à jour le jeton.
tokens-tool-use-enabled-toast = Utilisation des outils activée pour ce jeton.
tokens-tool-use-disabled-toast = Utilisation des outils désactivée pour ce jeton.
tokens-mcp-ask-enabled-toast = Outils MCP « ask » via l'API activés pour ce jeton.
tokens-mcp-ask-disabled-toast = Outils MCP « ask » via l'API désactivés pour ce jeton.

tokens-unknown-tool = Outil inconnu.
tokens-save-pref-failed = Impossible d'enregistrer la préférence.
tokens-capability-enabled-toast = { $name } activé pour ce jeton.
tokens-capability-disabled-toast = { $name } désactivé pour ce jeton.
