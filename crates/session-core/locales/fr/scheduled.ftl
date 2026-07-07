# STATUS: llm-generated, unreviewed — pending native-speaker QA

scheduled-page-title = Actions planifiées — LLM Gateway
scheduled-edit-page-title = Modifier l'action planifiée — LLM Gateway

scheduled-heading = Actions planifiées
scheduled-intro = Exécutez automatiquement un prompt selon un planning. Chaque exécution ouvre une nouvelle conversation que vous pouvez consulter ici — choisissez un modèle, écrivez le prompt et définissez quand il doit s'exécuter.
scheduled-create-submit = Créer une action planifiée
scheduled-list-heading = Vos actions planifiées
scheduled-list-empty = Aucune action planifiée pour l'instant. Créez-en une ci-dessus.

scheduled-back = Retour
scheduled-edit-heading = Modifier l'action planifiée
scheduled-save-submit = Enregistrer les modifications

scheduled-name-label = Nom
scheduled-name-placeholder = ex. Résumé quotidien de l'actualité
scheduled-model-label = Modèle
scheduled-model-placeholder = identifiant du modèle (ex. gpt-4o-mini)
scheduled-gdpr-warning = Ce modèle n'est pas conforme au RGPD. Les exécutions planifiées lui enverront automatiquement votre prompt — évitez les données personnelles.
scheduled-nda-warning = Ce modèle n'est pas couvert par un accord de confidentialité. Ne planifiez pas de contenu protégé par NDA ou propriétaire vers ce modèle.
scheduled-prompt-label = Prompt
scheduled-prompt-placeholder = Que doit faire le modèle à chaque exécution ?
scheduled-tools-toggle-label = Autoriser les outils (recherche web, RAG, pièces jointes) — comme dans le chat
scheduled-reuse-toggle-label = Réutiliser la conversation de l'exécution précédente — chaque exécution poursuit la même conversation
scheduled-reuse-rounds-prefix = envoyer les
scheduled-reuse-rounds-aria = Nombre de tours d'historique à rejouer
scheduled-reuse-rounds-suffix = derniers tours

scheduled-builder-heading = Planning
scheduled-mode-hourly = Toutes les heures
scheduled-mode-daily = Quotidien
scheduled-mode-weekly = Hebdomadaire
scheduled-mode-monthly = Mensuel
scheduled-mode-advanced = Avancé
scheduled-weekday-mon = Lun
scheduled-weekday-tue = Mar
scheduled-weekday-wed = Mer
scheduled-weekday-thu = Jeu
scheduled-weekday-fri = Ven
scheduled-weekday-sat = Sam
scheduled-weekday-sun = Dim
scheduled-on-day-label = Le jour
scheduled-of-every-month = de chaque mois
scheduled-at-label = À
scheduled-hour-aria = Heure
scheduled-minute-aria = Minute
scheduled-of-every-hour = de chaque heure
scheduled-timezone-label = Fuseau horaire
scheduled-timezone-placeholder = Europe/Berlin
scheduled-cron-label = Expression cron
scheduled-cron-help = Cinq champs : minute heure jour-du-mois mois jour-de-semaine.

scheduled-no-upcoming-runs = Aucune exécution à venir.
scheduled-next-runs-prefix = Prochaines exécutions :{ " " }

scheduled-err-pick-weekday = Choisissez au moins un jour de la semaine.
scheduled-err-enter-cron = Saisissez une expression cron.
scheduled-err-unknown-schedule-type = Type de planification inconnu « { $kind } ».
scheduled-field-minute = minute
scheduled-field-hour = heure
scheduled-field-day-of-month = jour du mois
scheduled-err-enter-field = Saisissez { $field }.
scheduled-err-invalid-field = { $field } invalide : { $value }.
scheduled-err-field-range = { $field } doit être compris entre { $min } et { $max }.
scheduled-err-name-length = Le nom doit contenir entre 1 et 128 caractères.
scheduled-err-prompt-length = Le prompt doit contenir entre 1 et 8000 caractères.
scheduled-err-pick-model = Choisissez un modèle.
scheduled-err-unknown-timezone = Fuseau horaire inconnu « { $tz } ».

scheduled-model-non-gdpr = { $model } (non conforme RGPD)
scheduled-model-nda-restricted = { $model } (restreint par confidentialité)
scheduled-model-non-gdpr-nda-restricted = { $model } (non conforme RGPD, restreint par confidentialité)

scheduled-toast-save-failed = Impossible d'enregistrer la planification.
scheduled-toast-created = Action planifiée créée.
scheduled-toast-updated = Planification mise à jour.
scheduled-toast-not-found = Aucune action planifiée de ce type.
scheduled-toast-update-failed = Impossible de mettre à jour la planification.
scheduled-toast-resumed = Planification reprise.
scheduled-toast-paused = Planification suspendue.
scheduled-toast-refresh-failed = Impossible d'actualiser la planification.
scheduled-toast-deleted = Action planifiée supprimée.
scheduled-toast-already-gone = Déjà supprimée.
scheduled-toast-delete-failed = Impossible de supprimer la planification.

scheduled-badge-active = active
scheduled-badge-paused = suspendue
scheduled-status-paused = Suspendue
scheduled-next-run = Prochaine exécution : { $when }
scheduled-no-upcoming-run = Aucune exécution à venir
scheduled-last-success = Dernière : ✓ { $when }
scheduled-last-success-open = Dernière : ✓ { $when } — ouvrir
scheduled-last-failure = Dernière : ✗ { $when }
scheduled-last-failure-open = Dernière : ✗ { $when } — ouvrir
scheduled-pause-title = Suspendre
scheduled-resume-title = Reprendre
scheduled-edit-title = Modifier
scheduled-delete-title = Supprimer
