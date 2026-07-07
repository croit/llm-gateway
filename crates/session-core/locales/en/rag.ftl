# Strings owned by `gateway/src/rama_server/pages/rag.rs` — the
# admin-only `/rag` page: create/list/edit/delete RAG collections, their
# per-source refs, the live status poll, and the indexing log viewer.

rag-page-title = RAG collections — LLM Gateway
rag-heading = RAG collections
rag-description-prefix = Codebases the gateway has indexed. The
rag-description-suffix = tool reaches into these collections to answer questions about the code.
rag-collections-heading = Configured collections
rag-empty-list = No collections yet. Create one above.

# Toasts — collection CRUD
rag-toast-malformed-form = malformed form: { $err }
rag-toast-name-exists = a collection named `{ $name }` already exists
rag-toast-create-failed = could not create collection
rag-toast-indexing-queued = Indexing `{ $name }` @ `{ $ref }` was queued.
rag-toast-created-aggregate = Created `{ $name }` (aggregate). Add source repos below to index them.
rag-toast-collection-not-found = collection not found
rag-toast-collection-not-found-cap = Collection not found.
rag-toast-load-collection-failed = could not load collection
rag-toast-load-collection-failed-cap = Could not load collection.
rag-toast-name-length = Name must be 1..=64 characters.
rag-toast-git-url-required = Git URL is required.
rag-toast-embedding-model-required = Embedding model is required.
rag-toast-chunk-size-range = Chunk size must be in (0, 8000].
rag-toast-chunk-overlap-range = Chunk overlap must be in [0, chunk_size).
rag-toast-save-failed = Saving collection failed.
rag-toast-vanished = Collection vanished after save.
rag-toast-saved-reload-failed = Saved but reload failed.
rag-toast-saved = Saved `{ $name }`.
rag-toast-collection-removed = Collection removed.
rag-toast-collection-already-gone = Collection already gone.
rag-toast-delete-failed = Delete failed.

# Toasts — refs / sources
rag-toast-reindex-queue-failed = could not queue re-index
rag-toast-reindex-queued-count = Queued re-index of { $count } ref(s).
rag-toast-ref-required = Ref (branch/tag/commit) is required.
rag-toast-ref-exists = ref `{ $ref }` already exists on this collection
rag-toast-add-ref-failed = could not add ref
rag-toast-indexing-queued-ref = Queued indexing of `{ $ref }`.
rag-toast-no-source-urls = No source URLs found.
rag-toast-bulk-queued-skipped = Queued { $added } source(s); skipped { $skipped } duplicate(s).
rag-toast-bulk-queued = Queued indexing of { $added } source(s).
rag-toast-ref-not-found = ref not found
rag-toast-reindex-queued-ref = Queued re-index of `{ $ref }`.
rag-toast-set-primary-failed = could not set primary
rag-toast-now-default = `{ $ref }` is now the default ref.
rag-toast-delete-ref-failed = could not delete ref
rag-toast-ref-removed = Removed ref `{ $ref }`.
rag-toast-load-log-failed = could not load log
rag-toast-git-url-required-aggregate = Git URL is required for an aggregate source.
rag-toast-update-source-failed = could not update source
rag-toast-source-updated = Source updated.

# Status badges
rag-status-pending = pending
rag-status-cloning = cloning
rag-status-indexing = indexing
rag-status-ready = ready
rag-status-error = error

# Collection row
rag-pat-set = PAT set
rag-pat-none = no PAT
rag-meta-aggregate = { $count } source(s) · { $hint }
rag-meta-versioned = { $url } · { $hint }
rag-badge-aggregate = aggregate
rag-embed-prefix = embed:
rag-button-edit = Edit
rag-button-delete-collection = Delete collection
rag-placeholder-source-git-url = https://github.com/org/repo.git
rag-placeholder-ref-default = ref (default: collection's)
rag-button-add-source = Add source
rag-placeholder-branch-tag-commit = branch, tag, or commit
rag-button-add-ref = Add ref
rag-placeholder-bulk-sources = Bulk add — one repo per line, optional @ref:
    https://github.com/proxmox/pve-manager.git
    https://github.com/proxmox/qemu-server.git @master
rag-button-add-bulk = Add sources (bulk)

# Ref / source row
rag-badge-primary = primary
rag-ref-indexed-line = indexed { $date } · { $commit }
rag-never = never
rag-button-log = Log
rag-button-reindex = Re-index
rag-button-set-primary = Set primary
rag-button-remove = Remove

# Indexing log
rag-log-info = info
rag-log-warn = warn
rag-log-error = error
rag-log-heading = Indexing log
rag-log-empty = No indexing events recorded yet. The first run logs here once the indexer picks this ref up.

# Inline per-source editor
rag-label-git-url-source = Git URL (this source)
rag-label-git-url-inherit = Git URL (blank = inherit collection)
rag-placeholder-git-url = https://example.com/org/repo.git
rag-label-branch-tag = Branch / tag
rag-button-save-source = Save source
rag-button-cancel = Cancel

# Create-collection form
rag-create-heading = Index a new collection
rag-create-description = The indexer clones the repo, chunks each file, and embeds it through the configured embedding model. PATs are stored verbatim (the gateway runs on trusted infra).
rag-label-name = Name
rag-placeholder-name = e.g. gateway-repo
rag-label-description-optional = Description (optional)
rag-placeholder-description = short, human-readable
rag-label-git-url-versioned = Git URL (versioned only)
rag-label-pat-optional = Personal access token (optional)
rag-placeholder-pat = for private repos
rag-label-include-globs-full = Include globs (comma- or newline-separated)
rag-placeholder-include-globs = *.rs, *.md
rag-label-exclude-globs = Exclude globs
rag-placeholder-exclude-globs = target/, node_modules/
rag-label-chunk-size = Chunk size
rag-label-chunk-overlap = Chunk overlap
rag-create-aggregate-help = Aggregate (multi-source): search across many repos as one corpus. Leave the Git URL empty and add each source repo after creating. Branch / tag becomes the default ref for added sources.
rag-button-queue-indexing = Queue indexing

# Edit-collection form
rag-edit-heading = Editing { $name }
rag-label-description = Description
rag-label-pat = Personal access token
rag-badge-pat-set = currently set
rag-badge-pat-none = none stored
rag-placeholder-pat-keep = leave blank to keep existing
rag-label-clear-pat = Remove the stored PAT (no longer authenticate)
rag-label-include-globs = Include globs
rag-button-save-changes = Save changes

# Embedding model field
rag-label-embedding-model = Embedding model
rag-placeholder-embedding-model-none = no embedding pools configured — type a model id
rag-option-choose-embedding-model = Choose an embedding model…
rag-suffix-not-advertised = (no longer advertised)
