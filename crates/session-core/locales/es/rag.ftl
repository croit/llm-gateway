# STATUS: llm-generated, unreviewed — pending native-speaker QA

rag-page-title = Colecciones RAG — LLM Gateway
rag-heading = Colecciones RAG
rag-description-prefix = Bases de código que la pasarela ha indexado. La herramienta
rag-description-suffix = consulta estas colecciones para responder preguntas sobre el código.
rag-collections-heading = Colecciones configuradas
rag-empty-list = Aún no hay colecciones. Cree una arriba.

# Toasts — collection CRUD
rag-toast-malformed-form = formulario incorrecto: { $err }
rag-toast-name-exists = ya existe una colección llamada `{ $name }`
rag-toast-create-failed = no se pudo crear la colección
rag-toast-indexing-queued = Se puso en cola la indexación de `{ $name }` @ `{ $ref }`.
rag-toast-created-aggregate = `{ $name }` creada (agregado). Añada los repositorios de origen abajo para indexarlos.
rag-toast-collection-not-found = colección no encontrada
rag-toast-collection-not-found-cap = Colección no encontrada.
rag-toast-load-collection-failed = no se pudo cargar la colección
rag-toast-load-collection-failed-cap = No se pudo cargar la colección.
rag-toast-name-length = El nombre debe tener entre 1 y 64 caracteres.
rag-toast-git-url-required = La URL de Git es obligatoria.
rag-toast-embedding-model-required = El modelo de embedding es obligatorio.
rag-toast-chunk-size-range = El tamaño del fragmento debe estar en (0, 8000].
rag-toast-chunk-overlap-range = El solapamiento del fragmento debe estar en [0, chunk_size).
rag-toast-save-failed = Error al guardar la colección.
rag-toast-vanished = La colección desapareció tras guardarse.
rag-toast-saved-reload-failed = Guardado, pero la recarga falló.
rag-toast-saved = `{ $name }` guardada.
rag-toast-collection-removed = Colección eliminada.
rag-toast-collection-already-gone = La colección ya no existe.
rag-toast-delete-failed = Error al eliminar.

# Toasts — refs / sources
rag-toast-reindex-queue-failed = no se pudo programar la reindexación
rag-toast-reindex-queued-count = Reindexación de { $count } referencia(s) puesta en cola.
rag-toast-ref-required = Se requiere la referencia (rama/etiqueta/commit).
rag-toast-ref-exists = la referencia `{ $ref }` ya existe en esta colección
rag-toast-add-ref-failed = no se pudo añadir la referencia
rag-toast-indexing-queued-ref = Se puso en cola la indexación de `{ $ref }`.
rag-toast-no-source-urls = No se encontraron URL de origen.
rag-toast-bulk-queued-skipped = { $added } fuente(s) en cola; { $skipped } duplicado(s) omitido(s).
rag-toast-bulk-queued = Se puso en cola la indexación de { $added } fuente(s).
rag-toast-ref-not-found = referencia no encontrada
rag-toast-reindex-queued-ref = Reindexación de `{ $ref }` puesta en cola.
rag-toast-set-primary-failed = no se pudo establecer como principal
rag-toast-now-default = `{ $ref }` es ahora la referencia predeterminada.
rag-toast-delete-ref-failed = no se pudo eliminar la referencia
rag-toast-ref-removed = Referencia `{ $ref }` eliminada.
rag-toast-load-log-failed = no se pudo cargar el registro
rag-toast-git-url-required-aggregate = La URL de Git es obligatoria para una fuente de agregado.
rag-toast-update-source-failed = no se pudo actualizar la fuente
rag-toast-source-updated = Fuente actualizada.

# Status badges
rag-status-pending = pendiente
rag-status-cloning = clonando
rag-status-indexing = indexando
rag-status-ready = listo
rag-status-error = error

# Collection row
rag-pat-set = PAT establecido
rag-pat-none = sin PAT
rag-meta-aggregate = { $count } fuente(s) · { $hint }
rag-meta-versioned = { $url } · { $hint }
rag-badge-aggregate = agregado
rag-embed-prefix = embed:
rag-button-edit = Editar
rag-button-delete-collection = Eliminar colección
rag-placeholder-source-git-url = https://github.com/org/repo.git
rag-placeholder-ref-default = referencia (predeterminada: la de la colección)
rag-button-add-source = Añadir fuente
rag-placeholder-branch-tag-commit = rama, etiqueta o commit
rag-button-add-ref = Añadir referencia
rag-placeholder-bulk-sources = Añadir en bloque — un repositorio por línea, @ref opcional:
    https://github.com/proxmox/pve-manager.git
    https://github.com/proxmox/qemu-server.git @master
rag-button-add-bulk = Añadir fuentes (en bloque)

# Ref / source row
rag-badge-primary = principal
rag-ref-indexed-line = indexado { $date } · { $commit }
rag-never = nunca
rag-button-log = Registro
rag-button-reindex = Reindexar
rag-button-set-primary = Establecer como principal
rag-button-remove = Quitar

# Indexing log
rag-log-info = info
rag-log-warn = aviso
rag-log-error = error
rag-log-heading = Registro de indexación
rag-log-empty = Aún no se han registrado eventos de indexación. La primera ejecución se registrará aquí en cuanto el indexador procese esta referencia.

# Inline per-source editor
rag-label-git-url-source = URL de Git (esta fuente)
rag-label-git-url-inherit = URL de Git (vacío = heredar de la colección)
rag-placeholder-git-url = https://example.com/org/repo.git
rag-label-branch-tag = Rama / etiqueta
rag-button-save-source = Guardar fuente
rag-button-cancel = Cancelar

# Create-collection form
rag-create-heading = Indexar una nueva colección
rag-create-description = El indexador clona el repositorio, divide cada archivo en fragmentos y los convierte en embeddings con el modelo configurado. Los PAT se almacenan tal cual (la pasarela se ejecuta en infraestructura de confianza).
rag-label-name = Nombre
rag-placeholder-name = p. ej. gateway-repo
rag-label-description-optional = Descripción (opcional)
rag-placeholder-description = breve y legible
rag-label-git-url-versioned = URL de Git (solo versionado)
rag-label-pat-optional = Token de acceso personal (opcional)
rag-placeholder-pat = para repositorios privados
rag-label-include-globs-full = Patrones de inclusión (separados por comas o saltos de línea)
rag-placeholder-include-globs = *.rs, *.md
rag-label-exclude-globs = Patrones de exclusión
rag-placeholder-exclude-globs = target/, node_modules/
rag-label-chunk-size = Tamaño del fragmento
rag-label-chunk-overlap = Solapamiento del fragmento
rag-create-aggregate-help = Agregado (multi-fuente): busca en muchos repositorios como un único corpus. Deje la URL de Git vacía y añada cada repositorio de origen después de crear la colección. La rama / etiqueta se convierte en la referencia predeterminada de las fuentes añadidas.
rag-button-queue-indexing = Programar indexación

# Edit-collection form
rag-edit-heading = Editando { $name }
rag-label-description = Descripción
rag-label-pat = Token de acceso personal
rag-badge-pat-set = actualmente establecido
rag-badge-pat-none = ninguno guardado
rag-placeholder-pat-keep = deje en blanco para conservar el existente
rag-label-clear-pat = Eliminar el PAT guardado (dejar de autenticarse)
rag-label-include-globs = Patrones de inclusión
rag-button-save-changes = Guardar cambios

# Embedding model field
rag-label-embedding-model = Modelo de embedding
rag-placeholder-embedding-model-none = no hay pools de embedding configurados — escriba un id de modelo
rag-option-choose-embedding-model = Elija un modelo de embedding…
rag-suffix-not-advertised = (ya no disponible)

rag-label-allowed-groups = Grupos permitidos
rag-hint-allowed-groups = Grupos del gateway (separados por comas) autorizados a listar y buscar en esta colecciÃ³n. VacÃ­o = todos los que tengan las herramientas RAG. Los admins siempre tienen acceso.
