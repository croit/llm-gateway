# Web UI

The gateway serves its own HTML directly — no SPA, no React, no client framework other than ~34 KB of [datastar](https://data-star.dev/) for live updates. Pages are server-rendered through [plait](https://github.com/devashishdxt/plait)'s `html!` macro inline in rama handlers; styling is [daisyUI v5](https://daisyui.com/) component classes on top of Tailwind v4.

The whole stack:

| Layer | Tech | Lives in |
|---|---|---|
| HTTP server / router | rama 0.3 | `crates/gateway/src/rama_server/router.rs` |
| HTML templates | plait `html!` macro | `crates/gateway/src/rama_server/pages/` (`mod.rs` chrome + `chat.rs` / `tokens.rs` / `dashboard.rs`) |
| Reactivity (chat + tokens) | datastar 1.x (self-hosted, baked in via `include_bytes!`) | `crates/gateway/src/rama_server/assets.rs` |
| Client-side glue | TypeScript (strict) bundled with esbuild | `ui/ts/` builds → `crates/gateway/assets/app.js` |
| Styling | Tailwind v4 + daisyUI v5 | `ui/src/main.css` builds → `crates/gateway/assets/app.css` |

## Pages

| Route | What it does | Auth | Response shape |
|---|---|---|---|
| `GET /login` | Standalone sign-in page — single button kicks off `GET /auth/login`. | anonymous | HTML |
| `GET /` | Dashboard. User identity + OIDC roles + RBAC role IDs + link to `/tokens`. | session | HTML |
| `GET /tokens` | Lists tokens. Inline form to mint a new one. Always emits the `<ul id="token-list">` (empty or not) so SSE patches have a stable target. | session | HTML |
| `POST /tokens` | Mints a token. Returns `text/event-stream` patches: append the row to `#token-list`, swap `#token-minted-banner` with the filled banner, reset the create form, append success toast. | session | **SSE** |
| `POST /tokens/{id}/revoke` | Re-renders the row from the freshly-revoked DB record, swaps it in via `mode outer` on `#token-row-<id>`, appends toast. | session | **SSE** |
| `POST /tokens/{id}/delete` | `mode remove` on `#token-row-<id>` + toast. Refuses active tokens (returns an info toast). | session | **SSE** |
| `GET /chat` | Redirects to the user's most-recent conversation (creating one if they've never chatted). | session | 303 redirect |
| `GET /chat/{id}` | Sidebar of all conversations + the chosen one's history + composer. Includes a `data-init` auto-tail when there's still an in-flight assistant turn for this session. | session | HTML |
| `POST /chat/sessions` | Creates a fresh conversation, nav-patches `<main>` to its URL. | session | **SSE** |
| `POST /chat/{id}/messages` | Submits a user message. Persists user + assistant turn rows to SQLite, spawns the streaming worker, and SSE-tails the worker's broadcast. | session | **SSE** |
| `GET /chat/{id}/tail` | Reconnect endpoint. Subscribes to the user's in-flight worker (if any belongs to this session) and emits the same patches the original POST got. Used after backgrounding / network blips / second tab attach. | session | **SSE** |
| `POST /chat/{id}/cancel` | Flips the worker's cancel flag. Worker observes between upstream chunks and exits to finalize. | session | **SSE** |
| `POST /chat/{id}/delete` | Removes the conversation (cascades turns + tool_calls) and nav-patches to the next session. | session | **SSE** |
| `POST /theme/toggle` | Flips the theme cookie and 303s back. | anonymous | 303 redirect |

Assets (every URL is `?v=<sha256-prefix>` cache-busted):

| Route | Source | Notes |
|---|---|---|
| `GET /assets/app.css` | `ui/src/main.css` → `crates/gateway/assets/app.css` | Tailwind/daisyUI build |
| `GET /assets/datastar.js` | `crates/gateway/assets/datastar.js` | Upstream release, vendored |
| `GET /assets/app.js` | `ui/ts/app.ts` (+ per-feature modules under `ui/ts/`) → `crates/gateway/assets/app.js` | esbuild bundle, minified IIFE |
| `GET /assets/pcm-recorder.js` | `ui/ts/pcm-recorder.ts` → `crates/gateway/assets/pcm-recorder.js` | AudioWorklet processor for the voice button — separate bundle because it runs in its own JS realm |

Everything is `include_bytes!`'d into the binary so the release image doesn't need an asset directory at runtime. Both the CSS and JS bundles are committed to the repo as build outputs — the binary's `cargo build` doesn't depend on node, but a clean `mise run build` rebuilds them.

## Authoring patterns

### `html!` is just Rust expressions

```rust
let body = html! {
    h1(class: "text-2xl font-bold mb-2") { "API tokens" }
    ul(id: "token-list", class: "flex flex-col divide-y divide-base-300") {
        for r in rows.iter() {
            (render_token_row(r))
        }
    }
}.to_html();
```

Things to know:
- Bare strings get HTML-escaped.
- `(expr)` interpolates an expression via `ToHtml`. A `plait::Html` (already rendered) is *not* re-escaped.
- `#(raw_string)` splices in already-trusted HTML without escaping. Use sparingly (markdown output, embedded SVG icons).
- The `html!` macro generates an `Fn` closure under the hood, which means captured `Option<String>` etc. has to be borrowed with `.as_ref()` before destructuring inside the macro.
- `plait` auto-emits `<!DOCTYPE html>` when the root element is `<html>`. Don't write the literal yourself — it goes through HTML-escaping and renders as `&lt;!DOCTYPE html&gt;`.
- Empty elements that aren't void (`span`, `div`, etc.) need explicit `{}` — `span;` is a syntax error; `span {}` is fine. Void elements (`input`, `meta`, `link`) use `;`.

### Module split

Templates live in a directory module so each page sits in its own file:

```
crates/gateway/src/rama_server/pages/
├── mod.rs       shared chrome — layout, nav, theme, SSE framing,
│                 Flash, session gate, error pages, /login, /theme/toggle
├── chat/        multi-conversation chat (own directory because of size)
│   ├── mod.rs   handlers (chat_index / chat_session_view /
│   │             chat_session_create / chat_message_send / chat_tail /
│   │             chat_cancel / chat_session_delete) + the shared
│   │             SSE-streaming task that re-reads DB on each tick
│   ├── worker.rs  run_chat_turn — the per-user streaming loop that
│   │             walks the upstream SSE, appends to chat_turns /
│   │             chat_tool_calls in SQLite, and broadcasts a Tick
│   │             after every DB write
│   └── render.rs  render_chat_page / render_sidebar / render_turn /
│                  render_thinking_block / render_tool_call /
│                  render_composer — pure functions of `chat::Turn` /
│                  `chat::TurnWithTools` / `chat::Session`
├── tokens.rs    /tokens CRUD + render_token_row / render_minted_banner /
│                 empty_banner_placeholder
└── dashboard.rs /  handler + render_dashboard_body
```

Each submodule grabs the chrome it needs via `use super::{...}` — `Flash`, `NavItem`, `Theme`, `sse_patch`, `require_session_or_redirect`, `read_body_to_bytes`, etc. The public handlers are pub-re-exported from `mod.rs` (`pub use chat::{chat_index, chat_session_view, …};`) so the router still calls `pages::chat_index` / `pages::tokens_create` / etc. unchanged.

### Layouts

Four helpers in `pages/mod.rs`, in increasing order of chrome:

| Helper | What | Used by |
|---|---|---|
| `layout(theme, title, body) -> String` | Bare `<html>` chrome with stylesheet + datastar + the `app.js` bundle. | `html_page` |
| `layout_authed(theme, active, title, user_email, body) -> String` | Same plus the top nav bar. `active: Option<NavItem>` marks the selected tab. | `html_authed_page` |
| `html_page(theme, title, body) -> Response` | `layout(...)` → 200 `text/html` response (with `Permissions-Policy` header). | `/login` |
| `html_authed_page(theme, active, title, user_email, body) -> Response` | `layout_authed(...)` → 200 `text/html` response. | every authed GET |

The chat page swaps `<main>` to a `chat-main` flex column (full viewport height − nav) so the composer is structurally pinned to the bottom. `main_class_for(active)` picks the right class.

### daisyUI tokens + Tailwind utilities

daisyUI ships **semantic component classes** (`btn`, `card`, `alert`, `badge`, `input`, `select`, `tabs`, `dropdown`, …) and a **theme token system** (`--color-base-100`, `--color-primary`, etc.) that lets a global theme override change every page without touching the templates.

We use a **shadcn-flavoured palette** registered as the `light` / `dark` themes via `@plugin "daisyui/theme"` in `ui/src/main.css` — the primary action is near-black (light theme) / near-white (dark theme); only the status colours (info / success / warning / error) carry hue.

Component classes:

```
btn / btn-primary / btn-ghost / btn-error / btn-outline / btn-sm / btn-circle / btn-square
card / card-body / card-title / card-actions
alert / alert-success / alert-error / alert-info
badge / badge-outline / badge-success / badge-error
input / select / textarea (+ -bordered)
tabs / tab / tab-active
dropdown / dropdown-end / dropdown-content
toast / toast-bottom / toast-end
form-control / label / label-text
```

Token utilities for bespoke layout:

```
bg-base-100 / bg-base-200 / bg-base-300
text-base-content / text-base-content/60   (alpha = muted)
border-base-300 / divide-base-300
text-primary / text-success / text-error
border-l-success / border-l-error / border-l-info
```

Plain Tailwind utilities cover layout (`flex`, `grid`, `gap-4`, `mb-6`, `p-6`, `max-w-md`, …) and don't have daisyUI equivalents.

**Hard rules:**
- No bespoke `.brand-mark` / `.tagline`-style classes. If a treatment isn't covered by daisyUI + Tailwind, drop it.
- Override daisyUI focus/borders/etc. in `@layer utilities` *unlayered* (i.e. `@layer utilities { … }` without a nested sub-layer name). daisyUI emits its components inside `@layer utilities { @layer daisyui.l1.l2.l3 { … } }`, so anything you put in `@layer components` always loses regardless of specificity. Per CSS Cascade Layers spec, unlayered content in a layer comes after any sub-layers — that's the slot we need.

### Mobile-first

Every styled rule and utility class should target ~360 px first; `sm:` enhances for wider screens. Touch targets meet 44 px minimum. `dvh`/`dvw`, never `vh`/`vw`. Stack cards via parent `gap`, not child `margin-top`.

## datastar-driven updates

Every interactive surface — chat streaming, every token CRUD action — uses [datastar](https://data-star.dev/reference/sse_events) instead of round-tripping a full page reload. The pattern is the same in every handler:

1. The form template attaches `data-on:submit__prevent="@post('/some/url', {contentType: 'form'})"`. Datastar intercepts the submit, serialises the form, POSTs as `application/x-www-form-urlencoded`.
2. The handler returns `text/event-stream` with one or more `datastar-patch-elements` (HTML) or `datastar-patch-signals` (state) events.
3. Datastar applies each patch to the DOM / signal store in place — append / outer / inner / before / after / remove for elements; deep-merge for signals.

There's no flash-cookie roundtrip. Feedback (toasts, banner swaps, row insertions, streaming-flag flips) lives on the **same response** that did the work.

### Datastar attribute idioms

Prefer per-element `data-*` attributes over document-delegated JS listeners. The attributes are scoped to the element, survive every nav patch (datastar re-evaluates them on mount), and read top-to-bottom alongside the HTML.

| Attribute | Use |
|---|---|
| `data-signals="{name: value, …}"` | Declare reactive state on this element's scope. Datastar surfaces signals as `$name` inside any datastar expression and re-runs every binding when they change. |
| `data-class="{'classname': $expr}"` | Toggle a class on this element off a signal/expression. |
| `data-on:<event>="<expression>"` | Inline expression evaluated on the DOM event. The variable `el` is the element, `evt` is the event, `$signalName` reads/writes a signal, and `@post('/url', …)` / `@get(…)` issue SSE-aware fetches. Modifiers like `__prevent`, `__stop`, `__capture`, `__outside`, `__window` tune the listener. |
| `data-init="<expression>"` | Run an expression on mount **and** on every mutation of the attribute. Use it as the "wire up this element" hook for behaviour that can't fit in an inline expression — call a TS helper, pass `el`. |

When an interaction needs more JS than fits in an attribute (AudioWorklet plumbing, FormData uploads, MutationObservers, walking DOM), the TS side exposes a function on `window.<feature>.*` and the attribute calls it. See [TypeScript glue](#typescript-glue) below for the registered surface and how it's structured.

#### Worked example: the chat composer's streaming flag

The form owns its own `$chatStreaming` signal; `data-class` drives the CSS button-swap; `data-on:submit__prevent` is a single expression that short-circuits on empty submits, flips the signal, then hands off to datastar's `@post`. End-of-stream, the server emits a `datastar-patch-signals` event that flips the signal back — no client-side state mutation needed.

```rust
form(
    id: "chat-form",
    "data-signals": "{chatStreaming: false}",
    "data-class": "{'chat-composer--streaming': $chatStreaming}",
    "data-on:submit__prevent":
        "window.chatComposer.onSubmit(evt) && \
         ($chatStreaming = true, @post('/chat/{id}/messages', {contentType: 'form'}))",
    method: "post",
    class: "chat-composer"
) { … }
```

Then in the chat-stream worker, at the end of the loop:

```rust
let _ = tx.send(Ok(sse_signals(r#"{"chatStreaming":false}"#))).await;
```

The form's `data-class` binding reactively un-toggles `chat-composer--streaming` and the send button reappears. No `<script>` payload, no manual `classList.remove`.

### Server-side helpers (`pages/mod.rs`)

All in `crates/gateway/src/rama_server/pages/mod.rs`. Re-use these in new handlers — don't open-code SSE framing.

| Helper | Use |
|---|---|
| `sse_patch(selector, mode, elements_html) -> Bytes` | One `datastar-patch-elements` event. `mode` is one of `outer`/`inner`/`append`/`prepend`/`before`/`after`/`remove`. `elements_html` may be empty for `mode remove`. |
| `sse_signals(signals_json) -> Bytes` | One `datastar-patch-signals` event. Body is a JSON object deep-merged into the global signal store. Use this whenever the server needs to flip reactive client state (e.g. mark a stream done). |
| `sse_response(&[Bytes]) -> Response` | Bundle N pre-built event payloads into a `text/event-stream` 200 response. |
| `sse_toast(&Flash) -> Bytes` | A `mode append` patch targeting `#toasts` with one rendered toast item. |
| `sse_toast_response(kind, msg) -> Response` | (Lives in `pages/tokens.rs`.) Shorthand for the failure / no-op branches: one toast, no body changes. |
| `sse_script(js) -> Bytes` | Wraps `js` in a self-removing `<script>` and appends it to `<body>` via a patch. Reach for this only when datastar's element/signal patches can't express the action — `form.reset()` is the canonical example. **Prefer `sse_signals` for state transitions** (signal flips); prefer element patches for DOM changes. |
| `render_toast(&Flash) -> Html` | Single source of truth for toast markup. Reused by `sse_toast` and (via `window.pushToast` in `ui/ts/app.ts`) for client-raised toasts. |

### DRY rule for patch payloads

Every fragment you patch in via SSE **must** also be reachable from the initial server render of the same page. Extract a helper (returning `plait::Html`) and call it from both sites.

Example — the token list:

```rust
// One row, used by render_tokens_body (initial render) AND by
// tokens_create (mode append patch) AND by tokens_revoke (mode outer
// patch). One source of truth means a row's HTML can't drift between
// the page-load shape and the patched-in shape.
fn render_token_row(r: &TokenRowData) -> Html { … }

// Initial render — render_tokens_body
ul(id: "token-list", …) {
    for r in rows.iter() {
        (render_token_row(r))
    }
}

// SSE patch — tokens_create
let row_html = render_token_row(&row_data).to_string();
sse_patch(Some("#token-list"), Some("append"), &row_html)
```

If you find yourself writing the same `<li class="…">…</li>` markup in two places, stop and extract a helper.

### Stable DOM ids on patch targets

Anything you want to swap or remove via SSE needs an id you can put in the `selector`. Convention is `<resource>-<id>` (`#token-row-abc123`, `#token-minted-banner`, `#conversation`, `#turn-<uuid>`, `#turn-<uuid>-text`, `#tc-<tool-call-id>`).

Chat-side specifically: every assistant turn gets a server-side UUID (the `chat_turns.id` primary key) that shows up in the DOM as `id="turn-<uuid>"` for the bubble, plus matching `…-thinking` / `…-tools` / `…-text` slot ids. Per-turn ids mean two concurrent stream attaches (multiple tabs, a retry after a network blip) can't cross-write each other's DOM.

Long-lived interactive subtrees (`<details>` blocks for the thinking spoiler and tool calls) carry `data-preserve-attr="open"` so datastar's morph leaves the user's collapse state alone when the bubble re-renders on each tick.

### Empty-state without server branching

When the SSE patches can transition a list between "has items" and "empty", drive the empty-state visibility from CSS rather than re-rendering the page. The token list does:

```rust
ul(id: "token-list", class: "token-list …") {
    for r in rows.iter() { (render_token_row(r)) }
}
p(class: "token-list-empty …") { "No tokens yet. Create one above." }
```

```css
.token-list-empty { display: none; }
.token-list:not(:has(li)) + .token-list-empty { display: block; }
```

The `<ul>` is always present (so SSE patches always have a target); the empty paragraph appears automatically the moment `:has(li)` evaluates to false. `:has()` is ~93%+ supported (Chrome 105+, Safari 15.4+, Firefox 121+).

### Toasts via SSE

Every page already mounts an empty `#toasts` container in the layout. Any handler that needs to surface a notification appends to it:

```rust
sse_response(&[
    /* …the actual work… */
    sse_toast(&Flash { kind: FlashKind::Success, message: "Token revoked.".into() }),
])
```

`ui/ts/app.ts`'s MutationObserver on `#toasts` picks up the new `.toast-item` and arms a 5.2 s auto-dismiss timer.

For client-raised toasts (e.g. when a mic capture fails inside `ui/ts/chat/mic.ts`), call `window.pushToast(kind, message)`. It emits the same markup `render_toast` does, so the styling is consistent.

### Worked example: full handler skeleton

```rust
pub async fn things_create(State(state): State<Arc<RamaState>>, req: Request) -> Response {
    let (_, user) = match require_session_or_redirect(&state, &req).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let (_, body) = req.into_parts();
    let body = match read_body_to_bytes(body).await {
        Ok(b) => b,
        Err(msg) => return sse_toast_response(FlashKind::Error, msg),
    };
    let form: CreateThingForm = match serde_urlencoded::from_bytes(&body) {
        Ok(f) => f,
        Err(err) => return sse_toast_response(FlashKind::Error, format!("malformed: {err}")),
    };

    // …do the work, get a `thing` back…
    let thing = match things::create(&state.db, &user.id, &form).await {
        Ok(t) => t,
        Err(_) => return sse_toast_response(FlashKind::Error, "Create failed."),
    };

    let row_html = render_thing_row(&thing).to_string();
    sse_response(&[
        sse_patch(Some("#thing-list"), Some("append"), &row_html),
        sse_script("document.getElementById('thing-create-form').reset()"),
        sse_toast(&Flash {
            kind: FlashKind::Success,
            message: "Created.".into(),
        }),
    ])
}
```

### Persisted chat — multi-session, resume-on-reconnect

The chat page is multi-conversation and DB-backed. Every turn (user, assistant, tool call, reasoning chunk) writes to SQLite as it happens; closing a tab mid-stream doesn't lose the response, because the worker keeps running independent of any HTTP listener and writes its progress to the DB. Any client that reopens the page reads the persisted state and (if the worker's still going) attaches to its broadcast for the rest.

**Schema** (`migrations/0005_chat_persistence.sql`):

| Table | Purpose |
|---|---|
| `chat_sessions` | One row per conversation thread, scoped to a user. Sidebar lists these ordered by `updated_at DESC`. |
| `chat_turns` | One row per message. `role='user'` carries the prompt; `role='assistant'` carries the streamed reply with `status` cycling through `in_progress → completed | cancelled | errored`. Accumulated `content` / `reasoning` strings, `reasoning_elapsed_ms`, optional `model`. |
| `chat_tool_calls` | Side table; one assistant turn can fan out into many calls across model rounds. Status flips `running → completed | errored`. |

All CRUD lives in `crates/gateway/src/server/db/chat.rs` with 14 unit tests against an in-memory pool.

**Worker** (`pages/chat/worker.rs::run_chat_turn`). One worker per user, alive while an assistant turn is producing. For each upstream delta:

  - Appends to `chat_turns.content` / `chat_turns.reasoning` (SQLite `||` concatenation, idempotent on repeat).
  - Inserts running rows in `chat_tool_calls`, flips them to completed when the tool returns.
  - Broadcasts a single `TurnUpdate::Tick` on the per-worker channel after every DB write.
  - On exit: `finalize_turn` writes the final `status` + `completed_at`, `touch_session` bumps the sidebar order, broadcasts `TurnUpdate::Finalized`.

The worker doesn't care if anyone's listening — it runs to completion either way. The DB is the source of truth; nothing flows through the broadcast except "go re-read the row."

**Worker registry** (`rama_server::chat_workers::ChatWorkers`). User-id → `ActiveWorker { turn_id, session_id, cancel: AtomicBool, broadcast }`. `register()` refuses if there's already a worker for this user (concurrent submits get a clean 409 toast, not a race-y duplicate-stream); `cancel()` flips the flag; `get()` is how the tail handler attaches; `clear()` removes the entry when the worker exits.

**The streaming flow**:

1. **`POST /chat/{id}/messages`** validates the form, persists the user turn + an assistant turn in `in_progress`, calls `ChatWorkers::register` (refuses with a toast if busy), spawns `run_chat_turn`, and SSE-tails the broadcast. Initial event is `mode append` of both fresh bubbles onto `#conversation`. Each `Tick` triggers a re-read of the assistant turn from the DB plus an `mode outer` patch on `#turn-<uuid>` with the current render. `Finalized` emits one last patch plus a `datastar-patch-signals` flipping `$chatStreaming` to false.

2. **`GET /chat/{id}/tail`** is the reconnect path. Looks up the user's active worker; if it belongs to this session, subscribes to the same broadcast and runs the same re-read-and-patch loop without the initial bubble-append (the bubbles are already on the page from the original `GET /chat/{id}` render). If there's no live worker the response sends `chatStreaming=false` and closes — defensive against a stale tab that's optimistically set the flag.

3. **`POST /chat/{id}/cancel`** flips the cancel flag. The worker observes between upstream chunks and exits cleanly into finalize.

`GET /chat/{id}` always reads from the DB. If there's an in-flight assistant turn, the conversation `<section>` emits `data-init="window.chatScroll.init(el); @get('/chat/{id}/tail')"` so the page auto-subscribes to the live worker on mount. Datastar re-fires `data-init` on every nav-patch, so a user backgrounding their phone and unlocking it half a minute later still picks up the live stream.

**Why this shape**: the previous chat handler accumulated all turn state in memory (the channel was the only place the response lived). A datastar retry after a connection abort would race a brand-new worker against the still-finishing previous one, producing two cross-written assistant bubbles. With the DB as the source of truth and an explicit one-worker-per-user registry, retries are idempotent (the tail endpoint attaches to the same worker rather than spawning a fresh one) and concurrent submits get a clear "still streaming" toast.

`ui/ts/chat/composer.ts` no longer collects history client-side — the server reconstructs the upstream message list from `chat_turns`. The composer just validates non-empty, flips `$chatStreaming`, and clears the textarea once the server's initial SSE event lands.

## TypeScript glue

Anything genuinely interactive that doesn't fit in a `data-on:*` expression lives in TypeScript under `ui/ts/`. esbuild bundles each entry into the same `crates/gateway/assets/*.js` paths the server's `include_bytes!` already pointed at. `tsc --strict` runs as a separate type-check step (esbuild strips types without checking).

```
ui/
├── tsconfig.json
├── package.json             # esbuild + typescript + tailwindcss + daisyui
├── src/
│   └── main.css             # Tailwind/daisyUI entry, builds to assets/app.css
└── ts/
    ├── app.ts               # entry — toasts + timezone + popstate, imports below
    ├── global.d.ts          # window.* augmentations
    ├── clipboard.ts         # window.uiCopy
    ├── chat/
    │   ├── composer.ts      # window.chatComposer (Enter / submit / history)
    │   ├── mic.ts           # window.chatMic   (AudioWorklet → WAV → /transcriptions)
    │   └── scroll.ts        # window.chatScroll (autoscroll observer)
    └── pcm-recorder.ts      # AudioWorklet processor, separate bundle
```

### Per-feature module pattern

Each module owns one interactive surface and exposes a minimal entry point on `window.*`. The server-rendered HTML calls it via `data-on:*` / `data-init`. Nothing reaches across modules through DOM IDs — modules talk to each other via `window.*` calls (e.g. the scroll observer pings `window.chatComposer.notifyConversationMutated()` each mutation).

```ts
// ui/ts/clipboard.ts
const uiCopy = async (btn: HTMLElement): Promise<void> => {
    const selector = btn.dataset.copyTarget;
    if (!selector) return;
    const target = document.querySelector(selector);
    if (!target) return;
    try {
        await navigator.clipboard.writeText(target.textContent ?? '');
    } catch (err) {
        window.pushToast('error', `Couldn't copy: ${err}`);
        return;
    }
    window.pushToast('success', 'Copied to clipboard.');
};
window.uiCopy = uiCopy;
```

```rust
// pages/tokens.rs — minted-banner copy button
button(
    type: "button",
    "data-copy-target": "#minted-token-value",
    "data-on:click": "window.uiCopy(el)",
    …
) { (icons::copy(16)) }
```

The `window.*` surface is declared in `ui/ts/global.d.ts` (`interface Window { uiCopy(btn: HTMLElement): Promise<void>; chatComposer: { … }; chatMic: { … }; chatScroll: { … }; }`) so call sites stay type-checked.

### When to reach for what

| Need | Use |
|---|---|
| Toggle a class off reactive state | `data-signals` + `data-class` (no JS) |
| Run a server action on click/submit | `data-on:click="@post('/url')"` or `data-on:submit__prevent="@post('/url', {contentType: 'form'})"` |
| Local computation / state read on event | Inline expression in `data-on:*` — read `$signal`, write `$signal = expr`, call `evt.preventDefault()` etc. |
| Multi-step work (FormData upload, AudioWorklet, walking DOM) | TS module exposing a `window.<feature>.<fn>(el, evt)` call site, invoked from `data-on:*` |
| Wire up element-bound state on mount (and re-mount after nav patch) | `data-init="window.<feature>.init(el)"`. Datastar fires `data-init` on every mount; the module's `init` is responsible for being idempotent. |
| Server-driven state transition (no DOM change) | `sse_signals(json)` from the handler; client signals re-evaluate. Prefer over `sse_script` for state flips. |
| Server-driven DOM change | `sse_patch(selector, mode, html)`. |

### Anti-patterns

- `document.addEventListener('click', e => { const btn = e.target.closest('[data-foo]'); if (!btn) return; … })` — moved to `data-on:click="window.foo(el)"` per-element. The id/closest filter pattern was a workaround for elements being re-rendered by SSE patches; datastar's per-element attrs survive that natively.
- `window.__appBootstrap` — gone. The previous workaround re-bound MutationObservers after each nav patch from a single global function. Now each element-bound observer is set up via `data-init` on its target, which datastar re-fires automatically.
- `sse_script("document.getElementById('foo').classList.add('bar')")` — for state, prefer a signal + `sse_signals(…)`. Reserve `sse_script` for things datastar can't express (notably `form.reset()`).

## Browser debugging — `mise run dev-ui`

The chat / tokens / dashboard pages are all gated by OIDC, which makes ad-hoc browser debugging annoying. **Don't fabricate hand-rolled `test.html`** — they can't initialise datastar correctly and miss real bugs.

```bash
mise run dev-ui
```

Boots the full rama gateway on `127.0.0.1:8080` against an in-memory SQLite, a wiremock chat + transcription backend, and a pre-seeded session. Prints the signed cookie on startup; paste it via `document.cookie` after a `goto`, then drive any authed page with playwright. Full recipe in [`docs/dev-workflow.md`](dev-workflow.md#debugging-the-ui).

## Building the assets

```bash
mise run dev          # build-assets (css + js) then cargo run
mise run build        # release: build-assets then cargo build --release

mise run watch-css    # live rebuild of the CSS bundle
mise run watch-js     # live rebuild of the JS bundles
mise run build-css    # one-shot CSS (runs in CI before cargo)
mise run build-js     # one-shot JS  (runs in CI before cargo)
mise run typecheck    # tsc --noEmit (runs as part of `mise run lint`)
mise run build-assets # css + js composite (the dep every Rust task pulls)
```

`ui/package.json` pins Tailwind v4 + daisyUI v5 + esbuild + typescript. `ui/src/main.css` registers the shadcn-flavoured `light` / `dark` themes and `@source`'s the rama_server `.rs` files so Tailwind picks up class names from the rendered HTML strings (since the templates live inside Rust string literals, the scanner needs to be told where to look). `ui/tsconfig.json` is strict-mode + bundler-resolution; esbuild writes sourcemaps next to each bundle (gitignored — bundles themselves are committed so `cargo build` in CI doesn't strictly need node).
