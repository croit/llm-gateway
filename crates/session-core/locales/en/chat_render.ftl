# Strings owned by `gateway/src/rama_server/pages/chat/render.rs` — the
# gateway-only chat-page chrome: the header model/voice pickers, the
# compliance banners, the composer's "+" tools/integrations/skills menu,
# the "Denken" (effort/thinking) picker, and the share/export/fork
# controls. Prefixed `chat-render-` (rather than `chat-`) to avoid
# colliding with `chat/mod.rs`'s own `chat-*` keys in the sibling
# `chat.ftl`.

chat-render-canvas-toggle-title = Show / hide the document canvas
chat-render-canvas-toggle-label = Canvas
chat-render-canvas-document-tab = Document
chat-render-canvas-assets-tab = Assets
chat-render-canvas-assets-heading = Conversation assets
chat-render-canvas-assets-count = { $count ->
    [one] { $count } file
   *[other] { $count } files
}
chat-render-canvas-assets-empty = No files have been added to this conversation yet.
chat-render-canvas-asset-download = Download file
chat-render-canvas-close-title = Close canvas

chat-render-model-placeholder = model (e.g. gpt-4o-mini)
chat-render-model-aria = Chat model
chat-render-voice-model-aria = Voice model
chat-render-tts-voice-aria = Spoken-reply voice
chat-render-tts-voice-default = Default voice

chat-render-model-non-gdpr = { $id } (non-GDPR)
chat-render-model-confidential = { $id } (confidential-restricted)
chat-render-model-non-gdpr-confidential = { $id } (non-GDPR, confidential-restricted)

chat-render-gdpr-banner = You are sending data to a non-GDPR-compliant model. Do not enter personal information (names, emails, addresses, customer or employee data).
chat-render-nda-banner = This model is not covered by a confidentiality agreement. Do not send NDA-protected or proprietary material.

chat-render-shared-readonly-banner = Shared chat — read-only. Only the creator can reply.
chat-render-composer-placeholder = Message the model…

chat-render-new-conversation-fallback = New conversation

chat-render-feedback-title = Send feedback

chat-render-effort-title = Thinking effort
chat-render-effort-tooltip = Thinking effort: higher = more reasoning and more tool rounds, but slower
chat-render-effort-label-prefix = Thinking:
chat-render-effort-fast = Fast
chat-render-effort-standard = Standard
chat-render-effort-deep = Deep
chat-render-effort-max = Max

chat-render-tools-tooltip = Tools, integrations & skills for this conversation
chat-render-tools-label = Tools
chat-render-tools-search-placeholder = Search tools…
chat-render-all-tools-label = All tools
chat-render-no-tools-prefix = No tools are available to your account yet. Connect an integration under
chat-render-no-tools-suffix = .

chat-render-close = Close

chat-render-group-web-network = Web & Network
chat-render-group-attachments-documents = Attachments & Documents
chat-render-group-document-templates = Document templates
chat-render-group-knowledge-base = Knowledge base
chat-render-group-code-sandbox = Code & Sandbox
chat-render-group-memory = Memory
chat-render-group-integrations = Integrations
chat-render-group-utility = Utility
chat-render-group-skills = Skills

chat-render-tool-count = { $count ->
    [one] { $count } tool
   *[other] { $count } tools
}

chat-render-active-count-title = Active tools — tap to manage
chat-render-unpin-title = Unpin (back to automatic)

chat-render-state-off-tip = Off — blocked; hidden from the assistant
chat-render-state-auto-tip = Auto — the assistant turns it on when a request needs it
chat-render-state-on-tip = On — always available to the assistant

chat-render-share-label-on = Shared ✓
chat-render-share-label-off = Share
chat-render-share-tooltip = Shared chats are readable by any signed-in user who has the link

chat-render-fork-tooltip = Copy this conversation into your own chats so you can keep chatting
chat-render-fork-label = Continue in my chats

chat-render-export-tooltip = Download this conversation
chat-render-export-aria = Export conversation
chat-render-export-label = Export
chat-render-export-pdf = PDF document
chat-render-export-md = Markdown (.md)
