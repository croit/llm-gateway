# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Strings owned by `gateway/src/rama_server/pages/chat/mod.rs` — the
# multi-conversation chat page's server-side handlers: page title
# fallback, sidebar/effort/share/pin toasts, and the SSE-toast error
# messages the composer's fetch layer surfaces on failed actions.

chat-default-title = Chat

chat-toast-conversation-already-gone = La conversation avait déjà disparu.
chat-toast-share-copied = Lien copié — tout utilisateur connecté disposant du lien peut suivre la conversation.
chat-toast-share-stopped = Partage arrêté — le lien ne fonctionne plus.
chat-toast-pinned = Épinglé — cette conversation reste maintenant en haut.
chat-toast-unpinned = Désépinglé.
chat-toast-already-in-your-chats = Cette conversation est déjà dans vos discussions.
chat-toast-effort-set = Effort de réflexion : { $level }

chat-mcp-bridged-description = Outils fournis par l'intégration « { $name } ».

chat-error-conversation-not-found = Conversation introuvable.
chat-error-message-not-found = Message introuvable.
chat-error-message-empty = le message ne peut pas être vide
chat-error-message-must-not-be-empty = Le message ne doit pas être vide.
chat-error-still-streaming = Une réponse est toujours en cours pour cet utilisateur — attendez ou appuyez sur Arrêter.
chat-error-retry-assistant-only = Réessayer ne s'applique qu'aux réponses de l'assistant.
chat-error-edit-own-messages-only = La modification ne s'applique qu'à vos propres messages.
chat-error-pdf-export-unavailable = Export PDF indisponible : le CLI typst n'est pas installé sur la passerelle
chat-error-pdf-export-failed = Échec de l'export PDF

chat-error-document-not-found = Document introuvable.
chat-error-document-too-large = Ce document est trop volumineux pour être enregistré (limite 512 Ko).

chat-error-auth-required = authentification requise
chat-error-no-such-turn = ce message n'existe pas
chat-error-db-error = erreur de base de données
chat-error-attachments-not-configured = les pièces jointes du chat ne sont pas configurées
chat-error-bad-filename = nom de fichier invalide
chat-error-attachment-not-found = introuvable
chat-error-rate-limited = Vous avez atteint une limite d'utilisation. Voir /usage pour les détails et la réinitialisation.
