// Document-canvas splitter.
//
// The canvas docks as a right-hand column (`#document-canvas-slot`) next to
// the chat, with a draggable handle (`#canvas-splitter`) between them. This
// module makes the handle resize the column and remembers the width per
// browser.
//
// The width lives as a CSS custom property `--canvas-width` on <html> (read by
// `.canvas-col` in the stylesheet). Keeping it on <html> — not on the shell —
// means it survives datastar nav morphs that re-render <main>. Drag is bound
// once via delegation on `document` for the same reason: the splitter element
// is re-created on navigation, so we never hold a stale reference.
//
// Desktop only in effect: on narrow screens the splitter is `display:none` and
// the canvas is a full overlay, so the handler simply never fires.

const KEY = 'gw.canvasWidth';
const MIN = 320;
const MAX = 760;

function applyWidth(px: number): void {
    document.documentElement.style.setProperty('--canvas-width', `${px}px`);
}

// Restore a remembered width as early as possible (before first paint of the
// canvas) so there's no visible snap from the 420px default.
(() => {
    try {
        const saved = Number(localStorage.getItem(KEY));
        if (Number.isFinite(saved) && saved >= MIN && saved <= MAX) applyWidth(saved);
    } catch {
        /* localStorage unavailable (private mode) — fall back to the CSS default */
    }
})();

// Mouse events (not pointer events): they fire for every mouse drag in all
// browsers and are what synthetic automation drags dispatch too. The splitter
// is desktop-only (hidden on touch), so we don't need touch/pointer handling.
document.addEventListener('mousedown', (e) => {
    const target = e.target as Element | null;
    if (!target || !target.closest('#canvas-splitter')) return;
    const shell = document.querySelector('.chat-shell') as HTMLElement | null;
    if (!shell) return;

    e.preventDefault();
    document.body.style.userSelect = 'none';
    document.body.style.cursor = 'col-resize';

    const onMove = (ev: MouseEvent): void => {
        // The canvas hugs the shell's right edge, so its width is the distance
        // from the cursor to that edge. Clamp to sane bounds.
        const right = shell.getBoundingClientRect().right;
        const w = Math.max(MIN, Math.min(MAX, right - ev.clientX));
        applyWidth(w);
    };
    const onUp = (): void => {
        document.body.style.userSelect = '';
        document.body.style.cursor = '';
        document.removeEventListener('mousemove', onMove);
        document.removeEventListener('mouseup', onUp);
        try {
            const cur = getComputedStyle(document.documentElement)
                .getPropertyValue('--canvas-width')
                .trim();
            const px = parseInt(cur, 10);
            if (Number.isFinite(px)) localStorage.setItem(KEY, String(px));
        } catch {
            /* ignore persistence failures */
        }
    };
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
});
