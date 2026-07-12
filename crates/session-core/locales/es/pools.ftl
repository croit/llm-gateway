# STATUS: llm-generated, unreviewed — pending native-speaker QA

pools-page-title = Pools de origen — LLM Gateway
pools-heading = Pools de origen
pools-description = Agrupe backends en pools por tipo y estrategia de selección. Los cambios se guardan en la base de datos, pero solo surten efecto una vez que haga clic en «Aplicar cambios».

pools-fallbacks-heading = Alternativas para modelos desconocidos
pools-fallbacks-description = Cuando una solicitud nombra un modelo que el gateway nunca ha conocido, sustitúyalo por este modelo para ese tipo. En blanco = el fallo devuelve 404.

pools-add-heading = Añadir pool
pools-field-name = Nombre
pools-field-kind = Tipo
pools-field-strategy = Estrategia
pools-field-fallback-offline = Modelo alternativo fuera de línea
pools-field-fallback-offline-placeholder = servido cuando todos los backends están caídos
pools-field-models = Modelos (separados por comas)
pools-field-voices = Voces (lang=voice por línea)
pools-field-backends = Backends
pools-no-backends = Aún no hay backends definidos. Añada uno primero en la página de Backends.
pools-field-gdpr = Conforme al GDPR
pools-field-nda = Cubierto por NDA
pools-field-enforce-limits = Aplicar límites de tasa y cuotas
pools-save-pool = Guardar pool
pools-add-pool = Añadir pool
pools-delete-pool = Eliminar

pools-error-name-required = el nombre del pool es obligatorio
pools-error-invalid-kind = tipo de pool no válido `{ $kind }`
pools-saved = pool `{ $name }` guardado — haga clic en «Aplicar cambios» para recargar
pools-deleted = pool `{ $name }` eliminado — haga clic en «Aplicar cambios» para recargar
pools-fallback-saved = alternativa de { $kind } establecida en `{ $model }`
pools-fallback-cleared = alternativa de { $kind } eliminada
