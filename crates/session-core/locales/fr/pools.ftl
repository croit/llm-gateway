# STATUS: llm-generated, unreviewed — pending native-speaker QA

pools-page-title = Pools en amont — LLM Gateway
pools-heading = Pools en amont
pools-description = Regroupez les backends en pools par type et stratégie de sélection. Les modifications sont enregistrées en base mais ne prennent effet qu'une fois que vous cliquez sur « Appliquer les modifications ».

pools-fallbacks-heading = Replis pour modèles inconnus
pools-fallbacks-description = Lorsqu'une requête nomme un modèle que la gateway n'a jamais rencontré, ce modèle est substitué pour ce type. Vide = l'absence renvoie 404.

pools-add-heading = Ajouter un pool
pools-field-name = Nom
pools-field-kind = Type
pools-field-strategy = Stratégie
pools-field-fallback-offline = Modèle de repli hors ligne
pools-field-fallback-offline-placeholder = servi lorsque tous les backends sont hors service
pools-field-models = Modèles (séparés par des virgules)
pools-field-voices = Voix (lang=voice par ligne)
pools-field-backends = Backends
pools-no-backends = Aucun backend défini pour l'instant. Ajoutez-en un d'abord sur la page Backends.
pools-field-gdpr = Conforme au GDPR
pools-field-nda = Couvert par NDA
pools-field-enforce-limits = Appliquer les limites de débit et les quotas
pools-save-pool = Enregistrer le pool
pools-add-pool = Ajouter un pool
pools-delete-pool = Supprimer

pools-error-name-required = le nom du pool est requis
pools-error-invalid-kind = type de pool invalide `{ $kind }`
pools-saved = pool `{ $name }` enregistré — cliquez sur « Appliquer les modifications » pour recharger
pools-deleted = pool `{ $name }` supprimé — cliquez sur « Appliquer les modifications » pour recharger
pools-fallback-saved = repli { $kind } défini sur `{ $model }`
pools-fallback-cleared = repli { $kind } effacé
