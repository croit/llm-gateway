// Main entry for the gateway's client-side glue.
//
// Most behaviour lives in `data-*` attributes on the server-rendered
// HTML — Datastar evaluates `data-on:*` expressions in-place, watches
// `data-signals` for reactive state, and toggles `data-class` against
// signals. This file owns only what's genuinely page-global:
//   - Toast auto-dismiss for the `#toasts` region (outside <main>,
//     so its observer stays bound across nav patches).
//   - A one-shot timezone POST.
//   - `popstate` → `location.reload()` so browser back/forward at
//     least restores the right page.
//
// Per-feature wiring (chat composer, mic, autoscroll, copy) lives in
// its own sibling module. Each registers a small surface on `window`
// that the server-rendered HTML hits via `data-on:*` / `data-init`
// attributes. The imports below are side-effect-only — bundling them
// in means esbuild concatenates everything into the one
// `assets/app.js` the page loads.

// `composer` + `scroll` are session-core's reusable chat-form
// helpers (they attach `window.chatComposer` and `window.chatScroll`
// via top-level side-effects).
import '../../crates/session-core/ui/ts/composer.js';
import '../../crates/session-core/ui/ts/scroll.js';
import './chat/mic.js';
import './chat/actions.js';
import './clipboard.js';
import './canvas.js';
import './geo.js';
import './feedback.js';
import { initFeedbackCapture } from './feedback-capture.js';

// Start console + network capture as early as possible so a feedback report
// carries the diagnostics that led up to it. Cheap, bounded, best-effort.
initFeedbackCapture();

// ---- Toasts -----------------------------------------------------------
//
// Server-rendered structure: `<div id="toasts" class="toast ...">` is
// daisyUI's positioning container; each notification inside is an
// `alert alert-{success,error,info}` carrying a `.toast-item` marker
// so this script can find + auto-dismiss them without snagging on
// the container itself (which also matches `.toast`).
//
// The container lives outside `<main>`, so its observer stays bound
// across nav patches.
const toasts = document.getElementById('toasts');
if (toasts) {
    const arm = (el: Element): void => {
        window.setTimeout(() => el.remove(), 5200);
    };
    toasts.querySelectorAll('.toast-item').forEach(arm);
    new MutationObserver((muts) => {
        for (const m of muts) {
            for (const n of Array.from(m.addedNodes)) {
                if (n instanceof Element && n.classList.contains('toast-item')) arm(n);
            }
        }
    }).observe(toasts, { childList: true });
}

// Surface a transient toast from any script on the page. Matches the
// server-rendered structure in `render_toast` (pages/mod.rs): a neutral
// base-100 card with a 4px left border tinted to the kind's status
// colour. Stays quiet visually — shadcn-style.
window.pushToast = (kind, message) => {
    if (!toasts) return;
    const div = document.createElement('div');
    div.setAttribute('role', 'status');
    const borderMap: Record<ToastKind, string> = {
        success: 'border-l-success',
        error: 'border-l-error',
        info: 'border-l-info',
    };
    const accent = borderMap[kind] ?? borderMap.info;
    div.className =
        'toast-item pointer-events-auto bg-base-100 text-base-content ' +
        'border border-base-300 border-l-4 ' + accent + ' ' +
        'rounded-lg shadow-md px-3 py-2 text-sm max-w-sm';
    div.textContent = message;
    toasts.appendChild(div);
};

// ---- Timezone post ---------------------------------------------------
//
// Tell the gateway what timezone this browser is in, exactly once per
// browser session. The server stores it on the session row + user
// row; tools like `get_current_timestamp` query the user row to
// format wall-clock times in the user's locale instead of UTC.
// Fire-and-forget — a failed POST means tools fall back to UTC, not a
// broken page. `sessionStorage` keys it so we don't re-post on every
// nav patch.
(() => {
    try {
        if (sessionStorage.getItem('tz-posted') === '1') return;
        const tz = Intl.DateTimeFormat().resolvedOptions().timeZone;
        if (!tz) return;
        fetch('/api/v0/me/timezone', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ timezone: tz }),
        }).then((r) => {
            if (r.ok) sessionStorage.setItem('tz-posted', '1');
        }).catch(() => {});
    } catch (_) { /* sessionStorage unavailable / private mode */ }
})();

// ---- SPA-style nav back/forward -------------------------------------
//
// `history.pushState` from the server-driven nav doesn't fire its own
// page load on back/forward — the URL changes but the DOM wouldn't
// re-render. `location.reload()` is the simple, robust recovery: the
// URL is correct, the full page renders, no stale bindings hang
// around. A future iteration could re-issue the datastar @get for
// the popped URL instead and avoid the reload.
window.addEventListener('popstate', () => {
    location.reload();
});
