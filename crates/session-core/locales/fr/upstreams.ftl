# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/upstreams.rs` — la page
# fusionnée `/admin/upstreams` (pools + backends).

upstreams-page-title = Upstreams — LLM Gateway
upstreams-heading = Upstreams
upstreams-description = Les pools regroupent les backends par type et par stratégie de sélection. La santé, la charge et les modèles servis sont sondés en direct. Les modifications de topologie sont enregistrées en base et prennent effet lors de l'application des changements.

upstreams-add-pool = Pool
upstreams-add-backend = Backend
upstreams-cancel = Annuler
upstreams-edit-pool = Modifier le pool
upstreams-edit-backend = Modifier le backend
upstreams-delete-confirm = Vraiment supprimer ?

upstreams-apply-count = modifications non appliquées
upstreams-apply-note = — le registre d'exécution sert encore la topologie précédente.

upstreams-comp-gdpr = RGPD
upstreams-comp-nda = NDA
upstreams-comp-limits = limites

upstreams-backend-pending = en attente

# Info-bulle sur une puce de modèle barrée : découvert via /models mais retenu
# car la liste de modèles du pool (liste d'autorisation) ne le nomme pas.
upstreams-model-withheld-title = Découvert via /models mais retenu par la liste de modèles de ce pool — ni servi ni annoncé.

upstreams-unassigned-heading = Non assignés
upstreams-unassigned-description = Backends non assignés à un pool. Ajoutez-en un à un pool pour y acheminer le trafic.

upstreams-empty = Aucun pool ni backend configuré pour l'instant. Ajoutez un pool ou un backend pour commencer.
