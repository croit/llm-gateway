# Strings owned by `gateway/src/rama_server/pages/chat/mod.rs` — the
# multi-conversation chat page's server-side handlers: page title
# fallback, sidebar/effort/share/pin toasts, and the SSE-toast error
# messages the composer's fetch layer surfaces on failed actions.

chat-default-title = Chat

chat-toast-conversation-already-gone = Conversation was already gone.
chat-toast-share-copied = Link copied — any signed-in user with the link can read along.
chat-toast-share-stopped = Sharing stopped — the link no longer works.
chat-toast-pinned = Pinned — this conversation now stays at the top.
chat-toast-unpinned = Unpinned.
chat-toast-already-in-your-chats = This conversation is already in your chats.
chat-toast-effort-set = Thinking effort: { $level }

chat-mcp-bridged-description = Tools bridged from the "{ $name }" integration.

chat-error-conversation-not-found = Conversation not found.
chat-error-message-not-found = Message not found.
chat-error-message-empty = message can't be empty
chat-error-message-must-not-be-empty = Message must not be empty.
chat-error-still-streaming = A response is still streaming for this user — wait for it or hit stop.
chat-error-retry-assistant-only = Retry applies to assistant replies.
chat-error-edit-own-messages-only = Edit applies to your own messages.
chat-error-pdf-export-unavailable = PDF export unavailable: the typst CLI is not installed on the gateway
chat-error-pdf-export-failed = PDF export failed

chat-error-auth-required = auth required
chat-error-no-such-turn = no such turn
chat-error-db-error = db error
chat-error-attachments-not-configured = chat attachments not configured
chat-error-bad-filename = bad filename
chat-error-attachment-not-found = not found
