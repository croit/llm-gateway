# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/chat/render.rs` — the
# gateway-only chat-page chrome: the header model/voice pickers, the
# compliance banners, the composer's "+" tools/integrations/skills menu,
# the "Denken" (effort/thinking) picker, and the share/export/fork
# controls. Prefixed `chat-render-` (rather than `chat-`) to avoid
# colliding with `chat/mod.rs`'s own `chat-*` keys in the sibling
# `chat.ftl`.

chat-render-canvas-toggle-title = Afficher / masquer le canevas de document
chat-render-canvas-toggle-label = Canevas
chat-render-canvas-document-tab = Document
chat-render-canvas-assets-tab = Fichiers
chat-render-canvas-assets-heading = Fichiers de la conversation
chat-render-canvas-assets-count = { $count ->
    [one] { $count } fichier
   *[other] { $count } fichiers
}
chat-render-canvas-assets-empty = Aucun fichier n’a encore été ajouté à cette conversation.
chat-render-canvas-asset-download = Télécharger le fichier
chat-render-canvas-close-title = Fermer le canevas

chat-render-model-placeholder = modèle (ex. gpt-4o-mini)
chat-render-model-aria = Modèle de chat
chat-render-voice-model-aria = Modèle vocal

chat-render-model-non-gdpr = { $id } (non conforme RGPD)
chat-render-model-confidential = { $id } (confidentialité restreinte)
chat-render-model-non-gdpr-confidential = { $id } (non conforme RGPD, confidentialité restreinte)

chat-render-gdpr-banner = Vous envoyez des données à un modèle non conforme au RGPD. Ne saisissez aucune information personnelle (noms, e-mails, adresses, données clients ou employés).
chat-render-nda-banner = Ce modèle n'est pas couvert par un accord de confidentialité. N'envoyez pas de contenu protégé par un NDA ou de nature confidentielle.

chat-render-shared-readonly-banner = Chat partagé — lecture seule. Seul le créateur peut répondre.
chat-render-composer-placeholder = Écrire au modèle…

chat-render-new-conversation-fallback = Nouvelle conversation

chat-render-feedback-title = Envoyer un commentaire

chat-render-effort-title = Effort de réflexion
chat-render-effort-tooltip = Effort de réflexion : plus élevé = plus de raisonnement et de cycles d'outils, mais plus lent
chat-render-effort-label-prefix = Réflexion :
chat-render-effort-fast = Rapide
chat-render-effort-standard = Standard
chat-render-effort-deep = Approfondi
chat-render-effort-max = Maximal

chat-render-tools-tooltip = Outils, intégrations et skills pour cette conversation
chat-render-tools-label = Outils
chat-render-tools-search-placeholder = Rechercher des outils…
chat-render-all-tools-label = Tous les outils
chat-render-no-tools-prefix = Aucun outil n'est encore disponible pour votre compte. Connectez une intégration sous
chat-render-no-tools-suffix = .

chat-render-close = Fermer

chat-render-group-web-network = Web & Réseau
chat-render-group-attachments-documents = Pièces jointes & Documents
chat-render-group-document-templates = Modèles de documents
chat-render-group-knowledge-base = Base de connaissances
chat-render-group-code-sandbox = Code & Bac à sable
chat-render-group-memory = Mémoire
chat-render-group-integrations = Intégrations
chat-render-group-utility = Utilitaires
chat-render-group-skills = Skills

chat-render-tool-count = { $count ->
    [one] { $count } outil
   *[other] { $count } outils
}

chat-render-active-count-title = Outils actifs — appuyer pour gérer
chat-render-unpin-title = Détacher (retour à automatique)

chat-render-state-off-tip = Désactivé — bloqué ; masqué à l'assistant
chat-render-state-auto-tip = Automatique — l'assistant l'active lui-même si une demande le nécessite
chat-render-state-on-tip = Activé — toujours disponible pour l'assistant

chat-render-share-label-on = Partagé ✓
chat-render-share-label-off = Partager
chat-render-share-tooltip = Les chats partagés sont lisibles par tout utilisateur connecté disposant du lien

chat-render-fork-tooltip = Copier cette conversation dans vos propres chats pour continuer à discuter
chat-render-fork-label = Continuer dans mes chats

chat-render-export-tooltip = Télécharger cette conversation
chat-render-export-aria = Exporter la conversation
chat-render-export-label = Exporter
chat-render-export-pdf = Document PDF
chat-render-export-md = Markdown (.md)
