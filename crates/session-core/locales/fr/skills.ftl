# STATUS: llm-generated, unreviewed — pending native-speaker QA

skills-error-not-configured = Les skills ne sont pas configurés ([skills] dir n'est pas défini).
skills-error-no-file = Aucun fichier n'a été envoyé — choisissez une archive .skill.
skills-error-install-failed = Impossible d'installer le skill : { $error }
skills-error-bad-delete-request = Requête de suppression invalide : { $error }
skills-error-delete-failed = Impossible de supprimer le skill : { $error }
skills-page-title = Skills — LLM Gateway

skills-heading = Skills
skills-intro-part1 = Instructions installées par l'opérateur que le modèle de chat charge à la demande grâce à l'outil
skills-intro-part2 = prévu à cet effet. Téléversez une archive
skills-intro-part3 = ci-dessous — elle est disponible immédiatement, sans redémarrage.
skills-empty-loaded = Aucun skill chargé pour le moment. Téléversez une archive .skill pour en ajouter un.
skills-empty-not-configured = Les skills ne sont pas configurés. Définissez [skills] dir dans la configuration de la passerelle et redémarrez pour les activer.

skills-upload-heading = Ajouter un skill
skills-upload-button = Téléverser .skill
skills-loaded-heading = Skills chargés
skills-none-yet = Aucun pour le moment
skills-source-prefix = Source :

skills-download-title = Télécharger ce skill sous forme d'archive .skill
skills-download-button = Télécharger
skills-delete-title = Supprimer ce skill
skills-delete-button = Supprimer
skills-granted-to-heading = Accordé à
skills-granted-config-title = Accordé dans la configuration de la passerelle ([[roles]].skills)
skills-choose-access-title = Choisissez les rôles autorisés à utiliser ce skill
skills-no-grants-warning = aucun rôle ne l'accorde — définir l'accès
skills-edit-access-title = Modifier les rôles autorisés à utiliser ce skill
skills-edit-access-button = Modifier l'accès
skills-files-heading = Fichiers
skills-files-count = { $count } inclus
skills-description-heading = Description

skills-grant-dialog-heading = Qui peut utiliser ce skill ?
skills-grant-dialog-desc-part1 = Choisissez les rôles autorisés à charger ce skill :
skills-grant-dialog-desc-part2 = . Chaque personne ayant un rôle sélectionné y a accès.
skills-grant-dialog-no-roles-part1 = Aucun rôle n'est défini dans la configuration de la passerelle. Ajoutez des entrées
skills-grant-dialog-no-roles-part2 = avant de pouvoir accorder l'accès.
skills-cancel-button = Annuler
skills-save-access-button = Enregistrer l'accès

skills-from-config-badge = depuis la configuration

skills-error-no-dir-access = Pas d’accès au répertoire des compétences — vérifiez qu’il existe et que la passerelle peut y lire et écrire :
