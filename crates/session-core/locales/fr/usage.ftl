# STATUS: llm-generated, unreviewed — pending native-speaker QA

# Strings owned by `gateway/src/rama_server/pages/usage.rs` — the
# per-user `/usage` usage-statistics page and its admin-only "all
# users" scope toggle.

usage-title-all = Utilisation — tous les utilisateurs — LLM Gateway
usage-title-mine = Votre utilisation — LLM Gateway

usage-heading-all = Utilisation — tous les utilisateurs
usage-heading-mine = Votre utilisation
usage-blurb-all = Volume de requêtes et consommation de tokens par utilisateur et par backend, tous moyens d'accès confondus. « Requêtes » comptabilise les appels au backend en amont ; un tour utilisant des outils (qui effectue plusieurs allers-retours) compte donc pour plus d'un.
usage-blurb-mine = Votre volume de requêtes et votre consommation de tokens sur l'interface de chat, l'API et les actions planifiées. « Requêtes » comptabilise les appels au backend en amont ; un tour utilisant des outils compte donc pour plus d'un.

usage-metrics-disabled-prefix = Les métriques d'utilisation sont désactivées (
usage-metrics-disabled-suffix = ). Les chiffres ci-dessous ne reflètent que les données enregistrées avant la désactivation.

usage-toggle-mine = Moi
usage-toggle-all = Tous les utilisateurs

usage-source-all = Toutes les sources
usage-source-api = API (/v1)
usage-source-chat = Interface de chat
usage-source-scheduled = Planifié
usage-backend-all = Tous les backends

usage-filter-period = Période
usage-filter-source = Source
usage-filter-backend = Backend
usage-apply = Appliquer

usage-stat-requests-title = Requêtes
usage-stat-requests-desc = appels au backend en amont
usage-stat-tokens-title = Tokens
usage-stat-tokens-desc = prompt + complétion
usage-stat-users-title = Utilisateurs
usage-stat-users-desc = actifs sur la période
usage-stat-errors-title = Erreurs
usage-stat-errors-desc = statut ≥ 400

usage-table-by-user = Par utilisateur
usage-table-by-backend = Par backend
usage-table-by-source = Par source
usage-table-by-model = Par modèle

usage-key-user = Utilisateur
usage-key-backend = Backend
usage-key-source = Source
usage-key-model = Modèle

usage-col-requests = Requêtes
usage-col-tokens = Tokens
usage-col-errors = Erreurs

usage-no-activity = Aucune activité sur cette période.
