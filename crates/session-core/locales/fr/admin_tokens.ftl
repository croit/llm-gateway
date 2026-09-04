# STATUS: llm-generated, unreviewed — pending native-speaker QA

admin-tokens-page-title = Jetons d'API
admin-tokens-heading = Jetons d'API
admin-tokens-blurb = Tous les jetons d'API de cette installation et leur propriétaire. Le jeton lui-même n'est jamais affiché — seul un SHA-256 est stocké, il est donc impossible de le récupérer ici. Les quotas se règlent par jeton sur la page des limites. La liste des modèles autorisés a deux moitiés indépendantes — celle du propriétaire, sur sa page de jetons, et la vôtre ci-dessous — et le jeton ne peut utiliser que les modèles présents sur les deux : chaque partie ne peut donc que restreindre.
admin-tokens-none = Aucun jeton d'API n'a encore été créé.
admin-tokens-count = { $count } jeton(s)
admin-tokens-col-name = Jeton
admin-tokens-col-owner = Propriétaire
admin-tokens-col-state = État
admin-tokens-col-dates = Créé / utilisé / expire
admin-tokens-col-scope = Modèles et quota
admin-tokens-badge-expired = Expiré
admin-tokens-models-summary-all = Modèles : tous (aucune restriction de l'opérateur)
admin-tokens-models-summary-restricted = Modèles : l'opérateur en autorise { $count }
admin-tokens-models-help = Une restriction de l'opérateur sur ce jeton, distincte de celle du propriétaire. Le jeton ne peut utiliser que les modèles présents sur les deux listes : cocher ici n'accorde donc pas un modèle exclu par le propriétaire, et celui-ci ne peut pas réaccorder un modèle que vous retirez.
admin-tokens-models-restrict-label = Limiter ce jeton à des modèles précis
admin-tokens-models-saved-toast = Restriction de l'opérateur définie : { $count } modèles.
admin-tokens-models-cleared-toast = Restriction de l'opérateur supprimée.
