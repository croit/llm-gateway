# STATUS: llm-generated, unreviewed — pending native-speaker QA

rag-page-title = Collections RAG — LLM Gateway
rag-heading = Collections RAG
rag-description-prefix = Bases de code que la passerelle a indexées. L'outil
rag-description-suffix = interroge ces collections pour répondre aux questions sur le code.
rag-collections-heading = Collections configurées
rag-empty-list = Aucune collection pour l'instant. Créez-en une ci-dessus.

# Toasts — collection CRUD
rag-toast-malformed-form = formulaire invalide : { $err }
rag-toast-name-exists = une collection nommée `{ $name }` existe déjà
rag-toast-create-failed = impossible de créer la collection
rag-toast-indexing-queued = L'indexation de `{ $name }` @ `{ $ref }` a été mise en file d'attente.
rag-toast-created-aggregate = `{ $name }` créée (agrégat). Ajoutez les dépôts sources ci-dessous pour les indexer.
rag-toast-collection-not-found = collection introuvable
rag-toast-collection-not-found-cap = Collection introuvable.
rag-toast-load-collection-failed = impossible de charger la collection
rag-toast-load-collection-failed-cap = Impossible de charger la collection.
rag-toast-name-length = Le nom doit contenir entre 1 et 64 caractères.
rag-toast-git-url-required = L'URL Git est requise.
rag-toast-embedding-model-required = Le modèle d'embedding est requis.
rag-toast-chunk-size-range = La taille du chunk doit être dans (0, 8000].
rag-toast-chunk-overlap-range = Le chevauchement du chunk doit être dans [0, chunk_size).
rag-toast-save-failed = Échec de l'enregistrement de la collection.
rag-toast-vanished = La collection a disparu après l'enregistrement.
rag-toast-saved-reload-failed = Enregistré, mais le rechargement a échoué.
rag-toast-saved = `{ $name }` enregistrée.
rag-toast-collection-removed = Collection supprimée.
rag-toast-collection-already-gone = Collection déjà supprimée.
rag-toast-delete-failed = Échec de la suppression.

# Toasts — refs / sources
rag-toast-reindex-queue-failed = impossible de planifier la réindexation
rag-toast-reindex-queued-count = Réindexation de { $count } référence(s) mise en file d'attente.
rag-toast-ref-required = La référence (branche/tag/commit) est requise.
rag-toast-ref-exists = la référence `{ $ref }` existe déjà pour cette collection
rag-toast-add-ref-failed = impossible d'ajouter la référence
rag-toast-indexing-queued-ref = Indexation de `{ $ref }` mise en file d'attente.
rag-toast-no-source-urls = Aucune URL source trouvée.
rag-toast-bulk-queued-skipped = { $added } source(s) mise(s) en file d'attente ; { $skipped } doublon(s) ignoré(s).
rag-toast-bulk-queued = Indexation de { $added } source(s) mise en file d'attente.
rag-toast-ref-not-found = référence introuvable
rag-toast-reindex-queued-ref = Réindexation de `{ $ref }` mise en file d'attente.
rag-toast-set-primary-failed = impossible de définir la référence principale
rag-toast-now-default = `{ $ref }` est désormais la référence par défaut.
rag-toast-delete-ref-failed = impossible de supprimer la référence
rag-toast-ref-removed = Référence `{ $ref }` supprimée.
rag-toast-load-log-failed = impossible de charger le journal
rag-toast-git-url-required-aggregate = L'URL Git est requise pour une source d'agrégat.
rag-toast-update-source-failed = impossible de mettre à jour la source
rag-toast-source-updated = Source mise à jour.

# Status badges
rag-status-pending = en attente
rag-status-cloning = clonage
rag-status-indexing = indexation
rag-status-ready = prêt
rag-status-error = erreur

# Collection row
rag-pat-set = PAT défini
rag-pat-none = pas de PAT
rag-meta-aggregate = { $count } source(s) · { $hint }
rag-meta-versioned = { $url } · { $hint }
rag-badge-aggregate = agrégat
rag-embed-prefix = embed :
rag-button-edit = Modifier
rag-button-delete-collection = Supprimer la collection
rag-placeholder-source-git-url = https://github.com/org/repo.git
rag-placeholder-ref-default = référence (par défaut : celle de la collection)
rag-button-add-source = Ajouter une source
rag-placeholder-branch-tag-commit = branche, tag ou commit
rag-button-add-ref = Ajouter une référence
rag-placeholder-bulk-sources = Ajout en masse — un dépôt par ligne, @ref facultatif :
    https://github.com/proxmox/pve-manager.git
    https://github.com/proxmox/qemu-server.git @master
rag-button-add-bulk = Ajouter des sources (en masse)

# Ref / source row
rag-badge-primary = principale
rag-ref-indexed-line = indexé { $date } · { $commit }
rag-never = jamais
rag-button-log = Journal
rag-button-reindex = Réindexer
rag-button-set-primary = Définir comme principale
rag-button-remove = Supprimer

# Indexing log
rag-log-info = info
rag-log-warn = avertissement
rag-log-error = erreur
rag-log-heading = Journal d'indexation
rag-log-empty = Aucun événement d'indexation enregistré pour l'instant. La première exécution s'enregistrera ici dès que l'indexeur traitera cette référence.

# Inline per-source editor
rag-label-git-url-source = URL Git (cette source)
rag-label-git-url-inherit = URL Git (vide = hériter de la collection)
rag-placeholder-git-url = https://example.com/org/repo.git
rag-label-branch-tag = Branche / tag
rag-button-save-source = Enregistrer la source
rag-button-cancel = Annuler

# Create-collection form
rag-create-heading = Indexer une nouvelle collection
rag-create-description = L'indexeur clone le dépôt, découpe chaque fichier en chunks et les embedde via le modèle d'embedding configuré. Les PAT sont stockés en clair (la passerelle s'exécute sur une infrastructure de confiance).
rag-label-name = Nom
rag-placeholder-name = ex. gateway-repo
rag-label-description-optional = Description (facultatif)
rag-placeholder-description = courte, lisible
rag-label-git-url-versioned = URL Git (versionné uniquement)
rag-label-pat-optional = Jeton d'accès personnel (facultatif)
rag-placeholder-pat = pour les dépôts privés
rag-label-include-globs-full = Inclusions (globs séparés par des virgules ou des retours à la ligne)
rag-placeholder-include-globs = *.rs, *.md
rag-label-exclude-globs = Exclusions (globs)
rag-placeholder-exclude-globs = target/, node_modules/
rag-label-chunk-size = Taille du chunk
rag-label-chunk-overlap = Chevauchement du chunk
rag-create-aggregate-help = Agrégat (multi-source) : recherche dans de nombreux dépôts comme un seul corpus. Laissez l'URL Git vide et ajoutez chaque dépôt source après la création. La branche / le tag devient la référence par défaut des sources ajoutées.
rag-button-queue-indexing = Planifier l'indexation

# Edit-collection form
rag-edit-heading = Modification de { $name }
rag-label-description = Description
rag-label-pat = Jeton d'accès personnel
rag-badge-pat-set = actuellement défini
rag-badge-pat-none = aucun enregistré
rag-placeholder-pat-keep = laisser vide pour conserver l'existant
rag-label-clear-pat = Supprimer le PAT enregistré (ne plus s'authentifier)
rag-label-include-globs = Inclusions (globs)
rag-button-save-changes = Enregistrer les modifications

# Embedding model field
rag-label-embedding-model = Modèle d'embedding
rag-placeholder-embedding-model-none = aucun pool d'embedding configuré — saisissez un identifiant de modèle
rag-option-choose-embedding-model = Choisir un modèle d'embedding…
rag-suffix-not-advertised = (plus proposé)

rag-label-allowed-groups = Groupes autorisÃ©s
rag-hint-allowed-groups = Groupes du gateway (sÃ©parÃ©s par des virgules) autorisÃ©s Ã  lister et rechercher cette collection. Vide = tout le monde disposant des outils RAG. Les admins ont toujours accÃ¨s.
