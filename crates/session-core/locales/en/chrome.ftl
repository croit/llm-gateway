# Strings owned by `session-core/src/chrome.rs` — theme + language
# toggles, generic toast/flash chrome shared by every page.

chrome-theme-toggle-title = Toggle theme
chrome-theme-toggle-aria-to-light = Switch to light theme
chrome-theme-toggle-aria-to-dark = Switch to dark theme
chrome-lang-switcher-aria = Choose language

# Web Push turn-complete notifications (server-sent body; `spawn_assistant_worker`).
push-untitled-conversation = New conversation
push-turn-complete-body = Your answer is ready.
push-turn-error-body = The turn ended with an error.

# Web Push: a connector's authorization died (proactive-refresh sweep,
# `tools::mcp::worker`). { $connector } is the connector's display name.
push-connector-reconnect-title = Connection needs your sign-in
push-connector-reconnect-body = { $connector } was disconnected — open Integrations to reconnect.
