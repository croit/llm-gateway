# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Éditeur d'administration des limites de débit / quotas (/admin/limits).
limits-heading = Limites de débit et quotas
limits-intro = Limitez le nombre de requêtes, de tokens ou le montant des dépenses qu'un appelant peut utiliser sur une fenêtre glissante. Les règles sont résolues du plus spécifique au plus général : la règle propre à un utilisateur l'emporte, sinon la plus généreuse de ses rôles, sinon la valeur par défaut globale. Sans règle, tout le monde est illimité. Une règle portant sur un jeton d'API est un plafond supplémentaire vérifié en plus du budget de son propriétaire : elle ne peut que restreindre la dépense de ce jeton. Seuls les pools facturés comptent (les pools auto-hébergés avec enforce_limits = false sont exemptés), et l'ensemble du budget d'un utilisateur est partagé entre ses tokens API, le chat et les exécutions planifiées.
limits-add-heading = Ajouter ou mettre à jour une limite
limits-field-subject = S'applique à
limits-field-subject-id = Rôle / utilisateur / jeton
limits-field-subject-id-ph = id de rôle, e-mail d'utilisateur ou id de jeton
limits-field-model = Modèle
limits-field-model-ph = tous les modèles
limits-field-dimension = Limite
limits-field-window = Par
limits-field-value = Valeur
limits-add-submit = Enregistrer la limite
limits-subject-global = Tout le monde (par défaut)
limits-subject-role = Rôle
limits-subject-user = Utilisateur
limits-dim-requests = Requêtes
limits-dim-tokens = Tokens
limits-dim-cost = Coût ({ $cur })
limits-dim-cost-short = Coût
limits-win-hour = Heure
limits-win-day = Jour
limits-win-week = Semaine
limits-win-month = Mois
limits-col-subject = S'applique à
limits-col-scope = Modèle
limits-col-limit = Limite
limits-col-window = Fenêtre
limits-col-value = Valeur
limits-col-actions = Actions
limits-none = Aucune limite configurée — tout le monde est illimité.
limits-all-models = tous les modèles
limits-delete = Supprimer
limits-saved = limite enregistrée pour { $subject }
limits-deleted = limite supprimée
limits-invalid-value = la valeur `{ $value }` doit être un nombre non négatif
limits-unknown-role = rôle inconnu `{ $role }`
limits-unknown-user = aucun utilisateur ne correspond à `{ $user }`
limits-missing-subject-id = saisissez un id de rôle, un e-mail d'utilisateur ou un id de jeton
limits-subject-token = Jeton d'API
limits-unknown-token = aucun jeton ne correspond à `{ $token }`
