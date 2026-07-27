# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `session-core/src/render.rs` — the HTML renderers for
# the chat-style session UI (conversation bubbles, tool-call rows, the
# document canvas, and the composer). Driver-agnostic: both the gateway
# and any future consumer of this crate render through these functions.

render-edit-button = ✎ Modifier
render-edit-confirm = Enregistrer et régénérer ? Cela supprime tous les messages ci-dessous.
render-edit-save = Enregistrer et régénérer
render-edit-cancel = Annuler

render-retry-button = ↻ Réessayer
render-retry-confirm = Régénérer cette réponse ? Cela la supprime ainsi que tout ce qui suit.

render-attachment-unavailable-title = Cette pièce jointe n'est plus disponible
render-attachment-unavailable-meta = indisponible
render-attachment-open-title = Ouvrir { $filename } · { $mime } · { $size }
render-attachment-title = { $filename } · { $mime } · { $size }
render-attachment-chip-title = { $mime } · { $size }
render-attachment-remove-aria = Supprimer la pièce jointe
render-attachment-remove-confirm = Supprimer { $filename } ? Action irréversible.

# Légende de chaque média généré dans une réponse multi-médias, pour pouvoir
# y faire référence (« transforme la 2e image en vidéo »).
render-media-label = { $kind ->
    [image] Image { $n }
    [video] Vidéo { $n }
    [audio] Audio { $n }
   *[other] Média { $n }
}

# Bouton de copie sur un bloc de code d'une réponse (icône seule, donc
# ceci est son infobulle / nom accessible).
render-code-copy = Copier le code
render-code-copied = Copié

render-thinking-spinner = Réflexion…
render-thinking-finalized = Réflexion pendant { $secs } s
render-thinking-in-progress = Réflexion… ({ $secs } s)

render-tools-running = Outils en cours
render-tools-errored = Appels d'outils
render-tools-used = Outils utilisés
render-tools-summary = { $count } appels · { $breakdown }

render-tool-status-calling = Appel en cours
render-tool-status-used = Utilisé
render-tool-status-error = Erreur d'outil
render-tool-input-label = Entrée
render-tool-output-label = Sortie
render-tool-output-truncated = tronqué pour l'affichage — les { $bytes } octets complets restent disponibles pour le modèle et persistés dans la base de données ; affichage des { $chars } premiers caractères

render-canvas-hand-edited = modifié par vous
render-canvas-edit-button = ✎ Modifier
render-canvas-save = Enregistrer comme nouvelle version
render-canvas-cancel = Annuler
render-canvas-edit-hint = Enregistré comme nouvelle version ; l'assistant est informé de votre modification.
render-canvas-close-title = Fermer
render-canvas-close-aria = Fermer le panneau du document
render-canvas-document-aria = Document
render-canvas-version-aria = Version

render-composer-attach-aria = Joindre des fichiers
render-composer-attach-title = Joindre des fichiers (aussi par glisser-déposer / coller)
render-composer-record-aria = Enregistrer un message vocal
render-composer-record-title = Enregistrer
render-composer-send = Envoyer
render-composer-stop = Arrêter

render-compaction-divider = Messages précédents condensés pour économiser du contexte
