# STATUS: llm-generated, unreviewed — pending native-speaker QA
# Paramètres opérateur (/admin/settings). Les titres de cartes (settings-s-*),
# les libellés de champs (settings-f-*) et leur aide (settings-f-*-help) sont
# dérivés des entrées de gateway_core::server::settings::SECTIONS :
# `sandbox.runner_url` -> `settings-f-sandbox-runner_url`.
# Voir locales/en/settings.ftl pour la source.

settings-heading = Paramètres
settings-intro = Paramètres d'exploitation de cette passerelle. Ils sont stockés en base, aucun fichier de configuration n'est nécessaire — chaque champ affiche aussi la clé TOML qu'il remplace.
settings-save = Enregistrer la section
settings-saved = Enregistré. Actif dès la prochaine requête.
settings-saved-restart = Enregistré. Certains champs de cette section ne prennent effet qu'après un redémarrage.
settings-save-failed = Impossible d'enregistrer ces paramètres.
settings-cleared = Réinitialisé. La valeur par défaut s'applique de nouveau.
settings-restart-badge = redémarrage
settings-restart-note = Les champs marqués « redémarrage » ne sont lus qu'au démarrage ; les modifier exige un redémarrage.
settings-secret-set = enregistré — saisissez une nouvelle valeur pour le remplacer
settings-secret-unset = non défini
settings-secret-clear = Effacer

settings-no-backend-heading = Aucun backend de modèle
settings-no-backend-body = La connexion est configurée, mais cette passerelle ne sert aucun modèle avant l'ajout d'un backend. D'ici là, le chat et l'API /v1 refusent les requêtes.
settings-no-backend-cta = Ajouter un backend dans /admin/upstreams →

settings-tab-chat = Chat
settings-tab-tools = Outils
settings-tab-data = Contenu & données
settings-tab-access = Accès & utilisation
settings-tab-notifications = Notifications
settings-show-fields = Afficher { $count } réglages supplémentaires
settings-model-automatic = Automatique — utiliser le premier modèle disponible
settings-model-none-configured = Aucun modèle de ce type n'est encore configuré. Ajoutez un pool correspondant dans /admin/upstreams et il apparaîtra ici.
settings-model-unavailable = { $model } (configuré, mais indisponible actuellement)
settings-restart-pending-heading = Redémarrage en attente
settings-restart-pending-body = Ces réglages sont enregistrés mais ne prendront effet qu'après un redémarrage de la passerelle :

# ─── Cartes de section ───────────────────────────────────────────────────────

settings-s-chat-ocr = OCR des documents
settings-s-chat-ocr-blurb = Transformer les PDF et images envoyés en texte que le modèle peut lire.
settings-s-chat-compaction = Compactage des conversations
settings-s-chat-compaction-blurb = Résumer la moitié la plus ancienne d'une longue conversation pour qu'elle continue de tenir dans la fenêtre de contexte du modèle.
settings-s-chat-s3 = Stockage des pièces jointes (S3)
settings-s-chat-s3-blurb = Stockage objet pour les pièces jointes du chat. Sans lui, les envois sont refusés.
settings-s-sandbox = Bac à sable de code
settings-s-sandbox-blurb = L'exécuteur isolé qui lance le code écrit par le modèle.
settings-s-comfyui = ComfyUI image & vidéo
settings-s-comfyui-blurb = Le worker ComfyUI headless derrière les outils d'image et de vidéo.
settings-s-rag = Indexation RAG
settings-s-rag-blurb = Où sont stockées les sources indexées, et à quel rythme travaille l'indexeur.
settings-s-skills = Compétences
settings-s-skills-blurb = Le répertoire de bundles sur disque derrière /admin/skills.
settings-s-typst = Modèles Typst
settings-s-typst-blurb = Les modèles derrière l'export PDF et les outils de document.
settings-s-geoip = GeoIP
settings-s-geoip-blurb = Localisation approximative du client, pour l'outil get_user_location.
settings-s-usage = Métriques d'utilisation
settings-s-usage-blurb = Comptabilisation par requête derrière /usage.
settings-s-limits = Limites de débit & quotas
settings-s-limits-blurb = Interrupteur principal des règles configurées dans /admin/limits.
settings-s-feedback = Widget de retour
settings-s-feedback-blurb = Où le widget de retour intégré dépose ses tickets.
settings-s-push = Web Push
settings-s-push-blurb = Notifications de fin de réponse. La paire de clés est générée et enregistrée automatiquement.
settings-s-gateway = Sessions & jetons
settings-s-gateway-blurb = Combien de temps une session navigateur et un jeton d'API restent valides, et si les administrateurs peuvent usurper une identité.

# ─── Champs ──────────────────────────────────────────────────────────────────

settings-f-chat-ocr-enabled = Activer l'OCR
settings-f-chat-ocr-enabled-help = Interrupteur principal pour l'extraction de texte des documents envoyés.
settings-f-chat-ocr-model = Modèle d'OCR
settings-f-chat-ocr-model-help = Quel modèle lit les pages. Il doit être servi par un pool de type ocr ; en automatique, le premier disponible est utilisé.
settings-f-chat-ocr-max_tokens = Budget de jetons par requête
settings-f-chat-ocr-max_tokens-help = Budget de jetons pour une requête d'OCR.
settings-f-chat-ocr-ngram_window = Fenêtre de recouvrement
settings-f-chat-ocr-ngram_window-help = Recouvrement utilisé pour raccorder les textes des pages sans répéter de contenu.
settings-f-chat-ocr-max_bytes = Taille maximale du document
settings-f-chat-ocr-max_bytes-help = Plus grand document accepté, en octets.
settings-f-chat-ocr-max_pages = Nombre maximal de pages
settings-f-chat-ocr-max_pages-help = Nombre maximal de pages lues dans un même document.
settings-f-chat-ocr-dpi = Résolution de rastérisation
settings-f-chat-ocr-dpi-help = Résolution à laquelle les pages PDF sont rendues avant lecture, en DPI.
settings-f-chat-ocr-max_output_chars = Texte extrait maximal
settings-f-chat-ocr-max_output_chars-help = Plafond du texte extrait d'un document, en caractères.
settings-f-chat-ocr-timeout_secs = Délai d'attente
settings-f-chat-ocr-timeout_secs-help = Délai pour un document, en secondes.
settings-f-chat-ocr-max_concurrency = Pages en parallèle
settings-f-chat-ocr-max_concurrency-help = Combien de pages sont lues en même temps.
settings-f-chat-ocr-auto_min_text_chars_per_page = Seuil de détection de numérisation
settings-f-chat-ocr-auto_min_text_chars_per_page-help = En dessous de ce nombre de caractères intégrés par page, un PDF est considéré comme numérisé et envoyé à l'OCR.

settings-f-chat-compaction-enabled = Activer le compactage
settings-f-chat-compaction-enabled-help = Interrupteur principal du résumé des longues conversations.
settings-f-chat-compaction-default_context_window = Fenêtre de contexte supposée
settings-f-chat-compaction-default_context_window-help = Fenêtre de contexte en jetons supposée pour un modèle qui n'en déclare aucune.
settings-f-chat-compaction-trigger_ratio = Seuil de déclenchement
settings-f-chat-compaction-trigger_ratio-help = Fraction de la fenêtre de contexte qui déclenche le compactage (0,7 = à 70 % de remplissage).
settings-f-chat-compaction-keep_recent_turns = Tours récents conservés
settings-f-chat-compaction-keep_recent_turns-help = Tours conservés mot pour mot à la fin de la conversation.
settings-f-chat-compaction-min_turns_to_compact = Longueur minimale de conversation
settings-f-chat-compaction-min_turns_to_compact-help = Ne jamais compacter une conversation plus courte que ce nombre de tours.
settings-f-chat-compaction-summary_max_tokens = Budget de jetons du résumé
settings-f-chat-compaction-summary_max_tokens-help = Budget de jetons pour le résumé qui remplace les tours compactés.

settings-f-chat-s3-enabled = Stocker les pièces jointes dans S3
settings-f-chat-s3-enabled-help = Désactivé, les pièces jointes du chat sont indisponibles.
settings-f-chat-s3-endpoint = URL du point d'accès
settings-f-chat-s3-endpoint-help = Par exemple https://s3.eu-central-1.amazonaws.com, ou une adresse MinIO.
settings-f-chat-s3-region = Région
settings-f-chat-s3-region-help = Nom de la région.
settings-f-chat-s3-bucket = Bucket
settings-f-chat-s3-bucket-help = Bucket qui contient les pièces jointes.
settings-f-chat-s3-key_prefix = Préfixe de clé
settings-f-chat-s3-key_prefix-help = Préfixe sous lequel chaque clé d'objet est écrite.
settings-f-chat-s3-access_key = Identifiant de clé d'accès
settings-f-chat-s3-access_key-help = Identifiant de la clé d'accès utilisée pour joindre le bucket.
settings-f-chat-s3-secret_key = Clé d'accès secrète
settings-f-chat-s3-secret_key-help = Moitié secrète de cette clé d'accès. Stockée chiffrée.

settings-f-sandbox-enabled = Activer les outils du bac à sable
settings-f-sandbox-enabled-help = Enregistre les outils qui permettent au modèle d'exécuter du code.
settings-f-sandbox-runner_url = URL de l'exécuteur
settings-f-sandbox-runner_url-help = URL de base du service sandbox-runner. Il exécute du code arbitraire et ne doit donc être joignable que depuis la passerelle.
settings-f-sandbox-timeout_secs = Délai d'attente
settings-f-sandbox-timeout_secs-help = Délai HTTP pour une exécution, en secondes.
settings-f-sandbox-max_artifact_bytes = Taille maximale d'artéfact
settings-f-sandbox-max_artifact_bytes-help = Plus gros fichier accepté en retour d'une exécution, en octets.

settings-f-comfyui-enabled = Activer les outils image & vidéo
settings-f-comfyui-enabled-help = Enregistre les outils comfyui_*.
settings-f-comfyui-base_url = URL de ComfyUI
settings-f-comfyui-base_url-help = URL de base de l'instance ComfyUI. Elle n'a aucune authentification et ne doit donc être joignable que depuis la passerelle.
settings-f-comfyui-content_dir = Répertoire des workflows
settings-f-comfyui-content_dir-help = Contient un sous-répertoire par workflow. Le bouton de rechargement dans /admin/comfyui le relit sans redémarrage.
settings-f-comfyui-timeout_secs = Délai d'attente
settings-f-comfyui-timeout_secs-help = Délai pour une exécution de workflow, en secondes.
settings-f-comfyui-queue_poll_interval_ms = Intervalle d'interrogation de la file
settings-f-comfyui-queue_poll_interval_ms-help = À quelle fréquence la passerelle interroge ComfyUI sur une tâche en cours, en millisecondes.
settings-f-comfyui-max_concurrent_jobs = Tâches simultanées
settings-f-comfyui-max_concurrent_jobs-help = Nombre de workflows que le modèle peut faire tourner en même temps.

settings-f-rag-enabled = Lancer l'indexeur
settings-f-rag-enabled-help = Interrupteur principal de l'indexation et de la recherche RAG.
settings-f-rag-data_dir = Répertoire des index
settings-f-rag-data_dir-help = Où sont stockés les index. Doit se trouver sur le volume persistant, sinon chaque redémarrage réindexe tout. Les index existants ne suivent pas — pointez ceci ailleurs et tout est réindexé de zéro.
settings-f-rag-clone_concurrency = Tâches d'indexation en parallèle
settings-f-rag-clone_concurrency-help = Combien de clones git et de tâches d'indexation tournent en même temps.

settings-f-skills-enabled = Charger les bundles de compétences
settings-f-skills-enabled-help = Interrupteur principal des compétences gérées dans /admin/skills.
settings-f-skills-dir = Répertoire des bundles
settings-f-skills-dir-help = Répertoire qui contient les bundles de compétences.

settings-f-typst-enabled = Charger les modèles Typst
settings-f-typst-enabled-help = Interrupteur principal de l'export PDF et des outils de document.
settings-f-typst-templates_dir = Répertoire des modèles
settings-f-typst-templates_dir-help = Répertoire qui contient les modèles. Relu à l'enregistrement : ajouter un modèle ne demande aucun redémarrage.

settings-f-geoip-enabled = Activer les recherches GeoIP
settings-f-geoip-enabled-help = Interrupteur principal de l'outil get_user_location.
settings-f-geoip-db_path = Fichier de base de données
settings-f-geoip-db_path-help = Chemin de la base IP2Location au format BIN.
settings-f-geoip-update_token = Jeton de téléchargement
settings-f-geoip-update_token-help = Jeton IP2Location utilisé pour rafraîchir la base. Stocké chiffré.

settings-f-usage-enabled = Enregistrer l'utilisation
settings-f-usage-enabled-help = Comptabilisation par requête derrière /usage.
settings-f-usage-retention_days = Conservation
settings-f-usage-retention_days-help = Combien de jours les enregistrements sont conservés.
settings-f-usage-currency = Devise
settings-f-usage-currency-help = Devise dans laquelle les coûts sont indiqués.

settings-f-limits-enabled = Appliquer les limites et quotas
settings-f-limits-enabled-help = Désactivé, les règles de /admin/limits sont ignorées.

settings-f-feedback-enabled = Proposer le widget de retour
settings-f-feedback-enabled-help = Interrupteur principal du bouton de retour intégré.
settings-f-feedback-github_owner = Propriétaire du dépôt
settings-f-feedback-github_owner-help = Utilisateur ou organisation GitHub qui possède le suivi des tickets.
settings-f-feedback-github_repo = Dépôt
settings-f-feedback-github_repo-help = Nom du dépôt dans lequel les tickets sont créés.
settings-f-feedback-github_token = Jeton GitHub
settings-f-feedback-github_token-help = Nécessite issues:write, plus contents:write si des captures d'écran sont joints. Stocké chiffré.
settings-f-feedback-github_api_base = URL de base de l'API
settings-f-feedback-github_api_base-help = URL de base de l'API REST. À changer pour GitHub Enterprise.
settings-f-feedback-labels = Étiquettes des tickets
settings-f-feedback-labels-help = Étiquettes appliquées à chaque ticket créé.
settings-f-feedback-assets_branch = Branche des captures d'écran
settings-f-feedback-assets_branch-help = Branche orpheline dans laquelle les captures d'écran sont commitées.
settings-f-feedback-extraction_model = Modèle d'extraction
settings-f-feedback-extraction_model-help = Modèle de chat qui transforme une note vocale en champs du formulaire.
settings-f-feedback-voice_model = Modèle de transcription
settings-f-feedback-voice_model-help = Modèle qui transforme la note vocale en texte.

settings-f-push-enabled = Envoyer des notifications push
settings-f-push-enabled-help = Expose les points d'accès push et notifie la fin d'une réponse.
settings-f-push-contact = Contact de l'exploitant
settings-f-push-contact-help = Une URI mailto: ou https: par laquelle le service push peut vous joindre.

settings-f-gateway-token_ttl_days = Durée de vie des jetons d'API
settings-f-gateway-token_ttl_days-help = Combien de jours un jeton gwk_… nouvellement créé reste valide.
settings-f-gateway-session_ttl_days = Délai d'inactivité de session
settings-f-gateway-session_ttl_days-help = Délai d'inactivité glissant pour une session navigateur, en jours : chaque requête le repousse, c'est donc la durée d'absence tolérée avant de devoir se reconnecter.
settings-f-gateway-session_absolute_max_days = Âge maximal de session
settings-f-gateway-session_absolute_max_days-help = Plafond strict en jours depuis la connexion, qu'aucune activité ne prolonge. Il force aussi un passage périodique par le fournisseur d'identité, le seul moment où les revendications de groupe sont relues.
settings-f-gateway-allow_impersonation = Autoriser l'usurpation d'identité
settings-f-gateway-allow_impersonation-help = Permet aux administrateurs d'agir en tant qu'un autre utilisateur pour le débogage. Chaque usurpation est auditée et affiche une bannière permanente ; désactivé, les boutons disparaissent et le point d'accès refuse.
